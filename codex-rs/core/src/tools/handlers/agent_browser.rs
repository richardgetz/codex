use std::collections::HashMap;
use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use futures::SinkExt;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

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
const TOOL_BENCHMARK: &str = "benchmark";

const DEFAULT_VIEWPORT_WIDTH: u32 = 1365;
const DEFAULT_VIEWPORT_HEIGHT: u32 = 900;
const DEFAULT_LOCALE: &str = "en-US";
const DEFAULT_TIMEZONE: &str = "America/New_York";
const CDP_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const BROWSER_HTTP_TIMEOUT: Duration = Duration::from_secs(5);

static BROWSER_MANAGER: OnceLock<Mutex<BrowserManager>> = OnceLock::new();
static BROWSER_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

pub struct AgentBrowserHandler;

impl ToolHandler for AgentBrowserHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        !matches!(
            invocation.tool_name.name.as_str(),
            TOOL_SNAPSHOT | TOOL_SCREENSHOT
        )
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

        match invocation.tool_name.name.as_str() {
            TOOL_OPEN => text_output(handle_open(parse_arguments(&arguments)?).await?),
            TOOL_CLOSE => text_output(handle_close(parse_arguments(&arguments)?).await?),
            TOOL_NAVIGATE => text_output(handle_navigate(parse_arguments(&arguments)?).await?),
            TOOL_SNAPSHOT => text_output(handle_snapshot(parse_arguments(&arguments)?).await?),
            TOOL_SCREENSHOT => handle_screenshot(parse_arguments(&arguments)?).await,
            TOOL_CLICK => text_output(handle_click(parse_arguments(&arguments)?).await?),
            TOOL_TYPE => text_output(handle_type(parse_arguments(&arguments)?).await?),
            TOOL_PRESS => text_output(handle_press(parse_arguments(&arguments)?).await?),
            TOOL_SCROLL => text_output(handle_scroll(parse_arguments(&arguments)?).await?),
            TOOL_SELECTION => text_output(handle_selection(parse_arguments(&arguments)?).await?),
            TOOL_HIGHLIGHT => text_output(handle_highlight(parse_arguments(&arguments)?).await?),
            TOOL_BENCHMARK => text_output(handle_benchmark(parse_arguments(&arguments)?).await?),
            other => Err(FunctionCallError::RespondToModel(format!(
                "unknown agent_browser tool `{other}`"
            ))),
        }
    }
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

#[derive(Debug, Deserialize)]
struct OpenArgs {
    url: Option<String>,
    #[serde(default)]
    mode: BrowserMode,
    #[serde(default = "default_true")]
    stealth: bool,
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

#[derive(Debug, Deserialize)]
struct BenchmarkArgs {
    #[serde(default = "default_headless")]
    mode: BrowserMode,
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

#[derive(Debug, Serialize)]
struct OpenResult {
    session_id: String,
    mode: &'static str,
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
struct BenchmarkResult {
    mode: &'static str,
    stealth: bool,
    iterations: usize,
    launch_ms: f64,
    navigate_ms: f64,
    snapshot_ms: Vec<f64>,
    screenshot_ms: Vec<f64>,
    totals: BenchmarkTotals,
}

#[derive(Debug, Serialize)]
struct BenchmarkTotals {
    snapshot_avg_ms: f64,
    screenshot_avg_ms: f64,
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
    stealth: bool,
    cdp: CdpClient,
    process: Option<Child>,
    owned_page_close_url: Option<String>,
    _profile_dir: Option<TempDir>,
    overlay_script_registered: bool,
}

struct CdpClient {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl CdpClient {
    async fn connect(ws_url: &str) -> Result<Self, FunctionCallError> {
        let (socket, _) = connect_async(ws_url).await.map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to connect to browser websocket `{ws_url}`: {err}"
            ))
        })?;
        Ok(Self { socket, next_id: 1 })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, FunctionCallError> {
        timeout(CDP_CALL_TIMEOUT, self.call_inner(method, params))
            .await
            .map_err(|_| {
                FunctionCallError::RespondToModel(format!(
                    "browser command `{method}` timed out after {}s",
                    CDP_CALL_TIMEOUT.as_secs()
                ))
            })?
    }

    async fn call_inner(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, FunctionCallError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({
            "id": id,
            "method": method,
            "params": params,
        });

        self.socket
            .send(Message::Text(request.to_string().into()))
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to send browser command `{method}`: {err}"
                ))
            })?;

        while let Some(message) = self.socket.next().await {
            let message = message.map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "browser websocket failed while waiting for `{method}`: {err}"
                ))
            })?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "browser returned invalid JSON for `{method}`: {err}"
                ))
            })?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(FunctionCallError::RespondToModel(format!(
                    "browser command `{method}` failed: {error}"
                )));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }

        Err(FunctionCallError::RespondToModel(format!(
            "browser websocket closed while waiting for `{method}`"
        )))
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
    let viewport_width = args.viewport_width.unwrap_or(DEFAULT_VIEWPORT_WIDTH);
    let viewport_height = args.viewport_height.unwrap_or(DEFAULT_VIEWPORT_HEIGHT);
    let locale = args.locale.unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let timezone = args
        .timezone
        .unwrap_or_else(|| DEFAULT_TIMEZONE.to_string());

    let mut launch = if let Some(remote) = args.remote_debugging_url.as_deref() {
        attach_connection(remote).await?
    } else {
        launch_connection(
            &args.mode,
            args.stealth,
            viewport_width,
            viewport_height,
            &locale,
        )
        .await?
    };

    let endpoint = launch.endpoint.clone();
    let page_ws_url = launch.page_ws_url.clone();
    let notes = launch.notes.clone();

    let mut cdp = match CdpClient::connect(&page_ws_url).await {
        Ok(cdp) => cdp,
        Err(err) => {
            cleanup_launch(&mut launch).await;
            return Err(err);
        }
    };
    if let Err(err) = initialize_page(
        &mut cdp,
        args.stealth,
        viewport_width,
        viewport_height,
        &locale,
        &timezone,
        args.user_agent.as_deref(),
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
    let session = BrowserSession {
        id: session_id.clone(),
        mode: args.mode.clone(),
        stealth: args.stealth,
        cdp,
        process: launch.process.take(),
        owned_page_close_url: launch.owned_page_close_url.take(),
        _profile_dir: launch.profile_dir.take(),
        overlay_script_registered: false,
    };

    let mut manager = manager().lock().await;
    manager.active_session_id = Some(session_id.clone());
    manager.sessions.insert(session_id.clone(), session);

    Ok(OpenResult {
        session_id,
        mode: mode_name(&args.mode),
        stealth: args.stealth,
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
    let result = navigate_to(&mut session.cdp, &args.url)
        .await
        .map(|_| SimpleResult {
            ok: true,
            session_id,
            message: "navigated".to_string(),
            elapsed_ms: Some(elapsed_ms(started)),
        });
    put_session(session).await;
    result
}

async fn handle_snapshot(args: SnapshotArgs) -> Result<Value, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let result = snapshot_page(
        &mut session.cdp,
        args.max_text_chars.unwrap_or(12_000).clamp(1_000, 80_000),
        args.max_elements.unwrap_or(80).clamp(1, 250),
    )
    .await
    .map(|mut snapshot| {
        snapshot["session_id"] = Value::String(session.id.clone());
        snapshot["mode"] = Value::String(mode_name(&session.mode).to_string());
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
    let result = session
        .cdp
        .call(
            "Page.captureScreenshot",
            json!({
                "format": "png",
                "fromSurface": true,
                "captureBeyondViewport": args.full_page.unwrap_or(false),
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
                Some(true),
            ))
        });
    put_session(session).await;
    result
}

async fn handle_click(args: ClickArgs) -> Result<SimpleResult, FunctionCallError> {
    let started = Instant::now();
    let mut session = take_session(args.session_id).await?;
    let session_id = session.id.clone();

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
        let x = args.x.ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "click requires either `ref` or both `x` and `y`".to_string(),
            )
        });
        let y = args.y.ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "click requires either `ref` or both `x` and `y`".to_string(),
            )
        });
        match (x, y) {
            (Ok(x), Ok(y)) => dispatch_click(&mut session.cdp, x, y).await,
            (Err(err), _) | (_, Err(err)) => Err(err),
        }
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
    let expression = format!(
        "(() => {{ window.scrollBy({}, {}); return {{ ok: true, x: window.scrollX, y: window.scrollY }}; }})()",
        args.delta_x.unwrap_or(0.0),
        args.delta_y.unwrap_or(600.0)
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

async fn handle_benchmark(args: BenchmarkArgs) -> Result<BenchmarkResult, FunctionCallError> {
    let iterations = args.iterations.unwrap_or(3).clamp(1, 20);
    let open_started = Instant::now();
    let open = OpenArgs {
        url: None,
        mode: args.mode.clone(),
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
        let navigate_started = Instant::now();
        handle_navigate(NavigateArgs {
            session_id: Some(session_id.clone()),
            url: benchmark_url(),
        })
        .await?;
        let navigate_ms = elapsed_ms(navigate_started);
        let mut snapshot_ms = Vec::with_capacity(iterations);
        let mut screenshot_ms = Vec::with_capacity(iterations);

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
            let _ = handle_screenshot(ScreenshotArgs {
                session_id: Some(session_id.clone()),
                full_page: Some(false),
            })
            .await?;
            screenshot_ms.push(elapsed_ms(started));
        }

        Ok(BenchmarkResult {
            mode: mode_name(&args.mode),
            stealth: args.stealth,
            iterations,
            launch_ms,
            navigate_ms,
            totals: BenchmarkTotals {
                snapshot_avg_ms: average_ms(&snapshot_ms),
                screenshot_avg_ms: average_ms(&screenshot_ms),
            },
            snapshot_ms,
            screenshot_ms,
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

async fn initialize_page(
    cdp: &mut CdpClient,
    stealth: bool,
    viewport_width: u32,
    viewport_height: u32,
    locale: &str,
    timezone: &str,
    user_agent: Option<&str>,
) -> Result<(), FunctionCallError> {
    cdp.call("Page.enable", json!({})).await?;
    cdp.call("Runtime.enable", json!({})).await?;
    cdp.call(
        "Emulation.setDeviceMetricsOverride",
        json!({
            "width": viewport_width,
            "height": viewport_height,
            "deviceScaleFactor": 1,
            "mobile": false,
        }),
    )
    .await?;
    cdp.call(
        "Emulation.setLocaleOverride",
        json!({
            "locale": locale,
        }),
    )
    .await?;
    cdp.call(
        "Emulation.setTimezoneOverride",
        json!({
            "timezoneId": timezone,
        }),
    )
    .await?;
    let user_agent_override = if let Some(user_agent) = user_agent {
        Some(user_agent.to_string())
    } else if stealth {
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
                "acceptLanguage": locale,
            }),
        )
        .await?;
    }
    if stealth {
        let script = stealth_script(locale);
        cdp.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": script }),
        )
        .await?;
        evaluate_json(
            cdp,
            &format!("(() => {{ {script} return {{ ok: true }}; }})()"),
        )
        .await?;
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
) -> Result<Value, FunctionCallError> {
    let expression = format!(
        r#"(() => {{
            const maxText = {max_text_chars};
            const maxElements = {max_elements};
            window.__codexAgentBrowserNextRef = window.__codexAgentBrowserNextRef || 1;
            window.__codexAgentBrowserRefElements = new Map();
            const refFor = (el) => {{
                if (!el.__codexAgentRef) {{
                    Object.defineProperty(el, "__codexAgentRef", {{
                        value: "e" + window.__codexAgentBrowserNextRef++,
                        configurable: true
                    }});
                }}
                window.__codexAgentBrowserRefElements.set(el.__codexAgentRef, el);
                return el.__codexAgentRef;
            }};
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

async fn inject_overlay(cdp: &mut CdpClient) -> Result<(), FunctionCallError> {
    cdp.call(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": overlay_script() }),
    )
    .await?;
    evaluate_json(
        cdp,
        &format!(
            "(() => {{ {} return window.__codexAgentBrowserOverview(); }})()",
            overlay_script()
        ),
    )
    .await?;
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
        endpoint,
        page_ws_url: page.ws_url,
        process: None,
        profile_dir: None,
        owned_page_close_url: page.owned_close_url,
        notes: vec!["attached to existing remote debugging endpoint".to_string()],
    })
}

async fn launch_connection(
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

fn benchmark_url() -> String {
    let html = r##"<!doctype html>
<meta charset="utf-8">
<title>Codex Agent Browser Benchmark</title>
<main>
  <h1>Codex Agent Browser Benchmark</h1>
  <input aria-label="Search" placeholder="Search">
  <button>Run</button>
  <a href="#details">Details</a>
  <section id="details">This local page measures launch, navigation, snapshot, and screenshot latency.</section>
</main>"##;
    format!("data:text/html;charset=utf-8,{}", urlencoding_like(html))
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
            iterations: Some(iterations),
            stealth: true,
            remote_debugging_url: std::env::var("CODEX_AGENT_BROWSER_REMOTE_DEBUGGING_URL").ok(),
        })
        .await
        .expect("headless benchmark should complete");

        if let Ok(path) = std::env::var("CODEX_AGENT_BROWSER_BENCHMARK_OUTPUT") {
            fs::write(
                path,
                serde_json::to_string(&result).expect("serialize benchmark"),
            )
            .expect("write benchmark output");
        }
        assert_eq!(result.mode, "headless");
        assert!(result.launch_ms > 0.0);
        assert_eq!(result.snapshot_ms.len(), iterations);
        assert_eq!(result.screenshot_ms.len(), iterations);
    }

    #[tokio::test]
    #[ignore = "requires CODEX_AGENT_BROWSER_RUN_CHROME_TESTS=1 and a local Chrome/Chromium binary"]
    async fn headless_highlight_marks_and_clears_rect() {
        if std::env::var("CODEX_AGENT_BROWSER_RUN_CHROME_TESTS").as_deref() != Ok("1") {
            return;
        }

        let open = handle_open(OpenArgs {
            url: Some(benchmark_url()),
            mode: BrowserMode::Headless,
            stealth: true,
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
    }
}
