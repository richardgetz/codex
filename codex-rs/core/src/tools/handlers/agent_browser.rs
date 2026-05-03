use std::collections::HashMap;
use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::sleep;
use uuid::Uuid;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::agent_browser_cdp::CdpClient;
use crate::tools::handlers::agent_browser_visual;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use url::Url;

const TOOL_OPEN: &str = "open";
const TOOL_CLOSE: &str = "close";
const TOOL_NAVIGATE: &str = "navigate";
const TOOL_SNAPSHOT: &str = "snapshot";
const TOOL_SCREENSHOT: &str = "screenshot";
const TOOL_CLICK: &str = "click";
const TOOL_TYPE: &str = "type";
const TOOL_PRESS: &str = "press";
const TOOL_SCROLL: &str = "scroll";
const TOOL_SELECTION: &str = "selection_overview";
const TOOL_HIGHLIGHT: &str = "highlight";
const TOOL_SHARE: &str = "share";
const TOOL_BENCHMARK: &str = "benchmark";

const DEFAULT_VIEWPORT_WIDTH: u32 = 1365;
const DEFAULT_VIEWPORT_HEIGHT: u32 = 900;
const DEFAULT_LOCALE: &str = "en-US";
const DEFAULT_TIMEZONE: &str = "America/New_York";
const BROWSER_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SCROLL_DELTA: f64 = 10_000.0;

static BROWSER_MANAGER: OnceLock<Mutex<BrowserManager>> = OnceLock::new();
static BROWSER_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

pub struct AgentBrowserHandler;

impl ToolHandler for AgentBrowserHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn is_mutating(&self, _invocation: &ToolInvocation) -> bool {
        true
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let arguments = match &invocation.payload {
            ToolPayload::Function { arguments } => arguments.clone(),
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "agent_browser handler received unsupported payload".to_string(),
                ));
            }
        };

        let access_policy = BrowserAccessPolicy::from_invocation(&invocation);
        match invocation.tool_name.name.as_str() {
            TOOL_OPEN => {
                let args = parse_arguments(&arguments)?;
                access_policy.validate_open(&args)?;
                text_output(handle_open(args).await?)
            }
            TOOL_CLOSE => text_output(handle_close(parse_arguments(&arguments)?).await?),
            TOOL_NAVIGATE => {
                let args: NavigateArgs = parse_arguments(&arguments)?;
                access_policy.validate_page_url(&args.url, "navigate")?;
                text_output(handle_navigate(args).await?)
            }
            TOOL_SNAPSHOT => text_output(handle_snapshot(parse_arguments(&arguments)?).await?),
            TOOL_SCREENSHOT => handle_screenshot(parse_arguments(&arguments)?).await,
            TOOL_CLICK => text_output(handle_click(parse_arguments(&arguments)?).await?),
            TOOL_TYPE => text_output(handle_type(parse_arguments(&arguments)?).await?),
            TOOL_PRESS => text_output(handle_press(parse_arguments(&arguments)?).await?),
            TOOL_SCROLL => text_output(handle_scroll(parse_arguments(&arguments)?).await?),
            TOOL_SELECTION => text_output(handle_selection(parse_arguments(&arguments)?).await?),
            TOOL_HIGHLIGHT => text_output(handle_highlight(parse_arguments(&arguments)?).await?),
            TOOL_SHARE => text_output(handle_share(parse_arguments(&arguments)?).await?),
            TOOL_BENCHMARK => {
                let args: BenchmarkArgs = parse_arguments(&arguments)?;
                access_policy.validate_browser_process("benchmark")?;
                access_policy.validate_page_url(
                    args.url.as_deref().unwrap_or(BENCHMARK_DATA_URL_SENTINEL),
                    /*action*/ "benchmark",
                )?;
                text_output(handle_benchmark(args).await?)
            }
            other => Err(FunctionCallError::RespondToModel(format!(
                "unknown agent_browser tool `{other}`"
            ))),
        }
    }
}

const BENCHMARK_DATA_URL_SENTINEL: &str = "data:text/html;charset=utf-8,";

struct BrowserAccessPolicy {
    network: NetworkSandboxPolicy,
    file_system: FileSystemSandboxPolicy,
    cwd: PathBuf,
}

impl BrowserAccessPolicy {
    fn from_invocation(invocation: &ToolInvocation) -> Self {
        Self {
            network: invocation.turn.network_sandbox_policy(),
            file_system: invocation.turn.file_system_sandbox_policy(),
            cwd: invocation.turn.cwd.to_path_buf(),
        }
    }

    fn validate_open(&self, args: &OpenArgs) -> Result<(), FunctionCallError> {
        if args.share_id.is_none() {
            self.validate_browser_process("open")?;
        }
        if let Some(remote_debugging_url) = args.remote_debugging_url.as_deref() {
            self.validate_network_endpoint(
                remote_debugging_url,
                /*field*/ "remote_debugging_url",
            )?;
        }
        if let Some(url) = args.url.as_deref() {
            self.validate_page_url(url, "open")?;
        }
        Ok(())
    }

    fn validate_browser_process(&self, action: &str) -> Result<(), FunctionCallError> {
        if !self.network.is_enabled() {
            return Err(FunctionCallError::RespondToModel(format!(
                "agent_browser.{action} requires network-enabled permissions because browser processes are outside the command sandbox"
            )));
        }
        Ok(())
    }

    fn validate_network_endpoint(
        &self,
        raw_url: &str,
        field: &str,
    ) -> Result<(), FunctionCallError> {
        let url = parse_browser_url(raw_url, field)?;
        match url.scheme() {
            "http" | "https" | "ws" | "wss" => self.validate_network(/*action*/ field),
            other => Err(FunctionCallError::RespondToModel(format!(
                "agent_browser `{field}` does not support `{other}:` URLs"
            ))),
        }
    }

    fn validate_page_url(&self, raw_url: &str, action: &str) -> Result<(), FunctionCallError> {
        let url = parse_browser_url(raw_url, action)?;
        match url.scheme() {
            "about" | "data" => Ok(()),
            "file" => {
                let path = url.to_file_path().map_err(|_| {
                    FunctionCallError::RespondToModel(format!(
                        "agent_browser.{action} could not resolve file URL `{raw_url}`"
                    ))
                })?;
                if self
                    .file_system
                    .can_read_path_with_cwd(&path, /*cwd*/ &self.cwd)
                {
                    Ok(())
                } else {
                    Err(FunctionCallError::RespondToModel(format!(
                        "agent_browser.{action} cannot read `{}` under the current filesystem permissions",
                        path.display()
                    )))
                }
            }
            "http" | "https" | "ws" | "wss" => self.validate_network(action),
            other => Err(FunctionCallError::RespondToModel(format!(
                "agent_browser.{action} does not support `{other}:` URLs"
            ))),
        }
    }

    fn validate_network(&self, action: &str) -> Result<(), FunctionCallError> {
        if self.network.is_enabled() {
            return Ok(());
        }
        Err(FunctionCallError::RespondToModel(format!(
            "agent_browser.{action} cannot use network URLs while network access is restricted"
        )))
    }
}

fn parse_browser_url(raw_url: &str, field: &str) -> Result<Url, FunctionCallError> {
    Url::parse(raw_url).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "agent_browser `{field}` requires an absolute URL: {err}"
        ))
    })
}

fn text_output<T: Serialize>(value: T) -> Result<FunctionToolOutput, FunctionCallError> {
    let text = serde_json::to_string(&value).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to serialize agent_browser result: {err}"
        ))
    })?;
    Ok(FunctionToolOutput::from_text(text, Some(true)))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BrowserMode {
    #[default]
    Headful,
    Headless,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BrowserBackend {
    #[default]
    Auto,
    Obscura,
    Chromium,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BrowserEngine {
    Chromium,
    Obscura,
    ExternalCdp,
}

#[derive(Debug, Deserialize)]
struct OpenArgs {
    url: Option<String>,
    share_id: Option<String>,
    #[serde(default)]
    mode: BrowserMode,
    #[serde(default = "default_true")]
    stealth: bool,
    #[serde(default)]
    backend: BrowserBackend,
    viewport_width: Option<u32>,
    viewport_height: Option<u32>,
    locale: Option<String>,
    timezone: Option<String>,
    user_agent: Option<String>,
    remote_debugging_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionArgs {
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NavigateArgs {
    session_id: Option<String>,
    url: String,
}

#[derive(Debug, Deserialize)]
struct SnapshotArgs {
    session_id: Option<String>,
    max_text_chars: Option<usize>,
    max_elements: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ScreenshotArgs {
    session_id: Option<String>,
    full_page: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ClickArgs {
    session_id: Option<String>,
    #[serde(rename = "ref")]
    element_ref: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct TypeArgs {
    session_id: Option<String>,
    #[serde(rename = "ref")]
    element_ref: Option<String>,
    text: String,
    clear: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PressArgs {
    session_id: Option<String>,
    key: String,
}

#[derive(Debug, Deserialize)]
struct ScrollArgs {
    session_id: Option<String>,
    delta_x: Option<f64>,
    delta_y: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct SelectionArgs {
    session_id: Option<String>,
    enable_overlay: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct HighlightArgs {
    session_id: Option<String>,
    #[serde(rename = "ref")]
    element_ref: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
    label: Option<String>,
    color: Option<String>,
    clear: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ShareAccess {
    #[default]
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Deserialize)]
struct ShareArgs {
    session_id: Option<String>,
    #[serde(default)]
    access: ShareAccess,
}

#[derive(Debug, Deserialize)]
struct BenchmarkArgs {
    #[serde(default = "default_headless")]
    mode: BrowserMode,
    #[serde(default)]
    backend: BrowserBackend,
    url: Option<String>,
    iterations: Option<usize>,
    #[serde(default = "default_true")]
    stealth: bool,
    remote_debugging_url: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_headless() -> BrowserMode {
    BrowserMode::Headless
}

fn elapsed_ms(started: Instant) -> f64 {
    round_ms(started.elapsed().as_secs_f64() * 1_000.0)
}

fn round_ms(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn bounded_number(name: &str, value: f64, min: f64, max: f64) -> Result<f64, FunctionCallError> {
    if !value.is_finite() || value < min || value > max {
        return Err(FunctionCallError::RespondToModel(format!(
            "`{name}` must be a finite number from {min} to {max}"
        )));
    }
    Ok(value)
}

fn required_bounded_number(
    value: Option<f64>,
    name: &str,
    min: f64,
    max: f64,
    missing_message: &'static str,
) -> Result<f64, FunctionCallError> {
    let value =
        value.ok_or_else(|| FunctionCallError::RespondToModel(missing_message.to_string()))?;
    bounded_number(name, value, min, max)
}

fn positive_bounded_number(name: &str, value: f64, max: f64) -> Result<f64, FunctionCallError> {
    if !value.is_finite() || value <= 0.0 || value > max {
        return Err(FunctionCallError::RespondToModel(format!(
            "`{name}` must be a positive finite number up to {max}"
        )));
    }
    Ok(value)
}

fn remaining_viewport_extent(
    name: &str,
    viewport: u32,
    origin: f64,
) -> Result<f64, FunctionCallError> {
    let remaining = f64::from(viewport) - origin;
    if remaining <= 0.0 {
        return Err(FunctionCallError::RespondToModel(format!(
            "`{name}` origin is outside the viewport"
        )));
    }
    Ok(remaining)
}

#[derive(Debug, Serialize)]
struct OpenResult {
    session_id: String,
    mode: &'static str,
    backend: BrowserEngine,
    stealth: bool,
    endpoint: String,
    url: String,
    launch_ms: f64,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SimpleResult {
    ok: bool,
    session_id: String,
    message: String,
    elapsed_ms: Option<f64>,
}

#[derive(Debug, Serialize)]
struct ShareResult {
    ok: bool,
    session_id: String,
    share_id: String,
    access: ShareAccess,
    backend: BrowserEngine,
    mode: &'static str,
    endpoint: String,
    remote_debugging_url: String,
    share_file: String,
    url: String,
    notes: Vec<String>,
    elapsed_ms: f64,
}

#[derive(Debug, Serialize)]
struct BenchmarkResult {
    mode: &'static str,
    backend: BrowserEngine,
    stealth: bool,
    target_url: String,
    iterations: usize,
    launch_ms: f64,
    navigate_ms: f64,
    snapshot_ms: Vec<f64>,
    screenshot_ms: Vec<f64>,
    screenshot_png_bytes: Vec<usize>,
    screenshot_base64_chars: Vec<usize>,
    totals: BenchmarkTotals,
}

#[derive(Debug, Serialize)]
struct BenchmarkTotals {
    snapshot_avg_ms: f64,
    screenshot_avg_ms: f64,
    screenshot_png_avg_bytes: f64,
    screenshot_base64_avg_chars: f64,
}

struct BrowserManager {
    sessions: HashMap<String, BrowserSession>,
    active_session_id: Option<String>,
}

impl BrowserManager {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            active_session_id: None,
        }
    }

    fn resolve_session_id(&self, requested: Option<String>) -> Result<String, FunctionCallError> {
        if let Some(session_id) = requested {
            if self.sessions.contains_key(&session_id) {
                return Ok(session_id);
            }
            return Err(FunctionCallError::RespondToModel(format!(
                "agent_browser session `{session_id}` was not found"
            )));
        }

        self.active_session_id.clone().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "no active agent_browser session; call agent_browser.open first".to_string(),
            )
        })
    }
}

struct BrowserSession {
    id: String,
    mode: BrowserMode,
    engine: BrowserEngine,
    stealth: bool,
    viewport_width: u32,
    viewport_height: u32,
    endpoint: String,
    page_ws_url: String,
    target_id: Option<String>,
    access: ShareAccess,
    cdp: CdpClient,
    process: Option<Child>,
    owned_page_close_url: Option<String>,
    _profile_dir: Option<TempDir>,
    visual_shell_path: Option<PathBuf>,
    overlay_script_registered: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct BrowserShare {
    share_id: String,
    access: ShareAccess,
    engine: BrowserEngine,
    mode: String,
    endpoint: String,
    page_ws_url: String,
    target_id: Option<String>,
    viewport_width: u32,
    viewport_height: u32,
    stealth: bool,
    created_at_unix_ms: u128,
}

struct PageInitOptions<'a> {
    engine: BrowserEngine,
    stealth: bool,
    viewport_width: u32,
    viewport_height: u32,
    locale: &'a str,
    timezone: &'a str,
    user_agent: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotRefMode {
    ActionRefs,
    ReadOnly,
}

async fn cdp_call_allowing(
    cdp: &mut CdpClient,
    method: &str,
    params: Value,
    allowed_message: &str,
) -> Result<(), FunctionCallError> {
    match cdp.call(method, params).await {
        Ok(_) => Ok(()),
        Err(FunctionCallError::RespondToModel(message)) if message.contains(allowed_message) => {
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn manager() -> &'static Mutex<BrowserManager> {
    BROWSER_MANAGER.get_or_init(|| Mutex::new(BrowserManager::new()))
}

async fn take_session(
    requested_session_id: Option<String>,
) -> Result<BrowserSession, FunctionCallError> {
    let mut manager = manager().lock().await;
    let session_id = manager.resolve_session_id(requested_session_id)?;
    manager.sessions.remove(&session_id).ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "agent_browser session `{session_id}` disappeared before use"
        ))
    })
}

async fn put_session(session: BrowserSession) {
    let mut manager = manager().lock().await;
    manager.sessions.insert(session.id.clone(), session);
}

async fn handle_open(args: OpenArgs) -> Result<OpenResult, FunctionCallError> {
    let started = Instant::now();
    let session_id = format!("br-{}", Uuid::new_v4().simple());
    let requested_viewport_width = args
        .viewport_width
        .unwrap_or(DEFAULT_VIEWPORT_WIDTH)
        .clamp(320, 7680);
    let requested_viewport_height = args
        .viewport_height
        .unwrap_or(DEFAULT_VIEWPORT_HEIGHT)
        .clamp(240, 4320);
    let locale = args.locale.unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let timezone = args
        .timezone
        .unwrap_or_else(|| DEFAULT_TIMEZONE.to_string());

    if args.share_id.is_some() && args.remote_debugging_url.is_some() {
        return Err(FunctionCallError::RespondToModel(
            "`share_id` and `remote_debugging_url` cannot both be set".to_string(),
        ));
    }

    let shared_attach = args.share_id.is_some();
    let mut launch = if let Some(share_id) = args.share_id.as_deref() {
        attach_shared_connection(share_id).await?
    } else if let Some(remote) = args.remote_debugging_url.as_deref() {
        attach_connection(remote).await?
    } else {
        launch_connection(
            &args.backend,
            &args.mode,
            args.stealth,
            requested_viewport_width,
            requested_viewport_height,
            &locale,
        )
        .await?
    };

    let endpoint = launch.endpoint.clone();
    let page_ws_url = launch.page_ws_url.clone();
    let mut notes = launch.notes.clone();
    let session_access = launch.access;
    let stealth = launch.stealth.unwrap_or(args.stealth);
    let viewport_width = launch.viewport_width.unwrap_or(requested_viewport_width);
    let viewport_height = launch.viewport_height.unwrap_or(requested_viewport_height);
    let session_mode = launch.mode.clone().unwrap_or_else(|| args.mode.clone());
    let output_mode = mode_name(&session_mode);

    if args.url.is_some() && session_access == ShareAccess::ReadOnly {
        cleanup_launch(&mut launch).await;
        return Err(FunctionCallError::RespondToModel(
            "`url` cannot be used when opening a read_only shared browser session".to_string(),
        ));
    }

    let mut cdp = match CdpClient::connect(&page_ws_url).await {
        Ok(cdp) => cdp,
        Err(err) => {
            cleanup_launch(&mut launch).await;
            return Err(err);
        }
    };
    if let Some(target_id) = launch.target_id.as_deref()
        && let Err(err) = attach_obscura_target(&mut cdp, target_id).await
    {
        cleanup_launch(&mut launch).await;
        return Err(err);
    }
    if !(shared_attach && session_access == ShareAccess::ReadOnly)
        && let Err(err) = initialize_page(
            &mut cdp,
            PageInitOptions {
                engine: launch.engine,
                stealth,
                viewport_width,
                viewport_height,
                locale: &locale,
                timezone: &timezone,
                user_agent: args.user_agent.as_deref(),
            },
        )
        .await
    {
        cleanup_launch(&mut launch).await;
        return Err(err);
    }

    if let Some(url) = args.url.as_deref()
        && let Err(err) = navigate_to(&mut cdp, url).await
    {
        cleanup_launch(&mut launch).await;
        return Err(err);
    }

    let url = page_url(&mut cdp).await.unwrap_or_default();
    let visual_shell_path = if launch.engine == BrowserEngine::Obscura
        && matches!(session_mode, BrowserMode::Headful)
    {
        match launch.profile_dir.as_ref() {
            Some(profile_dir) => {
                let path = profile_dir.path().join("codex-obscura-headful.html");
                match refresh_obscura_visual_shell(&mut cdp, &path, viewport_width, viewport_height)
                    .await
                {
                    Ok(()) => {
                        notes.push(format!("Obscura headful mirror: {}", path.display()));
                        if agent_browser_visual::open_visual_shell(&path) {
                            notes.push("opened Obscura headful mirror".to_string());
                        }
                        Some(path)
                    }
                    Err(err) => {
                        notes.push(format!("Obscura headful mirror unavailable: {err}"));
                        None
                    }
                }
            }
            None => None,
        }
    } else {
        None
    };
    let session = BrowserSession {
        id: session_id.clone(),
        mode: session_mode,
        engine: launch.engine,
        stealth,
        viewport_width,
        viewport_height,
        endpoint: endpoint.clone(),
        page_ws_url,
        target_id: cdp.target_id().map(str::to_string),
        access: session_access,
        cdp,
        process: launch.process.take(),
        owned_page_close_url: launch.owned_page_close_url.take(),
        _profile_dir: launch.profile_dir.take(),
        visual_shell_path,
        overlay_script_registered: false,
    };

    let mut manager = manager().lock().await;
    manager.active_session_id = Some(session_id.clone());
    manager.sessions.insert(session_id.clone(), session);

    Ok(OpenResult {
        session_id,
        mode: output_mode,
        backend: launch.engine,
        stealth,
        endpoint: endpoint.clone(),
        url,
        launch_ms: elapsed_ms(started),
        notes,
    })
}

async fn handle_close(args: SessionArgs) -> Result<SimpleResult, FunctionCallError> {
    let (session_id, mut session) = {
        let mut manager = manager().lock().await;
        let session_id = manager.resolve_session_id(args.session_id)?;
        let session = manager.sessions.remove(&session_id).ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "agent_browser session `{session_id}` disappeared before close"
            ))
        })?;
        if manager.active_session_id.as_deref() == Some(session_id.as_str()) {
            manager.active_session_id = manager.sessions.keys().next().cloned();
        }
        (session_id, session)
    };

    if let Some(mut child) = session.process.take() {
        let _ = child.kill().await;
    } else if let Some(close_url) = session.owned_page_close_url.take() {
        let _ = browser_http_client().get(close_url).send().await;
    }

    Ok(SimpleResult {
        ok: true,
        session_id,
        message: "closed".to_string(),
        elapsed_ms: None,
    })
}

async fn handle_navigate(args: NavigateArgs) -> Result<SimpleResult, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let session_id = session.id.clone();
    if let Err(err) = ensure_write_access(&session, TOOL_NAVIGATE) {
        put_session(session).await;
        return Err(err);
    }
    let result = match navigate_to(&mut session.cdp, &args.url).await {
        Ok(()) => {
            if let Some(path) = session.visual_shell_path.clone() {
                let _ = refresh_obscura_visual_shell(
                    &mut session.cdp,
                    &path,
                    session.viewport_width,
                    session.viewport_height,
                )
                .await;
            }
            Ok(SimpleResult {
                ok: true,
                session_id,
                message: "navigated".to_string(),
                elapsed_ms: Some(elapsed_ms(started)),
            })
        }
        Err(err) => Err(err),
    };
    put_session(session).await;
    result
}

async fn handle_snapshot(args: SnapshotArgs) -> Result<Value, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let ref_mode = snapshot_ref_mode(&session);
    let result = snapshot_page(
        &mut session.cdp,
        args.max_text_chars
            .unwrap_or(12_000)
            .clamp(/*min*/ 1_000, /*max*/ 80_000),
        args.max_elements
            .unwrap_or(80)
            .clamp(/*min*/ 1, /*max*/ 250),
        ref_mode,
    )
    .await
    .map(|mut snapshot| {
        snapshot["session_id"] = Value::String(session.id.clone());
        snapshot["mode"] = Value::String(mode_name(&session.mode).to_string());
        snapshot["backend"] = json!(session.engine);
        snapshot["stealth"] = Value::Bool(session.stealth);
        snapshot["elapsed_ms"] = json!(elapsed_ms(started));
        snapshot
    });
    put_session(session).await;
    result
}

async fn handle_screenshot(args: ScreenshotArgs) -> Result<FunctionToolOutput, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let session_id = session.id.clone();
    if session.engine == BrowserEngine::Obscura {
        let result = obscura_snapshot_screenshot(
            &mut session,
            args.full_page.unwrap_or(/*default*/ false),
            started,
        )
        .await;
        put_session(session).await;
        return result;
    }
    let result = session
        .cdp
        .call(
            "Page.captureScreenshot",
            json!({
                "format": "png",
                "fromSurface": true,
                "captureBeyondViewport": args.full_page.unwrap_or(/*default*/ false),
            }),
        )
        .await
        .and_then(|result| {
            let data = result.get("data").and_then(Value::as_str).ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "browser screenshot result did not include image data".to_string(),
                )
            })?;
            let summary = json!({
                "session_id": session_id,
                "mode": mode_name(&session.mode),
                "backend": session.engine,
                "stealth": session.stealth,
                "elapsed_ms": elapsed_ms(started),
                "mime_type": "image/png",
            });
            Ok(FunctionToolOutput::from_content(
                vec![
                    FunctionCallOutputContentItem::InputText {
                        text: serde_json::to_string(&summary)
                            .unwrap_or_else(|_| summary.to_string()),
                    },
                    FunctionCallOutputContentItem::InputImage {
                        image_url: format!("data:image/png;base64,{data}"),
                        detail: Some(DEFAULT_IMAGE_DETAIL),
                    },
                ],
                /*success*/ Some(true),
            ))
        });
    put_session(session).await;
    result
}

async fn obscura_snapshot_screenshot(
    session: &mut BrowserSession,
    full_page: bool,
    started: Instant,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let max_text_chars = if full_page { 24_000 } else { 12_000 };
    let ref_mode = snapshot_ref_mode(session);
    let snapshot = snapshot_page(
        &mut session.cdp,
        max_text_chars,
        /*max_elements*/ 120,
        ref_mode,
    )
    .await?;
    if let Some(path) = session.visual_shell_path.clone() {
        let _ = agent_browser_visual::write_visual_shell(
            &path,
            &snapshot,
            session.viewport_width,
            session.viewport_height,
        );
    }
    let png = agent_browser_visual::render_snapshot_png(
        &snapshot,
        session.viewport_width,
        session.viewport_height,
        full_page,
    )?;
    let summary = json!({
        "session_id": session.id,
        "mode": mode_name(&session.mode),
        "backend": session.engine,
        "stealth": session.stealth,
        "elapsed_ms": elapsed_ms(started),
        "mime_type": "image/png",
        "visual_source": "obscura_dom_snapshot",
        "note": "Obscura rendered this from the browser DOM/CDP snapshot; it is an agent review surface while native compositor screenshots are added.",
        "snapshot": snapshot,
    });
    Ok(FunctionToolOutput::from_content(
        vec![
            FunctionCallOutputContentItem::InputText {
                text: serde_json::to_string(&summary).unwrap_or_else(|_| summary.to_string()),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: format!("data:image/png;base64,{}", BASE64_STANDARD.encode(png)),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ],
        /*success*/ Some(true),
    ))
}

async fn handle_click(args: ClickArgs) -> Result<SimpleResult, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let session_id = session.id.clone();
    if let Err(err) = ensure_write_access(&session, TOOL_CLICK) {
        put_session(session).await;
        return Err(err);
    }

    let result = if let Some(element_ref) = args.element_ref.as_deref() {
        let expression = format!(
            r#"(() => {{
                const agentRef = {};
                const refs = window.__codexAgentBrowserRefElements;
                const el = refs && refs.get(agentRef);
                if (!el || !el.isConnected) return {{ ok: false, message: "element ref not found; call snapshot first" }};
                el.scrollIntoView({{ block: "center", inline: "center" }});
                const rect = el.getBoundingClientRect();
                if (rect.width <= 0 || rect.height <= 0) {{
                    return {{ ok: false, message: "element ref is not visible" }};
                }}
                return {{
                    ok: true,
                    x: rect.left + rect.width / 2,
                    y: rect.top + rect.height / 2
                }};
            }})()"#,
            serde_json::to_string(element_ref).unwrap_or_else(|_| "\"\"".to_string())
        );
        match evaluate_json(&mut session.cdp, &expression).await {
            Ok(result) if result.get("ok").and_then(Value::as_bool) == Some(true) => {
                let x = result.get("x").and_then(Value::as_f64).ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "click failed: element ref did not return x coordinate".to_string(),
                    )
                })?;
                let y = result.get("y").and_then(Value::as_f64).ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "click failed: element ref did not return y coordinate".to_string(),
                    )
                })?;
                let x =
                    bounded_number("x", x, /*min*/ 0.0, f64::from(session.viewport_width))?;
                let y =
                    bounded_number("y", y, /*min*/ 0.0, f64::from(session.viewport_height))?;
                dispatch_click(&mut session.cdp, x, y).await
            }
            Ok(result) => Err(FunctionCallError::RespondToModel(format!(
                "click failed: {}",
                result
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            ))),
            Err(err) => Err(err),
        }
    } else {
        let x = required_bounded_number(
            args.x,
            "x",
            /*min*/ 0.0,
            f64::from(session.viewport_width),
            "click requires either `ref` or both `x` and `y`",
        )?;
        let y = required_bounded_number(
            args.y,
            "y",
            /*min*/ 0.0,
            f64::from(session.viewport_height),
            "click requires either `ref` or both `x` and `y`",
        )?;
        dispatch_click(&mut session.cdp, x, y).await
    };
    let result = result.map(|_| SimpleResult {
        ok: true,
        session_id,
        message: "clicked".to_string(),
        elapsed_ms: Some(elapsed_ms(started)),
    });
    put_session(session).await;
    result
}

async fn dispatch_click(cdp: &mut CdpClient, x: f64, y: f64) -> Result<(), FunctionCallError> {
    cdp.call(
        "Input.dispatchMouseEvent",
        json!({"type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1}),
    )
    .await?;
    cdp.call(
        "Input.dispatchMouseEvent",
        json!({"type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1}),
    )
    .await
    .map(|_| ())
}

async fn handle_type(args: TypeArgs) -> Result<SimpleResult, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let session_id = session.id.clone();
    if let Err(err) = ensure_write_access(&session, TOOL_TYPE) {
        put_session(session).await;
        return Err(err);
    }
    let mut result = Ok(());
    if let Some(element_ref) = args.element_ref.as_deref()
        && result.is_ok()
    {
        let expression = format!(
            r#"(() => {{
                const agentRef = {};
                const refs = window.__codexAgentBrowserRefElements;
                const el = refs && refs.get(agentRef);
                if (!el || !el.isConnected) return {{ ok: false, message: "element ref not found; call snapshot first" }};
                el.scrollIntoView({{ block: "center", inline: "center" }});
                el.focus();
                if ({}) {{
                    if ("value" in el) el.value = "";
                    else el.textContent = "";
                    el.dispatchEvent(new Event("input", {{ bubbles: true }}));
                    el.dispatchEvent(new Event("change", {{ bubbles: true }}));
                }}
                return {{ ok: true }};
            }})()"#,
            serde_json::to_string(element_ref).unwrap_or_else(|_| "\"\"".to_string()),
            args.clear.unwrap_or(false)
        );
        result = evaluate_json(&mut session.cdp, &expression)
            .await
            .and_then(|value| {
                if value.get("ok").and_then(Value::as_bool) == Some(true) {
                    Ok(())
                } else {
                    Err(FunctionCallError::RespondToModel(format!(
                        "type failed: {}",
                        value
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                    )))
                }
            });
    } else if args.clear.unwrap_or(false) && result.is_ok() {
        result = evaluate_json(
            &mut session.cdp,
            r#"(() => {
                const el = document.activeElement;
                if (el && "value" in el) {
                    el.value = "";
                    el.dispatchEvent(new Event("input", { bubbles: true }));
                    el.dispatchEvent(new Event("change", { bubbles: true }));
                } else if (el) {
                    el.textContent = "";
                    el.dispatchEvent(new Event("input", { bubbles: true }));
                    el.dispatchEvent(new Event("change", { bubbles: true }));
                }
                return { ok: true };
            })()"#,
        )
        .await
        .map(|_| ());
    }

    if result.is_ok() {
        result = session
            .cdp
            .call("Input.insertText", json!({ "text": args.text }))
            .await
            .map(|_| ());
    }

    let result = result.map(|_| SimpleResult {
        ok: true,
        session_id,
        message: "typed".to_string(),
        elapsed_ms: Some(elapsed_ms(started)),
    });
    put_session(session).await;
    result
}

async fn handle_press(args: PressArgs) -> Result<SimpleResult, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let session_id = session.id.clone();
    if let Err(err) = ensure_write_access(&session, TOOL_PRESS) {
        put_session(session).await;
        return Err(err);
    }
    let result = dispatch_key(&mut session.cdp, &args.key)
        .await
        .map(|_| SimpleResult {
            ok: true,
            session_id,
            message: "pressed".to_string(),
            elapsed_ms: Some(elapsed_ms(started)),
        });
    put_session(session).await;
    result
}

struct KeyDescriptor {
    key: &'static str,
    code: &'static str,
    windows_virtual_key_code: u32,
}

fn key_descriptor(key: &str) -> Option<KeyDescriptor> {
    let normalized = key.trim().to_ascii_lowercase();
    let descriptor = match normalized.as_str() {
        "enter" | "return" => KeyDescriptor {
            key: "Enter",
            code: "Enter",
            windows_virtual_key_code: 13,
        },
        "tab" => KeyDescriptor {
            key: "Tab",
            code: "Tab",
            windows_virtual_key_code: 9,
        },
        "escape" | "esc" => KeyDescriptor {
            key: "Escape",
            code: "Escape",
            windows_virtual_key_code: 27,
        },
        "backspace" => KeyDescriptor {
            key: "Backspace",
            code: "Backspace",
            windows_virtual_key_code: 8,
        },
        "delete" | "del" => KeyDescriptor {
            key: "Delete",
            code: "Delete",
            windows_virtual_key_code: 46,
        },
        "space" | " " => KeyDescriptor {
            key: " ",
            code: "Space",
            windows_virtual_key_code: 32,
        },
        "arrowup" | "up" => KeyDescriptor {
            key: "ArrowUp",
            code: "ArrowUp",
            windows_virtual_key_code: 38,
        },
        "arrowdown" | "down" => KeyDescriptor {
            key: "ArrowDown",
            code: "ArrowDown",
            windows_virtual_key_code: 40,
        },
        "arrowleft" | "left" => KeyDescriptor {
            key: "ArrowLeft",
            code: "ArrowLeft",
            windows_virtual_key_code: 37,
        },
        "arrowright" | "right" => KeyDescriptor {
            key: "ArrowRight",
            code: "ArrowRight",
            windows_virtual_key_code: 39,
        },
        "home" => KeyDescriptor {
            key: "Home",
            code: "Home",
            windows_virtual_key_code: 36,
        },
        "end" => KeyDescriptor {
            key: "End",
            code: "End",
            windows_virtual_key_code: 35,
        },
        "pageup" => KeyDescriptor {
            key: "PageUp",
            code: "PageUp",
            windows_virtual_key_code: 33,
        },
        "pagedown" => KeyDescriptor {
            key: "PageDown",
            code: "PageDown",
            windows_virtual_key_code: 34,
        },
        _ => return None,
    };
    Some(descriptor)
}

async fn dispatch_key(cdp: &mut CdpClient, key: &str) -> Result<(), FunctionCallError> {
    let descriptor = key_descriptor(key).ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "unsupported key `{key}`; use Enter, Tab, Escape, Backspace, Delete, Space, ArrowUp, ArrowDown, ArrowLeft, ArrowRight, Home, End, PageUp, or PageDown"
        ))
    })?;
    let base = json!({
        "key": descriptor.key,
        "code": descriptor.code,
        "windowsVirtualKeyCode": descriptor.windows_virtual_key_code,
        "nativeVirtualKeyCode": descriptor.windows_virtual_key_code,
    });
    let mut key_down = base.clone();
    key_down["type"] = Value::String("rawKeyDown".to_string());
    cdp.call("Input.dispatchKeyEvent", key_down).await?;
    let mut key_up = base;
    key_up["type"] = Value::String("keyUp".to_string());
    cdp.call("Input.dispatchKeyEvent", key_up).await.map(|_| ())
}

async fn handle_scroll(args: ScrollArgs) -> Result<SimpleResult, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let session_id = session.id.clone();
    if let Err(err) = ensure_write_access(&session, TOOL_SCROLL) {
        put_session(session).await;
        return Err(err);
    }
    let delta_x = bounded_number(
        "delta_x",
        args.delta_x.unwrap_or(0.0),
        /*min*/ -MAX_SCROLL_DELTA,
        MAX_SCROLL_DELTA,
    )?;
    let delta_y = bounded_number(
        "delta_y",
        args.delta_y.unwrap_or(600.0),
        /*min*/ -MAX_SCROLL_DELTA,
        MAX_SCROLL_DELTA,
    )?;
    let expression = format!(
        "(() => {{ window.scrollBy({delta_x}, {delta_y}); return {{ ok: true, x: window.scrollX, y: window.scrollY }}; }})()"
    );
    let result = evaluate_json(&mut session.cdp, &expression)
        .await
        .map(|_| SimpleResult {
            ok: true,
            session_id,
            message: "scrolled".to_string(),
            elapsed_ms: Some(elapsed_ms(started)),
        });
    put_session(session).await;
    result
}

async fn handle_selection(args: SelectionArgs) -> Result<Value, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let session_id = session.id.clone();
    let overlay_result = if args.enable_overlay.unwrap_or(true) {
        if let Err(err) = ensure_write_access(&session, TOOL_SELECTION) {
            put_session(session).await;
            return Err(err);
        }
        ensure_overlay(&mut session).await
    } else {
        Ok(())
    };
    let output = match overlay_result {
        Ok(()) => evaluate_json(
                &mut session.cdp,
                r#"(() => window.__codexAgentBrowserOverview ? window.__codexAgentBrowserOverview() : { overlay: false })()"#,
            )
            .await
            .map(|mut overview| {
                overview["session_id"] = Value::String(session_id);
                overview["elapsed_ms"] = json!(elapsed_ms(started));
                overview
            }),
        Err(err) => Err(err),
    };
    put_session(session).await;
    output
}

async fn handle_highlight(args: HighlightArgs) -> Result<Value, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let session_id = session.id.clone();
    if let Err(err) = ensure_write_access(&session, TOOL_HIGHLIGHT) {
        put_session(session).await;
        return Err(err);
    }
    let result = async {
        ensure_overlay(&mut session).await?;
        let clear = args.clear.unwrap_or(false);
        let mut payload = json!({
            "clear": clear,
            "label": args.label.unwrap_or_else(|| "Codex highlight".to_string()),
            "color": args.color.unwrap_or_else(|| "#d93025".to_string()),
        });

        if let Some(element_ref) = args.element_ref {
            payload["ref"] = Value::String(element_ref);
        } else if !clear {
            let (Some(x), Some(y), Some(width), Some(height)) =
                (args.x, args.y, args.width, args.height)
            else {
                return Err(FunctionCallError::RespondToModel(
                    "highlight requires `clear`, an element `ref`, or x/y/width/height"
                        .to_string(),
                ));
            };
            let x = bounded_number("x", x, /*min*/ 0.0, f64::from(session.viewport_width))?;
            let y = bounded_number("y", y, /*min*/ 0.0, f64::from(session.viewport_height))?;
            let width = positive_bounded_number(
                "width",
                width,
                remaining_viewport_extent(/*name*/ "x", session.viewport_width, x)?,
            )?;
            let height = positive_bounded_number(
                "height",
                height,
                remaining_viewport_extent(/*name*/ "y", session.viewport_height, y)?,
            )?;
            payload["rect"] = json!({
                "x": x,
                "y": y,
                "width": width,
                "height": height,
            });
        }

        let payload = serde_json::to_string(&payload).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to serialize highlight request: {err}"
            ))
        })?;
        evaluate_json(
            &mut session.cdp,
            &format!(
                "(() => window.__codexAgentBrowserHighlight ? window.__codexAgentBrowserHighlight({payload}) : {{ ok: false, message: 'overlay unavailable' }})()"
            ),
        )
        .await
        .and_then(|mut overview| {
            if overview.get("ok").and_then(Value::as_bool) == Some(false) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "highlight failed: {}",
                    overview
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                )));
            }
            overview["session_id"] = Value::String(session_id);
            overview["elapsed_ms"] = json!(elapsed_ms(started));
            Ok(overview)
        })
    }
    .await;
    put_session(session).await;
    result
}

async fn handle_share(args: ShareArgs) -> Result<ShareResult, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let session_id = session.id.clone();
    let share_id = format!("bs-{}", Uuid::new_v4().simple());
    let url = page_url(&mut session.cdp).await.unwrap_or_default();
    let share = BrowserShare {
        share_id: share_id.clone(),
        access: args.access,
        engine: session.engine,
        mode: mode_name(&session.mode).to_string(),
        endpoint: session.endpoint.clone(),
        page_ws_url: session.page_ws_url.clone(),
        target_id: session.target_id.clone(),
        viewport_width: session.viewport_width,
        viewport_height: session.viewport_height,
        stealth: session.stealth,
        created_at_unix_ms: unix_ms_now(),
    };
    let share_file = write_browser_share(&share)?;
    let result = ShareResult {
        ok: true,
        session_id,
        share_id,
        access: args.access,
        backend: session.engine,
        mode: mode_name(&session.mode),
        endpoint: session.endpoint.clone(),
        remote_debugging_url: session.page_ws_url.clone(),
        share_file: share_file.display().to_string(),
        url,
        notes: vec![
            "pass share_id to agent_browser.open from another agent to attach to this live page"
                .to_string(),
            if args.access == ShareAccess::ReadOnly {
                "read_only shares allow snapshot and screenshot, but block navigation and input"
                    .to_string()
            } else {
                "read_write shares allow the receiving agent to navigate and interact".to_string()
            },
        ],
        elapsed_ms: elapsed_ms(started),
    };
    put_session(session).await;
    Ok(result)
}

async fn handle_benchmark(args: BenchmarkArgs) -> Result<BenchmarkResult, FunctionCallError> {
    let iterations = args.iterations.unwrap_or(3).clamp(1, 20);
    let open_started = Instant::now();
    let open = OpenArgs {
        url: None,
        share_id: None,
        mode: args.mode.clone(),
        backend: args.backend.clone(),
        stealth: args.stealth,
        viewport_width: Some(1280),
        viewport_height: Some(800),
        locale: Some(DEFAULT_LOCALE.to_string()),
        timezone: Some(DEFAULT_TIMEZONE.to_string()),
        user_agent: None,
        remote_debugging_url: args.remote_debugging_url,
    };
    let opened = handle_open(open).await?;
    let launch_ms = elapsed_ms(open_started);
    let session_id = opened.session_id.clone();
    let result = async {
        let target_url = args.url.clone().unwrap_or_else(benchmark_url);
        let navigate_started = Instant::now();
        handle_navigate(NavigateArgs {
            session_id: Some(session_id.clone()),
            url: target_url.clone(),
        })
        .await?;
        let navigate_ms = elapsed_ms(navigate_started);
        let mut snapshot_ms = Vec::with_capacity(iterations);
        let mut screenshot_ms = Vec::with_capacity(iterations);
        let mut screenshot_png_bytes = Vec::with_capacity(iterations);
        let mut screenshot_base64_chars = Vec::with_capacity(iterations);

        for _ in 0..iterations {
            let started = Instant::now();
            let _ = handle_snapshot(SnapshotArgs {
                session_id: Some(session_id.clone()),
                max_text_chars: Some(8_000),
                max_elements: Some(80),
            })
            .await?;
            snapshot_ms.push(elapsed_ms(started));

            let started = Instant::now();
            let screenshot = handle_screenshot(ScreenshotArgs {
                session_id: Some(session_id.clone()),
                full_page: Some(false),
            })
            .await?;
            screenshot_ms.push(elapsed_ms(started));
            let (png_bytes, base64_chars) = screenshot_image_sizes(&screenshot);
            screenshot_png_bytes.push(png_bytes);
            screenshot_base64_chars.push(base64_chars);
        }

        Ok(BenchmarkResult {
            mode: mode_name(&args.mode),
            backend: opened.backend,
            stealth: args.stealth,
            target_url,
            iterations,
            launch_ms,
            navigate_ms,
            totals: BenchmarkTotals {
                snapshot_avg_ms: average_ms(&snapshot_ms),
                screenshot_avg_ms: average_ms(&screenshot_ms),
                screenshot_png_avg_bytes: average_usize(&screenshot_png_bytes),
                screenshot_base64_avg_chars: average_usize(&screenshot_base64_chars),
            },
            snapshot_ms,
            screenshot_ms,
            screenshot_png_bytes,
            screenshot_base64_chars,
        })
    }
    .await;

    let _ = handle_close(SessionArgs {
        session_id: Some(session_id),
    })
    .await;
    result
}

fn average_ms(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    round_ms(values.iter().sum::<f64>() / values.len() as f64)
}

fn average_usize(values: &[usize]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    round_ms(values.iter().sum::<usize>() as f64 / values.len() as f64)
}

fn screenshot_image_sizes(output: &FunctionToolOutput) -> (usize, usize) {
    output
        .body
        .iter()
        .find_map(|item| {
            let FunctionCallOutputContentItem::InputImage { image_url, .. } = item else {
                return None;
            };
            let payload = image_url.split_once(";base64,")?.1;
            let png_bytes = BASE64_STANDARD
                .decode(payload.as_bytes())
                .map(|bytes| bytes.len())
                .unwrap_or_default();
            Some((png_bytes, payload.len()))
        })
        .unwrap_or_default()
}

fn ensure_write_access(session: &BrowserSession, action: &str) -> Result<(), FunctionCallError> {
    if session.access == ShareAccess::ReadOnly {
        return Err(FunctionCallError::RespondToModel(format!(
            "agent_browser session `{}` is a read_only shared session; `{action}` requires a read_write share",
            session.id
        )));
    }
    Ok(())
}

fn unix_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn browser_share_dir() -> PathBuf {
    std::env::temp_dir().join("codex-agent-browser-shares")
}

fn browser_share_path(share_id: &str) -> Result<PathBuf, FunctionCallError> {
    if share_id.is_empty()
        || !share_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(FunctionCallError::RespondToModel(
            "`share_id` must contain only letters, numbers, '-' or '_'".to_string(),
        ));
    }
    Ok(browser_share_dir().join(format!("{share_id}.json")))
}

fn write_browser_share(share: &BrowserShare) -> Result<PathBuf, FunctionCallError> {
    let dir = browser_share_dir();
    fs::create_dir_all(&dir).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to create browser share directory `{}`: {err}",
            dir.display()
        ))
    })?;
    let path = browser_share_path(&share.share_id)?;
    let json = serde_json::to_vec(share).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize browser share: {err}"))
    })?;
    fs::write(&path, json).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to write browser share `{}`: {err}",
            path.display()
        ))
    })?;
    Ok(path)
}

fn read_browser_share(share_id: &str) -> Result<BrowserShare, FunctionCallError> {
    let path = browser_share_path(share_id)?;
    let json = fs::read(&path).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to read browser share `{}`: {err}",
            path.display()
        ))
    })?;
    serde_json::from_slice(&json).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "browser share `{}` was invalid: {err}",
            path.display()
        ))
    })
}

async fn ensure_obscura_page_session(cdp: &mut CdpClient) -> Result<(), FunctionCallError> {
    if cdp.has_session() {
        return Ok(());
    }

    let target = cdp
        .call_browser("Target.createTarget", json!({ "url": "about:blank" }))
        .await?;
    let target_id = target
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "Obscura Target.createTarget did not return targetId".to_string(),
            )
        })?;
    let attached = cdp
        .call_browser(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await?;
    let session_id = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "Obscura Target.attachToTarget did not return sessionId".to_string(),
            )
        })?;
    cdp.set_session_id(session_id.to_string());
    cdp.set_target_id(target_id.to_string());
    Ok(())
}

async fn attach_obscura_target(
    cdp: &mut CdpClient,
    target_id: &str,
) -> Result<(), FunctionCallError> {
    if cdp.has_session() {
        return Ok(());
    }
    let attached = cdp
        .call_browser(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await?;
    let session_id = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "shared Obscura Target.attachToTarget did not return sessionId".to_string(),
            )
        })?;
    cdp.set_session_id(session_id.to_string());
    cdp.set_target_id(target_id.to_string());
    Ok(())
}

async fn initialize_page(
    cdp: &mut CdpClient,
    options: PageInitOptions<'_>,
) -> Result<(), FunctionCallError> {
    if options.engine == BrowserEngine::Obscura {
        ensure_obscura_page_session(cdp).await?;
    }
    cdp.call("Page.enable", json!({})).await?;
    cdp.call("Runtime.enable", json!({})).await?;
    cdp.call(
        "Emulation.setDeviceMetricsOverride",
        json!({
            "width": options.viewport_width,
            "height": options.viewport_height,
            "deviceScaleFactor": 1,
            "mobile": false,
        }),
    )
    .await?;
    if options.engine != BrowserEngine::Obscura {
        cdp_call_allowing(
            cdp,
            "Emulation.setLocaleOverride",
            json!({
                "locale": options.locale,
            }),
            "Another locale override is already in effect",
        )
        .await?;
        cdp_call_allowing(
            cdp,
            "Emulation.setTimezoneOverride",
            json!({
                "timezoneId": options.timezone,
            }),
            "Another timezone override is already in effect",
        )
        .await?;
    } else {
        let _ = options.timezone;
    }
    let user_agent_override = if let Some(user_agent) = options.user_agent {
        Some(user_agent.to_string())
    } else if options.stealth && options.engine != BrowserEngine::Obscura {
        browser_user_agent(cdp)
            .await?
            .map(|user_agent| stealth_user_agent(&user_agent))
    } else {
        None
    };
    if let Some(user_agent) = user_agent_override {
        cdp.call("Network.enable", json!({})).await?;
        cdp.call(
            "Network.setUserAgentOverride",
            json!({
                "userAgent": user_agent,
                "acceptLanguage": options.locale,
            }),
        )
        .await?;
    }
    if options.stealth && options.engine != BrowserEngine::Obscura {
        let script = stealth_script(options.locale);
        cdp.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": script }),
        )
        .await?;
        evaluate_json(cdp, &script).await?;
    }
    Ok(())
}

async fn browser_user_agent(cdp: &mut CdpClient) -> Result<Option<String>, FunctionCallError> {
    let version = evaluate_json(cdp, "(() => ({ userAgent: navigator.userAgent }))()").await?;
    Ok(version
        .get("userAgent")
        .and_then(Value::as_str)
        .map(str::to_string))
}

fn stealth_user_agent(user_agent: &str) -> String {
    user_agent.replace("HeadlessChrome", "Chrome")
}

async fn navigate_to(cdp: &mut CdpClient, url: &str) -> Result<(), FunctionCallError> {
    cdp.call("Page.navigate", json!({ "url": url })).await?;
    for _ in 0..60 {
        let ready = evaluate_json(cdp, "(() => ({ readyState: document.readyState }))()").await?;
        if matches!(
            ready.get("readyState").and_then(Value::as_str),
            Some("interactive" | "complete")
        ) {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

async fn page_url(cdp: &mut CdpClient) -> Result<String, FunctionCallError> {
    let value = evaluate_json(cdp, "(() => ({ url: location.href }))()").await?;
    Ok(value
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string())
}

async fn snapshot_page(
    cdp: &mut CdpClient,
    max_text_chars: usize,
    max_elements: usize,
    ref_mode: SnapshotRefMode,
) -> Result<Value, FunctionCallError> {
    let ref_setup = match ref_mode {
        SnapshotRefMode::ActionRefs => {
            r#"
            window.__codexAgentBrowserNextRef = window.__codexAgentBrowserNextRef || 1;
            window.__codexAgentBrowserElementRefs = window.__codexAgentBrowserElementRefs || new WeakMap();
            window.__codexAgentBrowserRefElements = new Map();
            "#
        }
        SnapshotRefMode::ReadOnly => "const readOnlyRefs = new Map();",
    };
    let ref_for = match ref_mode {
        SnapshotRefMode::ActionRefs => {
            r#"
            const refFor = (el) => {
                let ref = window.__codexAgentBrowserElementRefs.get(el);
                if (!ref) {
                    ref = "e" + window.__codexAgentBrowserNextRef++;
                    window.__codexAgentBrowserElementRefs.set(el, ref);
                }
                window.__codexAgentBrowserRefElements.set(ref, el);
                return ref;
            };
            "#
        }
        SnapshotRefMode::ReadOnly => {
            r#"
            const refFor = (el) => {
                let ref = readOnlyRefs.get(el);
                if (!ref) {
                    ref = "e" + (readOnlyRefs.size + 1);
                    readOnlyRefs.set(el, ref);
                }
                return ref;
            };
            "#
        }
    };
    let expression = format!(
        r#"(() => {{
            const maxText = {max_text_chars};
            const maxElements = {max_elements};
            {ref_setup}
            {ref_for}
            const selectors = [
                "a[href]", "button", "input", "textarea", "select", "[role=button]",
                "[role=link]", "[contenteditable=true]", "summary", "[tabindex]:not([tabindex='-1'])"
            ];
            const seen = new Set();
            const elements = [];
            for (const el of document.querySelectorAll(selectors.join(","))) {{
                if (seen.has(el) || elements.length >= maxElements) continue;
                seen.add(el);
                const rect = el.getBoundingClientRect();
                if (rect.width <= 0 || rect.height <= 0) continue;
                const label = (el.getAttribute("aria-label") || el.getAttribute("title") || el.innerText || el.value || el.placeholder || "").trim().replace(/\s+/g, " ").slice(0, 220);
                elements.push({{
                    ref: refFor(el),
                    tag: el.tagName.toLowerCase(),
                    role: el.getAttribute("role"),
                    label,
                    href: el.href || null,
                    type: el.getAttribute("type"),
                    rect: {{
                        x: Math.round(rect.x),
                        y: Math.round(rect.y),
                        width: Math.round(rect.width),
                        height: Math.round(rect.height)
                    }}
                }});
            }}
            const selection = window.getSelection();
            return {{
                url: location.href,
                title: document.title,
                readyState: document.readyState,
                text: (document.body && document.body.innerText || "").replace(/\n{{3,}}/g, "\n\n").slice(0, maxText),
                selection: selection ? selection.toString().slice(0, 4000) : "",
                elements,
                viewport: {{
                    width: window.innerWidth,
                    height: window.innerHeight,
                    scrollX: window.scrollX,
                    scrollY: window.scrollY
                }}
            }};
        }})()"#
    );
    evaluate_json(cdp, &expression).await
}

fn snapshot_ref_mode(session: &BrowserSession) -> SnapshotRefMode {
    if session.access == ShareAccess::ReadOnly {
        SnapshotRefMode::ReadOnly
    } else {
        SnapshotRefMode::ActionRefs
    }
}

async fn inject_overlay(cdp: &mut CdpClient) -> Result<(), FunctionCallError> {
    cdp.call(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": overlay_script() }),
    )
    .await?;
    evaluate_json(cdp, overlay_script()).await?;
    evaluate_json(cdp, "(() => window.__codexAgentBrowserOverview())()").await?;
    Ok(())
}

async fn ensure_overlay(session: &mut BrowserSession) -> Result<(), FunctionCallError> {
    if session.overlay_script_registered {
        return Ok(());
    }
    inject_overlay(&mut session.cdp).await?;
    session.overlay_script_registered = true;
    Ok(())
}

async fn evaluate_json(cdp: &mut CdpClient, expression: &str) -> Result<Value, FunctionCallError> {
    let result = cdp
        .call(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )
        .await?;
    if let Some(exception) = result.get("exceptionDetails") {
        return Err(FunctionCallError::RespondToModel(format!(
            "browser evaluation failed: {exception}"
        )));
    }
    Ok(result
        .get("result")
        .and_then(|result| result.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

struct LaunchConnection {
    engine: BrowserEngine,
    mode: Option<BrowserMode>,
    access: ShareAccess,
    stealth: Option<bool>,
    viewport_width: Option<u32>,
    viewport_height: Option<u32>,
    target_id: Option<String>,
    endpoint: String,
    page_ws_url: String,
    process: Option<Child>,
    profile_dir: Option<TempDir>,
    owned_page_close_url: Option<String>,
    notes: Vec<String>,
}

async fn attach_connection(remote: &str) -> Result<LaunchConnection, FunctionCallError> {
    if remote.starts_with("ws://") || remote.starts_with("wss://") {
        return Ok(LaunchConnection {
            engine: BrowserEngine::ExternalCdp,
            mode: None,
            access: ShareAccess::ReadWrite,
            stealth: None,
            viewport_width: None,
            viewport_height: None,
            target_id: None,
            endpoint: remote.to_string(),
            page_ws_url: remote.to_string(),
            process: None,
            profile_dir: None,
            owned_page_close_url: None,
            notes: vec!["attached to explicit websocket endpoint".to_string()],
        });
    }

    let endpoint = remote.trim_end_matches('/').to_string();
    let page = first_page(&endpoint).await?;
    Ok(LaunchConnection {
        engine: BrowserEngine::ExternalCdp,
        mode: None,
        access: ShareAccess::ReadWrite,
        stealth: None,
        viewport_width: None,
        viewport_height: None,
        target_id: None,
        endpoint,
        page_ws_url: page.ws_url,
        process: None,
        profile_dir: None,
        owned_page_close_url: page.owned_close_url,
        notes: vec!["attached to existing remote debugging endpoint".to_string()],
    })
}

async fn attach_shared_connection(share_id: &str) -> Result<LaunchConnection, FunctionCallError> {
    let share = read_browser_share(share_id)?;
    Ok(LaunchConnection {
        engine: share.engine,
        mode: Some(browser_mode_from_name(&share.mode)?),
        access: share.access,
        stealth: Some(share.stealth),
        viewport_width: Some(share.viewport_width),
        viewport_height: Some(share.viewport_height),
        target_id: share.target_id,
        endpoint: share.endpoint,
        page_ws_url: share.page_ws_url,
        process: None,
        profile_dir: None,
        owned_page_close_url: None,
        notes: vec![format!("attached to shared browser session `{share_id}`")],
    })
}

async fn launch_connection(
    backend: &BrowserBackend,
    mode: &BrowserMode,
    stealth: bool,
    viewport_width: u32,
    viewport_height: u32,
    locale: &str,
) -> Result<LaunchConnection, FunctionCallError> {
    match backend {
        BrowserBackend::Obscura => launch_obscura_connection(mode, stealth).await,
        BrowserBackend::Chromium => {
            launch_chromium_connection(mode, stealth, viewport_width, viewport_height, locale).await
        }
        BrowserBackend::Auto => {
            if matches!(mode, BrowserMode::Headless)
                && let Ok(connection) = launch_obscura_connection(mode, stealth).await
            {
                return Ok(connection);
            }
            launch_chromium_connection(mode, stealth, viewport_width, viewport_height, locale).await
        }
    }
}

async fn launch_obscura_connection(
    mode: &BrowserMode,
    stealth: bool,
) -> Result<LaunchConnection, FunctionCallError> {
    let binary = find_obscura_binary()?;
    let port = free_local_port()?;
    let profile_dir = tempfile::Builder::new()
        .prefix("codex-agent-obscura-")
        .tempdir()
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to create Obscura temp dir: {err}"))
        })?;
    let browser_stderr_path = profile_dir.path().join("obscura-stderr.log");
    let browser_stderr = fs::File::create(&browser_stderr_path).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to create Obscura stderr log: {err}"))
    })?;
    let endpoint = format!("http://127.0.0.1:{port}");

    let mut command = Command::new(&binary);
    command.kill_on_drop(true);
    command
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(browser_stderr));
    if stealth {
        command.arg("--stealth");
    }

    let mut process = command.spawn().map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to launch Obscura browser `{}`: {err}",
            binary.display()
        ))
    })?;

    let page = match wait_for_first_page(&endpoint, &mut process, Some(&browser_stderr_path)).await
    {
        Ok(page) => page,
        Err(err) => {
            let _ = process.kill().await;
            return Err(err);
        }
    };
    let mut notes = vec![format!("launched Obscura {}", binary.display())];
    if matches!(mode, BrowserMode::Headful) {
        notes.push(
            "Obscura engine is headless; Codex is opening a lightweight headful mirror shell"
                .to_string(),
        );
    }
    Ok(LaunchConnection {
        engine: BrowserEngine::Obscura,
        mode: None,
        access: ShareAccess::ReadWrite,
        stealth: None,
        viewport_width: None,
        viewport_height: None,
        target_id: None,
        endpoint,
        page_ws_url: page.ws_url,
        process: Some(process),
        profile_dir: Some(profile_dir),
        owned_page_close_url: page.owned_close_url,
        notes,
    })
}

async fn launch_chromium_connection(
    mode: &BrowserMode,
    stealth: bool,
    viewport_width: u32,
    viewport_height: u32,
    locale: &str,
) -> Result<LaunchConnection, FunctionCallError> {
    let binary = find_browser_binary()?;
    let port = free_local_port()?;
    let profile_dir = tempfile::Builder::new()
        .prefix("codex-agent-browser-")
        .tempdir()
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to create browser profile dir: {err}"
            ))
        })?;
    let endpoint = format!("http://127.0.0.1:{port}");
    let browser_home = profile_dir.path().join("home");
    let browser_config = profile_dir.path().join("config");
    let browser_cache = profile_dir.path().join("cache");
    let browser_stderr_path = profile_dir.path().join("browser-stderr.log");
    fs::create_dir_all(&browser_home).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to create browser home dir: {err}"))
    })?;
    fs::create_dir_all(&browser_config).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to create browser config dir: {err}"))
    })?;
    fs::create_dir_all(&browser_cache).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to create browser cache dir: {err}"))
    })?;
    let browser_stderr = fs::File::create(&browser_stderr_path).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to create browser stderr log: {err}"))
    })?;

    let mut command = Command::new(&binary);
    command.kill_on_drop(true);
    command
        .env("HOME", &browser_home)
        .env("XDG_CONFIG_HOME", &browser_config)
        .env("XDG_CACHE_HOME", &browser_cache)
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile_dir.path().display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--disable-popup-blocking")
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .arg("--disable-breakpad")
        .arg("--disable-crash-reporter")
        .arg("--disable-crashpad")
        .arg("--disable-sync")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .arg("--metrics-recording-only")
        .arg("--password-store=basic")
        .arg("--use-mock-keychain")
        .arg(format!("--lang={locale}"))
        .arg(format!("--window-size={viewport_width},{viewport_height}"))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(browser_stderr));

    if matches!(mode, BrowserMode::Headless) {
        command.arg("--headless=new");
    }

    if stealth {
        command
            .arg("--disable-blink-features=AutomationControlled")
            .arg("--disable-infobars")
            .arg("--force-webrtc-ip-handling-policy=default_public_interface_only");
    }

    if std::env::var("CODEX_AGENT_BROWSER_NO_SANDBOX").as_deref() == Ok("1") {
        command.arg("--no-sandbox");
    }

    let mut process = command.spawn().map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to launch browser `{}`: {err}",
            binary.display()
        ))
    })?;

    let page = match wait_for_first_page(&endpoint, &mut process, Some(&browser_stderr_path)).await
    {
        Ok(page) => page,
        Err(err) => {
            let _ = process.kill().await;
            return Err(err);
        }
    };
    Ok(LaunchConnection {
        engine: BrowserEngine::Chromium,
        mode: None,
        access: ShareAccess::ReadWrite,
        stealth: None,
        viewport_width: None,
        viewport_height: None,
        target_id: None,
        endpoint,
        page_ws_url: page.ws_url,
        process: Some(process),
        profile_dir: Some(profile_dir),
        owned_page_close_url: page.owned_close_url,
        notes: vec![format!("launched {}", binary.display())],
    })
}

async fn kill_launched_browser(process: &mut Option<Child>) {
    if let Some(mut child) = process.take() {
        let _ = child.kill().await;
    }
}

async fn cleanup_launch(launch: &mut LaunchConnection) {
    let had_process = launch.process.is_some();
    kill_launched_browser(&mut launch.process).await;
    if !had_process && let Some(close_url) = launch.owned_page_close_url.take() {
        let _ = browser_http_client().get(close_url).send().await;
    }
}

async fn wait_for_first_page(
    endpoint: &str,
    process: &mut Child,
    stderr_path: Option<&Path>,
) -> Result<PageTarget, FunctionCallError> {
    let mut last_error = None;
    for _ in 0..80 {
        match first_page(endpoint).await {
            Ok(page) => return Ok(page),
            Err(err) => last_error = Some(err),
        }
        if let Some(status) = process.try_wait().map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to inspect browser process while waiting for `{endpoint}`: {err}"
            ))
        })? {
            let stderr = stderr_path
                .map(browser_stderr_tail)
                .filter(|stderr| !stderr.is_empty())
                .map(|stderr| format!("; stderr: {stderr}"))
                .unwrap_or_default();
            return Err(FunctionCallError::RespondToModel(format!(
                "browser process exited before exposing `{endpoint}` with status {status}{stderr}"
            )));
        }
        sleep(Duration::from_millis(100)).await;
    }
    let stderr = stderr_path
        .map(browser_stderr_tail)
        .filter(|stderr| !stderr.is_empty())
        .map(|stderr| format!("; stderr: {stderr}"))
        .unwrap_or_default();
    Err(last_error.unwrap_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "browser did not expose a debuggable page at `{endpoint}`{stderr}"
        ))
    }))
}

fn browser_stderr_tail(path: &Path) -> String {
    let Ok(stderr) = fs::read_to_string(path) else {
        return String::new();
    };
    let stderr = stderr.trim();
    if stderr.len() <= 4_000 {
        return stderr.to_string();
    }
    let mut tail = stderr
        .chars()
        .rev()
        .take(4_000)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    tail.insert_str(0, "...");
    tail
}

struct PageTarget {
    ws_url: String,
    owned_close_url: Option<String>,
}

async fn first_page(endpoint: &str) -> Result<PageTarget, FunctionCallError> {
    #[derive(Deserialize)]
    struct Target {
        #[serde(rename = "type")]
        target_type: Option<String>,
        #[serde(rename = "webSocketDebuggerUrl")]
        web_socket_debugger_url: Option<String>,
    }

    let url = format!("{}/json/list", endpoint.trim_end_matches('/'));
    let targets: Vec<Target> = browser_http_client()
        .get(&url)
        .send()
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to query browser target list `{url}`: {err}"
            ))
        })?
        .json()
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse browser target list `{url}`: {err}"
            ))
        })?;

    if let Some(ws_url) = targets
        .into_iter()
        .find(|target| target.target_type.as_deref() == Some("page"))
        .and_then(|target| target.web_socket_debugger_url)
    {
        return Ok(PageTarget {
            ws_url,
            owned_close_url: None,
        });
    }

    create_page(endpoint, &url).await
}

async fn create_page(
    endpoint: &str,
    target_list_url: &str,
) -> Result<PageTarget, FunctionCallError> {
    #[derive(Deserialize)]
    struct Target {
        id: Option<String>,
        #[serde(rename = "webSocketDebuggerUrl")]
        web_socket_debugger_url: Option<String>,
    }

    let url = format!("{}/json/new?about:blank", endpoint.trim_end_matches('/'));
    let target: Target = browser_http_client()
        .put(&url)
        .send()
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "browser target list `{target_list_url}` did not include a page websocket and `{url}` failed: {err}"
            ))
        })?
        .json()
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "browser target list `{target_list_url}` did not include a page websocket and `{url}` returned invalid JSON: {err}"
            ))
        })?;

    let ws_url = target.web_socket_debugger_url.ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "browser target list `{target_list_url}` did not include a page websocket and `{url}` did not create one"
        ))
    })?;
    let owned_close_url = target
        .id
        .map(|id| format!("{}/json/close/{id}", endpoint.trim_end_matches('/')));
    Ok(PageTarget {
        ws_url,
        owned_close_url,
    })
}

fn browser_http_client() -> &'static reqwest::Client {
    BROWSER_HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(BROWSER_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

fn find_browser_binary() -> Result<PathBuf, FunctionCallError> {
    if let Ok(path) = std::env::var("CODEX_AGENT_BROWSER_BINARY") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }

    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ];
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Ok(path);
        }
    }

    for name in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ] {
        if let Ok(path) = which::which(name) {
            return Ok(path);
        }
    }

    Err(FunctionCallError::RespondToModel(
        "no Chrome/Chromium browser binary found; set CODEX_AGENT_BROWSER_BINARY".to_string(),
    ))
}

fn find_obscura_binary() -> Result<PathBuf, FunctionCallError> {
    for path in candidate_obscura_binaries() {
        if path.exists() {
            return Ok(path);
        }
    }

    which::which("obscura").map_err(|_| {
        FunctionCallError::RespondToModel(
            "no Obscura browser binary found; bundle `obscura` next to the Codex executable, set CODEX_AGENT_BROWSER_OBSCURA_BINARY, or open with backend=chromium".to_string(),
        )
    })
}

fn candidate_obscura_binaries() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("CODEX_AGENT_BROWSER_OBSCURA_BINARY") {
        push_unique_path(&mut candidates, PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe() {
        for candidate in obscura_binary_candidates_for_exe(&exe) {
            push_unique_path(&mut candidates, candidate);
        }
    }
    candidates
}

fn obscura_binary_candidates_for_exe(exe: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = exe.parent() {
        push_unique_path(&mut candidates, dir.join("obscura"));
        #[cfg(target_os = "macos")]
        if dir.file_name().and_then(|value| value.to_str()) == Some("MacOS")
            && let Some(contents_dir) = dir.parent()
        {
            push_unique_path(
                &mut candidates,
                contents_dir.join("Resources").join("obscura"),
            );
        }
    }
    candidates
}

fn push_unique_path(candidates: &mut Vec<PathBuf>, path: PathBuf) {
    if !candidates.iter().any(|candidate| candidate == &path) {
        candidates.push(path);
    }
}

fn free_local_port() -> Result<u16, FunctionCallError> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to reserve browser debug port: {err}"))
    })?;
    let port = listener.local_addr().map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to read browser debug port: {err}"))
    })?;
    Ok(port.port())
}

fn mode_name(mode: &BrowserMode) -> &'static str {
    match mode {
        BrowserMode::Headful => "headful",
        BrowserMode::Headless => "headless",
    }
}

fn browser_mode_from_name(mode: &str) -> Result<BrowserMode, FunctionCallError> {
    match mode {
        "headful" => Ok(BrowserMode::Headful),
        "headless" => Ok(BrowserMode::Headless),
        other => Err(FunctionCallError::RespondToModel(format!(
            "shared browser session had unsupported mode `{other}`"
        ))),
    }
}

async fn refresh_obscura_visual_shell(
    cdp: &mut CdpClient,
    path: &Path,
    viewport_width: u32,
    viewport_height: u32,
) -> Result<(), FunctionCallError> {
    let snapshot = snapshot_page(
        cdp,
        /*max_text_chars*/ 16_000,
        /*max_elements*/ 160,
        SnapshotRefMode::ActionRefs,
    )
    .await?;
    agent_browser_visual::write_visual_shell(path, &snapshot, viewport_width, viewport_height)
}

fn benchmark_url() -> String {
    format!(
        "data:text/html;charset=utf-8,{}",
        urlencoding_like(benchmark_html())
    )
}

fn benchmark_html() -> &'static str {
    r##"<!doctype html>
<meta charset="utf-8">
<title>Codex Agent Browser Benchmark</title>
<main>
  <h1>Codex Agent Browser Benchmark</h1>
  <input aria-label="Search" placeholder="Search">
  <button>Run</button>
  <a href="#details">Details</a>
  <section id="details">This local page measures launch, navigation, snapshot, and screenshot latency.</section>
</main>"##
}

fn urlencoding_like(input: &str) -> String {
    input
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            b' ' => vec!['%', '2', '0'],
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn stealth_script(locale: &str) -> String {
    let languages = locale
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let languages_json = serde_json::to_string(&if languages.is_empty() {
        vec![DEFAULT_LOCALE]
    } else {
        languages
    })
    .unwrap_or_else(|_| "[\"en-US\"]".to_string());

    format!(
        r#"
        (() => {{
            const define = (target, key, getter) => {{
                try {{ Object.defineProperty(target, key, {{ get: getter, configurable: true }}); }} catch (_) {{}}
            }};
            define(Navigator.prototype, "webdriver", () => undefined);
            define(Navigator.prototype, "languages", () => {languages_json});
            if (!window.chrome) {{
                Object.defineProperty(window, "chrome", {{
                    value: {{ runtime: {{}} }},
                    configurable: true
                }});
            }}
            const originalQuery = window.navigator.permissions && window.navigator.permissions.query;
            if (originalQuery) {{
                window.navigator.permissions.query = (parameters) => (
                    parameters && parameters.name === "notifications"
                        ? Promise.resolve({{ state: Notification.permission }})
                        : originalQuery.call(window.navigator.permissions, parameters)
                );
            }}
        }})();
        "#
    )
}

fn overlay_script() -> &'static str {
    r##"
    (() => {
        if (window.__codexAgentBrowserOverlayInstalled) return;
        window.__codexAgentBrowserOverlayInstalled = true;
        window.__codexAgentBrowserLastSelection = null;
        window.__codexAgentBrowserHighlights = window.__codexAgentBrowserHighlights || [];
        const maxHighlights = 40;

        const box = document.createElement("div");
        box.id = "codex-agent-browser-selection-overlay";
        box.style.cssText = [
            "position:fixed", "pointer-events:none", "z-index:2147483647",
            "border:2px solid #1a73e8", "background:rgba(26,115,232,.14)",
            "box-shadow:0 0 0 1px rgba(255,255,255,.75)", "display:none"
        ].join(";");

        const label = document.createElement("div");
        label.style.cssText = [
            "position:fixed", "pointer-events:none", "z-index:2147483647",
            "font:12px/1.35 -apple-system,BlinkMacSystemFont,Segoe UI,sans-serif",
            "color:#fff", "background:#1a73e8", "padding:3px 6px",
            "border-radius:4px", "display:none"
        ].join(";");
        label.textContent = "Codex selection";

        const highlightLayer = document.createElement("div");
        highlightLayer.id = "codex-agent-browser-highlight-layer";
        highlightLayer.style.cssText = [
            "position:fixed", "inset:0", "pointer-events:none",
            "z-index:2147483646"
        ].join(";");

        const ensure = () => {
            if (!box.isConnected) document.documentElement.appendChild(box);
            if (!label.isConnected) document.documentElement.appendChild(label);
            if (!highlightLayer.isConnected) document.documentElement.appendChild(highlightLayer);
        };

        const rectFromSelection = () => {
            const selection = window.getSelection();
            if (!selection || selection.rangeCount === 0 || !selection.toString()) return null;
            const rect = selection.getRangeAt(0).getBoundingClientRect();
            if (!rect || (rect.width <= 0 && rect.height <= 0)) return null;
            return rect;
        };

        const paint = () => {
            ensure();
            const rect = rectFromSelection();
            if (!rect) {
                box.style.display = "none";
                label.style.display = "none";
                window.__codexAgentBrowserLastSelection = null;
                return;
            }
            box.style.display = "block";
            label.style.display = "block";
            box.style.left = `${Math.max(0, rect.left)}px`;
            box.style.top = `${Math.max(0, rect.top)}px`;
            box.style.width = `${Math.max(1, rect.width)}px`;
            box.style.height = `${Math.max(1, rect.height)}px`;
            label.style.left = `${Math.max(0, rect.left)}px`;
            label.style.top = `${Math.max(0, rect.top - 24)}px`;
            window.__codexAgentBrowserLastSelection = {
                text: window.getSelection().toString().slice(0, 4000),
                rect: {
                    x: Math.round(rect.x),
                    y: Math.round(rect.y),
                    width: Math.round(rect.width),
                    height: Math.round(rect.height)
                },
                url: location.href,
                title: document.title,
                capturedAt: new Date().toISOString()
            };
        };

        const cleanColor = (color) => {
            if (typeof color !== "string" || color.length > 64
                || !/^[#a-zA-Z0-9(),.%\s-]+$/.test(color)) {
                return "#d93025";
            }
            return !window.CSS || !CSS.supports || CSS.supports("color", color)
                ? color
                : "#d93025";
        };

        const rectJson = (rect) => ({
            x: Math.round(rect.x),
            y: Math.round(rect.y),
            width: Math.round(rect.width),
            height: Math.round(rect.height)
        });

        const rectForHighlight = (highlight) => {
            if (highlight.ref) {
                const refs = window.__codexAgentBrowserRefElements;
                const el = refs && refs.get(highlight.ref);
                if (!el || !el.isConnected) return null;
                const rect = el.getBoundingClientRect();
                if (!rect || rect.width <= 0 || rect.height <= 0) return null;
                return rectJson(rect);
            }
            return highlight.rect || null;
        };

        const renderHighlights = () => {
            ensure();
            highlightLayer.textContent = "";
            for (const highlight of window.__codexAgentBrowserHighlights) {
                const rect = rectForHighlight(highlight);
                if (!rect) continue;
                const item = document.createElement("div");
                const color = cleanColor(highlight.color);
                item.style.cssText = [
                    "position:fixed",
                    `left:${Math.max(0, rect.x)}px`,
                    `top:${Math.max(0, rect.y)}px`,
                    `width:${Math.max(1, rect.width)}px`,
                    `height:${Math.max(1, rect.height)}px`,
                    `border:2px solid ${color}`,
                    "background:rgba(217,48,37,.12)",
                    "box-shadow:0 0 0 1px rgba(255,255,255,.85)"
                ].join(";");
                const itemLabel = document.createElement("div");
                itemLabel.textContent = highlight.label || "Codex highlight";
                itemLabel.style.cssText = [
                    "position:absolute", "left:-2px", "top:-24px",
                    "max-width:280px", "white-space:nowrap", "overflow:hidden",
                    "text-overflow:ellipsis",
                    "font:12px/1.35 -apple-system,BlinkMacSystemFont,Segoe UI,sans-serif",
                    `background:${color}`, "color:#fff", "padding:3px 6px",
                    "border-radius:4px"
                ].join(";");
                item.appendChild(itemLabel);
                highlightLayer.appendChild(item);
            }
        };

        const overviewPayload = (ok = true) => {
            paint();
            renderHighlights();
            const selection = window.__codexAgentBrowserLastSelection;
            return {
                ok,
                overlay: true,
                hasSelection: !!(selection && selection.text),
                selection,
                highlights: window.__codexAgentBrowserHighlights.map((highlight) => ({
                    id: highlight.id,
                    ref: highlight.ref || null,
                    label: highlight.label,
                    color: highlight.color,
                    rect: rectForHighlight(highlight),
                    capturedAt: highlight.capturedAt
                })),
                url: location.href,
                title: document.title
            };
        };

        window.__codexAgentBrowserHighlight = (request) => {
            if (request && request.clear) {
                window.__codexAgentBrowserHighlights = [];
            }
            if (!request || request.clear) return overviewPayload();

            let rect = null;
            let ref = null;
            if (request.ref) {
                ref = String(request.ref);
                const refs = window.__codexAgentBrowserRefElements;
                const el = refs && refs.get(ref);
                if (!el || !el.isConnected) {
                    return { ok: false, message: "element ref not found; call snapshot first" };
                }
                const bounds = el.getBoundingClientRect();
                if (!bounds || bounds.width <= 0 || bounds.height <= 0) {
                    return { ok: false, message: "element ref is not visible" };
                }
                rect = rectJson(bounds);
            } else if (request.rect) {
                rect = {
                    x: Number(request.rect.x),
                    y: Number(request.rect.y),
                    width: Number(request.rect.width),
                    height: Number(request.rect.height)
                };
            }
            if (!rect || !Number.isFinite(rect.x) || !Number.isFinite(rect.y)
                || !Number.isFinite(rect.width) || !Number.isFinite(rect.height)
                || rect.width <= 0 || rect.height <= 0) {
                return { ok: false, message: "highlight requires a visible ref or positive rect" };
            }

            window.__codexAgentBrowserHighlights.push({
                id: `h${Date.now().toString(36)}${Math.random().toString(36).slice(2, 7)}`,
                ref,
                rect,
                label: String(request.label || "Codex highlight").slice(0, 120),
                color: cleanColor(request.color || "#d93025"),
                capturedAt: new Date().toISOString()
            });
            if (window.__codexAgentBrowserHighlights.length > maxHighlights) {
                window.__codexAgentBrowserHighlights.splice(
                    0,
                    window.__codexAgentBrowserHighlights.length - maxHighlights
                );
            }
            return overviewPayload();
        };

        document.addEventListener("selectionchange", () => requestAnimationFrame(paint), true);
        window.addEventListener("scroll", () => requestAnimationFrame(() => { paint(); renderHighlights(); }), true);
        window.addEventListener("resize", () => requestAnimationFrame(() => { paint(); renderHighlights(); }), true);

        window.__codexAgentBrowserOverview = () => {
            return overviewPayload();
        };
        paint();
        renderHighlights();
    })();
    "##
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_output_uses_compact_json() {
        let output = text_output(json!({
            "ok": true,
            "nested": {
                "value": 1
            }
        }))
        .expect("serialize output");

        assert_eq!(output.into_text(), r#"{"nested":{"value":1},"ok":true}"#);
    }

    #[test]
    fn browser_stderr_tail_trims_on_character_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stderr.log");
        let stderr = format!("{}{}", "a".repeat(4_050), "é");
        fs::write(&path, stderr).expect("write stderr");

        let tail = browser_stderr_tail(&path);

        assert!(tail.starts_with("..."));
        assert!(tail.ends_with('é'));
        assert!(tail.len() > 4_000);
    }

    #[test]
    fn stealth_user_agent_removes_headless_marker() {
        let user_agent = "Mozilla/5.0 HeadlessChrome/120.0.0.0 Safari/537.36";

        assert_eq!(
            stealth_user_agent(user_agent),
            "Mozilla/5.0 Chrome/120.0.0.0 Safari/537.36"
        );
    }

    #[test]
    fn key_descriptor_maps_common_keys() {
        let enter = key_descriptor("Enter").expect("enter descriptor");
        assert_eq!(enter.key, "Enter");
        assert_eq!(enter.code, "Enter");
        assert_eq!(enter.windows_virtual_key_code, 13);

        let arrow_down = key_descriptor("down").expect("down descriptor");
        assert_eq!(arrow_down.key, "ArrowDown");
        assert_eq!(arrow_down.windows_virtual_key_code, 40);

        assert!(key_descriptor("ctrl+a").is_none());
    }

    #[test]
    fn browser_numeric_inputs_are_bounded() {
        assert_eq!(
            bounded_number(
                "delta_y",
                /*value*/ 600.0,
                /*min*/ -MAX_SCROLL_DELTA,
                /*max*/ MAX_SCROLL_DELTA,
            )
            .expect("default scroll delta"),
            600.0
        );
        assert!(
            bounded_number(
                "delta_y",
                /*value*/ 20_000.0,
                /*min*/ -MAX_SCROLL_DELTA,
                /*max*/ MAX_SCROLL_DELTA,
            )
            .is_err()
        );
        assert!(
            bounded_number(
                "x",
                /*value*/ f64::NAN,
                /*min*/ 0.0,
                /*max*/ 1280.0
            )
            .is_err()
        );
        assert!(positive_bounded_number("width", /*value*/ 0.0, /*max*/ 1280.0).is_err());
        assert!(positive_bounded_number("width", /*value*/ 120.0, /*max*/ 1280.0).is_ok());
        assert_eq!(
            remaining_viewport_extent("x", /*viewport*/ 1280, /*origin*/ 1200.0).unwrap(),
            80.0
        );
        assert!(remaining_viewport_extent("x", /*viewport*/ 1280, /*origin*/ 1280.0).is_err());
    }

    #[test]
    fn browser_access_policy_blocks_network_when_restricted() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let policy = BrowserAccessPolicy {
            network: NetworkSandboxPolicy::Restricted,
            file_system: codex_protocol::models::PermissionProfile::read_only()
                .file_system_sandbox_policy(),
            cwd: cwd.path().to_path_buf(),
        };

        assert!(policy.validate_browser_process(/*action*/ "open").is_err());
        assert!(
            policy
                .validate_page_url("https://example.com", /*action*/ "navigate")
                .is_err()
        );
        assert!(
            policy
                .validate_page_url(
                    "data:text/html;charset=utf-8,ok",
                    /*action*/ "navigate",
                )
                .is_ok()
        );
        assert!(
            policy
                .validate_page_url(
                    Url::from_file_path(cwd.path().join("fixture.html"))
                        .expect("file URL")
                        .as_str(),
                    /*action*/ "navigate"
                )
                .is_ok()
        );
    }

    #[test]
    fn browser_engine_serializes_as_tool_output_backend() {
        assert_eq!(json!(BrowserEngine::Obscura), json!("obscura"));
        assert_eq!(json!(BrowserEngine::Chromium), json!("chromium"));
        assert_eq!(json!(BrowserEngine::ExternalCdp), json!("external_cdp"));
    }

    #[test]
    fn browser_share_roundtrips_local_metadata() {
        let share = BrowserShare {
            share_id: format!("bs-test-{}", Uuid::new_v4().simple()),
            access: ShareAccess::ReadOnly,
            engine: BrowserEngine::ExternalCdp,
            mode: "headless".to_string(),
            endpoint: "http://127.0.0.1:9222".to_string(),
            page_ws_url: "ws://127.0.0.1:9222/devtools/page/1".to_string(),
            target_id: Some("target-1".to_string()),
            viewport_width: 1280,
            viewport_height: 800,
            stealth: true,
            created_at_unix_ms: 1,
        };

        let path = write_browser_share(&share).expect("write browser share");
        let loaded = read_browser_share(&share.share_id).expect("read browser share");

        assert!(path.ends_with(format!("{}.json", share.share_id)));
        assert_eq!(loaded.share_id, share.share_id);
        assert_eq!(loaded.access, ShareAccess::ReadOnly);
        assert_eq!(loaded.target_id.as_deref(), Some("target-1"));
    }

    #[test]
    fn obscura_binary_candidates_include_bundled_locations() {
        let candidates = obscura_binary_candidates_for_exe(Path::new(
            "/Applications/Codex.app/Contents/MacOS/codex",
        ));
        assert!(
            candidates
                .iter()
                .any(|path| path.ends_with("Contents/MacOS/obscura"))
        );
        #[cfg(target_os = "macos")]
        assert!(
            candidates
                .iter()
                .any(|path| path.ends_with("Contents/Resources/obscura"))
        );
    }

    #[test]
    fn obscura_dom_renderer_outputs_png() {
        let snapshot = json!({
            "url": "https://example.test/",
            "title": "Example",
            "text": "Example page\nRun Search Details",
            "viewport": { "width": 800, "height": 600 },
            "elements": [
                {
                    "ref": "e1",
                    "tag": "button",
                    "label": "Run",
                    "rect": { "x": 20, "y": 80, "width": 120, "height": 40 }
                }
            ]
        });
        let png = agent_browser_visual::render_snapshot_png(
            &snapshot, /*viewport_width*/ 800, /*viewport_height*/ 600,
            /*full_page*/ false,
        )
        .expect("render png snapshot");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(png.len() > 512);
        let html = agent_browser_visual::visual_shell_html(
            &snapshot, /*viewport_width*/ 800, /*viewport_height*/ 600,
        );
        assert!(html.contains("Obscura headful mirror"));
        assert!(html.contains("button e1 Run"));
    }

    #[tokio::test]
    #[ignore = "requires CODEX_AGENT_BROWSER_RUN_CHROME_TESTS=1 and a local Chrome/Chromium binary"]
    async fn headless_benchmark_launches_browser() {
        if std::env::var("CODEX_AGENT_BROWSER_RUN_CHROME_TESTS").as_deref() != Ok("1") {
            return;
        }

        let iterations = std::env::var("CODEX_AGENT_BROWSER_BENCHMARK_ITERATIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1);
        let result = handle_benchmark(BenchmarkArgs {
            mode: BrowserMode::Headless,
            backend: BrowserBackend::Chromium,
            url: None,
            iterations: Some(iterations),
            stealth: true,
            remote_debugging_url: std::env::var("CODEX_AGENT_BROWSER_REMOTE_DEBUGGING_URL").ok(),
        })
        .await
        .expect("headless benchmark should complete");

        write_benchmark_output("CODEX_AGENT_BROWSER_BENCHMARK_OUTPUT", &result);
        assert_eq!(result.mode, "headless");
        assert!(result.launch_ms > 0.0);
        assert_eq!(result.target_url, benchmark_url());
        assert_eq!(result.snapshot_ms.len(), iterations);
        assert_eq!(result.screenshot_ms.len(), iterations);
        assert_eq!(result.screenshot_png_bytes.len(), iterations);
        assert_eq!(result.screenshot_base64_chars.len(), iterations);
        assert!(result.totals.screenshot_png_avg_bytes > 0.0);
    }

    #[tokio::test]
    #[ignore = "requires CODEX_AGENT_BROWSER_RUN_OBSCURA_TESTS=1 and an Obscura binary"]
    async fn obscura_benchmark_launches_browser() {
        if std::env::var("CODEX_AGENT_BROWSER_RUN_OBSCURA_TESTS").as_deref() != Ok("1") {
            return;
        }

        let iterations = std::env::var("CODEX_AGENT_BROWSER_BENCHMARK_ITERATIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1);
        let (_page_dir, page_url) = benchmark_file_url("codex-obscura-benchmark-");
        let expected_url = page_url.clone();
        let result = handle_benchmark(BenchmarkArgs {
            mode: BrowserMode::Headless,
            backend: BrowserBackend::Obscura,
            url: Some(page_url),
            iterations: Some(iterations),
            stealth: true,
            remote_debugging_url: None,
        })
        .await
        .expect("Obscura benchmark should complete");

        write_benchmark_output("CODEX_AGENT_BROWSER_OBSCURA_BENCHMARK_OUTPUT", &result);
        assert_eq!(result.mode, "headless");
        assert_eq!(result.backend, BrowserEngine::Obscura);
        assert_eq!(result.target_url, expected_url);
        assert!(result.launch_ms > 0.0);
        assert_eq!(result.snapshot_ms.len(), iterations);
        assert_eq!(result.screenshot_ms.len(), iterations);
        assert_eq!(result.screenshot_png_bytes.len(), iterations);
        assert_eq!(result.screenshot_base64_chars.len(), iterations);
        assert!(result.totals.screenshot_base64_avg_chars > 0.0);
    }

    #[tokio::test]
    #[ignore = "requires CODEX_AGENT_BROWSER_RUN_CHROME_TESTS=1 and a local Chrome/Chromium binary"]
    async fn headless_highlight_marks_and_clears_rect() {
        if std::env::var("CODEX_AGENT_BROWSER_RUN_CHROME_TESTS").as_deref() != Ok("1") {
            return;
        }

        let open = handle_open(OpenArgs {
            url: Some(benchmark_url()),
            share_id: None,
            mode: BrowserMode::Headless,
            stealth: true,
            backend: BrowserBackend::Chromium,
            viewport_width: Some(1280),
            viewport_height: Some(800),
            locale: Some(DEFAULT_LOCALE.to_string()),
            timezone: Some(DEFAULT_TIMEZONE.to_string()),
            user_agent: None,
            remote_debugging_url: std::env::var("CODEX_AGENT_BROWSER_REMOTE_DEBUGGING_URL").ok(),
        })
        .await
        .expect("open browser");
        let session_id = open.session_id;
        let snapshot = handle_snapshot(SnapshotArgs {
            session_id: Some(session_id.clone()),
            max_text_chars: None,
            max_elements: None,
        })
        .await
        .expect("snapshot page");
        let button_ref = snapshot
            .get("elements")
            .and_then(Value::as_array)
            .and_then(|elements| {
                elements.iter().find_map(|element| {
                    let is_run = element
                        .get("label")
                        .and_then(Value::as_str)
                        .is_some_and(|label| label == "Run");
                    if is_run {
                        element
                            .get("ref")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    } else {
                        None
                    }
                })
            })
            .expect("run button ref");
        let mut session = take_session(Some(session_id.clone()))
            .await
            .expect("take session for ref pollution check");
        let ref_property = evaluate_json(
            &mut session.cdp,
            r#"(() => ({
                hasRefProperty: Array.from(document.querySelectorAll("button")).some((el) => (
                    Object.prototype.hasOwnProperty.call(el, "__codexAgentRef")
                ))
            }))()"#,
        )
        .await
        .expect("check ref property");
        put_session(session).await;

        let marked = handle_highlight(HighlightArgs {
            session_id: Some(session_id.clone()),
            element_ref: Some(button_ref),
            x: None,
            y: None,
            width: None,
            height: None,
            label: Some("Button issue".to_string()),
            color: None,
            clear: None,
        })
        .await
        .expect("mark ref highlight");
        let marked_ref_count = marked
            .get("highlights")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or_default();

        let marked = handle_highlight(HighlightArgs {
            session_id: Some(session_id.clone()),
            element_ref: None,
            x: Some(12.0),
            y: Some(24.0),
            width: Some(120.0),
            height: Some(40.0),
            label: Some("Rect issue".to_string()),
            color: None,
            clear: None,
        })
        .await
        .expect("mark rect highlight");
        let marked_count = marked
            .get("highlights")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or_default();

        let cleared = handle_highlight(HighlightArgs {
            session_id: Some(session_id.clone()),
            element_ref: None,
            x: None,
            y: None,
            width: None,
            height: None,
            label: None,
            color: None,
            clear: Some(true),
        })
        .await
        .expect("clear highlight");
        let cleared_count = cleared
            .get("highlights")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or_default();

        handle_close(SessionArgs {
            session_id: Some(session_id),
        })
        .await
        .expect("close browser");

        assert_eq!(marked_ref_count, 1);
        assert_eq!(marked_count, 2);
        assert_eq!(cleared_count, 0);
        assert_eq!(
            ref_property.get("hasRefProperty").and_then(Value::as_bool),
            Some(false)
        );
    }

    #[tokio::test]
    #[ignore = "requires CODEX_AGENT_BROWSER_RUN_CHROME_TESTS=1 and a local Chrome/Chromium binary"]
    async fn shared_browser_attaches_read_only_to_live_page() {
        if std::env::var("CODEX_AGENT_BROWSER_RUN_CHROME_TESTS").as_deref() != Ok("1") {
            return;
        }

        let open = handle_open(OpenArgs {
            url: Some(benchmark_url()),
            share_id: None,
            mode: BrowserMode::Headless,
            stealth: true,
            backend: BrowserBackend::Chromium,
            viewport_width: Some(1280),
            viewport_height: Some(800),
            locale: Some(DEFAULT_LOCALE.to_string()),
            timezone: Some(DEFAULT_TIMEZONE.to_string()),
            user_agent: None,
            remote_debugging_url: std::env::var("CODEX_AGENT_BROWSER_REMOTE_DEBUGGING_URL").ok(),
        })
        .await
        .expect("open browser");
        let original_session_id = open.session_id;
        let share = handle_share(ShareArgs {
            session_id: Some(original_session_id.clone()),
            access: ShareAccess::ReadOnly,
        })
        .await
        .expect("share browser");

        let attached = handle_open(OpenArgs {
            url: None,
            share_id: Some(share.share_id),
            mode: BrowserMode::Headless,
            stealth: true,
            backend: BrowserBackend::Auto,
            viewport_width: None,
            viewport_height: None,
            locale: None,
            timezone: None,
            user_agent: None,
            remote_debugging_url: None,
        })
        .await
        .expect("attach shared browser");
        let shared_session_id = attached.session_id;
        let snapshot = handle_snapshot(SnapshotArgs {
            session_id: Some(shared_session_id.clone()),
            max_text_chars: Some(2_000),
            max_elements: Some(20),
        })
        .await
        .expect("snapshot shared browser");
        assert!(
            snapshot
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("Codex Agent Browser Benchmark"))
        );
        let mut original_session = take_session(Some(original_session_id.clone()))
            .await
            .expect("take original Obscura browser");
        let globals = evaluate_json(
            &mut original_session.cdp,
            r#"(() => ({
                hasNextRef: Object.prototype.hasOwnProperty.call(window, "__codexAgentBrowserNextRef"),
                hasElementRefs: Object.prototype.hasOwnProperty.call(window, "__codexAgentBrowserElementRefs"),
                hasRefElements: Object.prototype.hasOwnProperty.call(window, "__codexAgentBrowserRefElements")
            }))()"#,
        )
        .await
        .expect("read original page globals");
        put_session(original_session).await;
        assert_eq!(
            globals.get("hasNextRef").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            globals.get("hasElementRefs").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            globals.get("hasRefElements").and_then(Value::as_bool),
            Some(false)
        );

        let blocked = handle_navigate(NavigateArgs {
            session_id: Some(shared_session_id.clone()),
            url: "about:blank".to_string(),
        })
        .await
        .expect_err("read-only share should reject navigate");
        assert!(blocked.to_string().contains("read_only shared session"));

        handle_close(SessionArgs {
            session_id: Some(shared_session_id),
        })
        .await
        .expect("close shared browser");
        handle_close(SessionArgs {
            session_id: Some(original_session_id),
        })
        .await
        .expect("close original browser");
    }

    #[tokio::test]
    #[ignore = "requires CODEX_AGENT_BROWSER_RUN_OBSCURA_TESTS=1 and an Obscura binary"]
    async fn obscura_shared_browser_attaches_to_live_target() {
        if std::env::var("CODEX_AGENT_BROWSER_RUN_OBSCURA_TESTS").as_deref() != Ok("1") {
            return;
        }

        let (_page_dir, page_url) = benchmark_file_url("codex-obscura-share-");
        let open = handle_open(OpenArgs {
            url: Some(page_url),
            share_id: None,
            mode: BrowserMode::Headless,
            stealth: true,
            backend: BrowserBackend::Obscura,
            viewport_width: Some(800),
            viewport_height: Some(600),
            locale: Some(DEFAULT_LOCALE.to_string()),
            timezone: Some(DEFAULT_TIMEZONE.to_string()),
            user_agent: None,
            remote_debugging_url: None,
        })
        .await
        .expect("open Obscura browser");
        let original_session_id = open.session_id;
        assert_eq!(open.backend, BrowserEngine::Obscura);

        let share = handle_share(ShareArgs {
            session_id: Some(original_session_id.clone()),
            access: ShareAccess::ReadOnly,
        })
        .await
        .expect("share Obscura browser");
        let attached = handle_open(OpenArgs {
            url: None,
            share_id: Some(share.share_id),
            mode: BrowserMode::Headless,
            stealth: true,
            backend: BrowserBackend::Auto,
            viewport_width: None,
            viewport_height: None,
            locale: None,
            timezone: None,
            user_agent: None,
            remote_debugging_url: None,
        })
        .await
        .expect("attach shared Obscura browser");
        let shared_session_id = attached.session_id;
        assert_eq!(attached.backend, BrowserEngine::Obscura);

        let snapshot = handle_snapshot(SnapshotArgs {
            session_id: Some(shared_session_id.clone()),
            max_text_chars: Some(2_000),
            max_elements: Some(20),
        })
        .await
        .expect("snapshot shared Obscura browser");
        assert!(
            snapshot
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("Codex Agent Browser Benchmark"))
        );

        let blocked = handle_navigate(NavigateArgs {
            session_id: Some(shared_session_id.clone()),
            url: "about:blank".to_string(),
        })
        .await
        .expect_err("read-only Obscura share should reject navigate");
        assert!(blocked.to_string().contains("read_only shared session"));

        handle_close(SessionArgs {
            session_id: Some(shared_session_id),
        })
        .await
        .expect("close shared Obscura browser");
        handle_close(SessionArgs {
            session_id: Some(original_session_id),
        })
        .await
        .expect("close original Obscura browser");
    }

    #[tokio::test]
    #[ignore = "requires CODEX_AGENT_BROWSER_RUN_OBSCURA_TESTS=1 and an Obscura binary"]
    async fn obscura_backend_renders_dom_screenshot() {
        if std::env::var("CODEX_AGENT_BROWSER_RUN_OBSCURA_TESTS").as_deref() != Ok("1") {
            return;
        }

        let (_page_dir, page_url) = benchmark_file_url("codex-obscura-smoke-");

        let open = handle_open(OpenArgs {
            url: Some(page_url),
            share_id: None,
            mode: BrowserMode::Headless,
            stealth: true,
            backend: BrowserBackend::Obscura,
            viewport_width: Some(800),
            viewport_height: Some(600),
            locale: Some(DEFAULT_LOCALE.to_string()),
            timezone: Some(DEFAULT_TIMEZONE.to_string()),
            user_agent: None,
            remote_debugging_url: None,
        })
        .await
        .expect("open Obscura browser");
        let session_id = open.session_id;
        assert_eq!(open.backend, BrowserEngine::Obscura);

        let snapshot = handle_snapshot(SnapshotArgs {
            session_id: Some(session_id.clone()),
            max_text_chars: Some(4_000),
            max_elements: Some(20),
        })
        .await
        .expect("snapshot Obscura page");
        assert_eq!(
            snapshot.get("backend").and_then(Value::as_str),
            Some("obscura")
        );

        let screenshot = handle_screenshot(ScreenshotArgs {
            session_id: Some(session_id.clone()),
            full_page: Some(false),
        })
        .await
        .expect("render Obscura DOM screenshot");
        let has_png = screenshot.body.iter().any(|item| {
            matches!(
                item,
                FunctionCallOutputContentItem::InputImage { image_url, .. }
                    if image_url.starts_with("data:image/png;base64,")
            )
        });
        let screenshot_text = screenshot.into_text();
        assert!(screenshot_text.contains("obscura_dom_snapshot"));
        assert!(has_png);

        handle_close(SessionArgs {
            session_id: Some(session_id),
        })
        .await
        .expect("close Obscura browser");
    }

    fn benchmark_file_url(prefix: &str) -> (TempDir, String) {
        let page_dir = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("create benchmark page dir");
        let page_path = page_dir.path().join("benchmark.html");
        fs::write(&page_path, benchmark_html()).expect("write benchmark page");
        let page_url = format!("file://{}", page_path.display());
        (page_dir, page_url)
    }

    fn write_benchmark_output(env_var: &str, result: &BenchmarkResult) {
        if let Ok(path) = std::env::var(env_var) {
            fs::write(
                path,
                serde_json::to_string(result).expect("serialize benchmark"),
            )
            .expect("write benchmark output");
        }
    }
}
