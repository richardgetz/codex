use std::cell::RefCell;
use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use crossbeam_channel::bounded;
use objc2_app_kit::NSWindow;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use tao::dpi::LogicalSize;
use tao::event::Event;
use tao::event::WindowEvent;
use tao::event_loop::ControlFlow;
use tao::event_loop::EventLoop;
use tao::event_loop::EventLoopBuilder;
use tao::event_loop::EventLoopProxy;
use tao::platform::macos::WindowExtMacOS;
use tao::window::Window;
use tao::window::WindowBuilder;
use tiny_http::Header;
use tiny_http::Request;
use tiny_http::Response;
use tiny_http::Server;
use wry::WebView;
use wry::WebViewBuilder;

const DEFAULT_WIDTH: u32 = 1200;
const DEFAULT_HEIGHT: u32 = 900;

#[derive(Debug, Deserialize)]
struct Args {
    url: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    user_agent: Option<String>,
}

enum UiCommand {
    Navigate {
        url: String,
        response: crossbeam_channel::Sender<Result<Value, String>>,
    },
    Eval {
        script: String,
        response: crossbeam_channel::Sender<Result<Value, String>>,
    },
    Close {
        response: crossbeam_channel::Sender<Result<Value, String>>,
    },
}

pub(crate) fn run() -> Result<()> {
    let args = parse_args()?;
    let width = args.width.unwrap_or(DEFAULT_WIDTH).clamp(320, 7680);
    let height = args.height.unwrap_or(DEFAULT_HEIGHT).clamp(240, 4320);
    let event_loop: EventLoop<UiCommand> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("Codex Agent Browser")
        .with_inner_size(LogicalSize::new(f64::from(width), f64::from(height)))
        .build(&event_loop)
        .context("create WRY window")?;
    let window_number = window_number(&window);

    let server = ControlServer::start(proxy, window_number).context("start WRY control server")?;
    let endpoint = server.endpoint;

    let mut builder = WebViewBuilder::new().with_initialization_script(INIT_SCRIPT);
    if let Some(user_agent) = args.user_agent {
        builder = builder.with_user_agent(&user_agent);
    }
    if let Some(url) = args.url {
        builder = builder.with_url(&url);
    } else {
        builder = builder.with_html("<!doctype html><title>Codex Agent Browser</title>");
    }
    let webview = builder.build(&window).context("create WRY WebView")?;
    let webview = Rc::new(RefCell::new(Some(webview)));

    println!(
        "{}",
        json!({
            "endpoint": endpoint,
            "backend": "wry",
        })
    );

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(command) => {
                handle_ui_command(command, &webview, control_flow);
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    })
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        url: None,
        width: None,
        height: None,
        user_agent: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--url" => args.url = iter.next(),
            "--width" => args.width = iter.next().and_then(|value| value.parse().ok()),
            "--height" => args.height = iter.next().and_then(|value| value.parse().ok()),
            "--user-agent" => args.user_agent = iter.next(),
            other => anyhow::bail!("unknown argument `{other}`"),
        }
    }
    Ok(args)
}

fn handle_ui_command(
    command: UiCommand,
    webview: &Rc<RefCell<Option<WebView>>>,
    control_flow: &mut ControlFlow,
) {
    match command {
        UiCommand::Navigate { url, response } => {
            let result = with_webview(webview, |webview| {
                webview
                    .load_url(&url)
                    .map(|_| json!({ "ok": true, "url": url }))
                    .map_err(|err| err.to_string())
            });
            let _ = response.send(result);
        }
        UiCommand::Eval { script, response } => {
            let response_for_callback = response.clone();
            let result = with_webview(webview, |webview| {
                webview
                    .evaluate_script_with_callback(&script, move |value| {
                        let parsed =
                            serde_json::from_str::<Value>(&value).unwrap_or(Value::String(value));
                        let _ = response_for_callback.send(Ok(parsed));
                    })
                    .map_err(|err| err.to_string())
            });
            if let Err(err) = result {
                let _ = response.send(Err(err));
            }
        }
        UiCommand::Close { response } => {
            webview.borrow_mut().take();
            let _ = response.send(Ok(json!({ "ok": true })));
            *control_flow = ControlFlow::Exit;
        }
    }
}

fn with_webview<T>(
    webview: &Rc<RefCell<Option<WebView>>>,
    f: impl FnOnce(&WebView) -> Result<T, String>,
) -> Result<T, String> {
    let guard = webview.borrow();
    let webview = guard
        .as_ref()
        .ok_or_else(|| "WRY webview is closed".to_string())?;
    f(webview)
}

fn window_number(window: &Window) -> Option<i64> {
    let ns_window = window.ns_window();
    if ns_window.is_null() {
        return None;
    }
    let ns_window = ns_window.cast::<NSWindow>();
    // SAFETY: tao returns the NSWindow pointer owned by this live Window. We only
    // read the immutable windowNumber value immediately after the Window is built.
    Some(unsafe { (*ns_window).windowNumber() } as i64)
}

struct ControlServer {
    endpoint: String,
}

impl ControlServer {
    fn start(proxy: EventLoopProxy<UiCommand>, window_number: Option<i64>) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        let server = Server::from_listener(listener, None)
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        let endpoint = format!("http://{addr}");
        thread::spawn(move || {
            for request in server.incoming_requests() {
                handle_request(request, &proxy, window_number);
            }
        });
        Ok(Self { endpoint })
    }
}

fn handle_request(
    mut request: Request,
    proxy: &EventLoopProxy<UiCommand>,
    window_number: Option<i64>,
) {
    let response = match (request.method().as_str(), request.url()) {
        ("GET", "/status") => Ok(json!({ "ok": true, "backend": "wry" })),
        ("POST", "/navigate") => {
            let body = read_json_body(&mut request);
            body.and_then(|body| {
                let url = body
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "missing `url`".to_string())?;
                send_command(proxy, UiCommandRequest::Navigate(url.to_string()))
            })
        }
        ("POST", "/eval") => {
            let body = read_json_body(&mut request);
            body.and_then(|body| {
                let script = body
                    .get("script")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "missing `script`".to_string())?;
                send_command(proxy, UiCommandRequest::Eval(script.to_string()))
            })
        }
        ("POST", "/snapshot") => {
            let body = read_json_body(&mut request);
            body.and_then(|body| {
                send_command(proxy, UiCommandRequest::Eval(snapshot_script(&body)))
            })
        }
        ("POST", "/screenshot") => capture_window_png_data_url(window_number),
        ("POST", "/selection") => send_command(proxy, UiCommandRequest::Eval(selection_script())),
        ("POST", "/click") => {
            let body = read_json_body(&mut request);
            body.and_then(|body| send_command(proxy, UiCommandRequest::Eval(click_script(&body))))
        }
        ("POST", "/type") => {
            let body = read_json_body(&mut request);
            body.and_then(|body| send_command(proxy, UiCommandRequest::Eval(type_script(&body))))
        }
        ("POST", "/press") => {
            let body = read_json_body(&mut request);
            body.and_then(|body| send_command(proxy, UiCommandRequest::Eval(press_script(&body))))
        }
        ("POST", "/scroll") => {
            let body = read_json_body(&mut request);
            body.and_then(|body| send_command(proxy, UiCommandRequest::Eval(scroll_script(&body))))
        }
        ("POST", "/highlight") => {
            let body = read_json_body(&mut request);
            body.and_then(|body| {
                send_command(proxy, UiCommandRequest::Eval(highlight_script(&body)))
            })
        }
        ("POST", "/close") => send_command(proxy, UiCommandRequest::Close),
        _ => Err("not found".to_string()),
    };
    let (status, body) = match response {
        Ok(value) => (200, value),
        Err(message) => (500, json!({ "ok": false, "error": message })),
    };
    let mut http_response = Response::from_string(body.to_string()).with_status_code(status);
    if let Ok(header) = Header::from_bytes("content-type", "application/json") {
        http_response.add_header(header);
    }
    let _ = request.respond(http_response);
}

enum UiCommandRequest {
    Navigate(String),
    Eval(String),
    Close,
}

fn send_command(
    proxy: &EventLoopProxy<UiCommand>,
    request: UiCommandRequest,
) -> Result<Value, String> {
    let (tx, rx) = bounded(1);
    let command = match request {
        UiCommandRequest::Navigate(url) => UiCommand::Navigate { url, response: tx },
        UiCommandRequest::Eval(script) => UiCommand::Eval {
            script,
            response: tx,
        },
        UiCommandRequest::Close => UiCommand::Close { response: tx },
    };
    proxy
        .send_event(command)
        .map_err(|_| "WRY event loop is closed".to_string())?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| "timed out waiting for WRY response".to_string())?
}

fn read_json_body(request: &mut Request) -> Result<Value, String> {
    let mut body = String::new();
    request
        .as_reader()
        .take(1_000_000)
        .read_to_string(&mut body)
        .map_err(|err| err.to_string())?;
    serde_json::from_str(&body).map_err(|err| err.to_string())
}

fn capture_window_png_data_url(window_number: Option<i64>) -> Result<Value, String> {
    let window_number = window_number.ok_or_else(|| "WRY window id is unavailable".to_string())?;
    let path = temporary_screenshot_path();
    let status = Command::new("/usr/sbin/screencapture")
        .arg("-x")
        .arg("-l")
        .arg(window_number.to_string())
        .arg(&path)
        .status()
        .map_err(|err| format!("failed to run screencapture: {err}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(format!("screencapture failed with status {status}"));
    }
    let png = std::fs::read(&path).map_err(|err| format!("failed to read screenshot: {err}"))?;
    let _ = std::fs::remove_file(&path);
    let mut image_url = String::with_capacity("data:image/png;base64,".len() + png.len() * 4 / 3);
    image_url.push_str("data:image/png;base64,");
    BASE64_STANDARD.encode_string(png, &mut image_url);
    Ok(json!({
        "ok": true,
        "mimeType": "image/png",
        "imageUrl": image_url,
    }))
}

fn temporary_screenshot_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "codex-agent-browser-wry-{}-{nanos}.png",
        std::process::id()
    ))
}

fn json_literal(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn snapshot_script(body: &Value) -> String {
    let max_text_chars = body
        .get("maxTextChars")
        .and_then(Value::as_u64)
        .unwrap_or(12_000)
        .clamp(1_000, 80_000);
    let max_elements = body
        .get("maxElements")
        .and_then(Value::as_u64)
        .unwrap_or(80)
        .clamp(1, 250);
    let action_refs = body
        .get("actionRefs")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    format!(
        r#"(() => {{
            {REF_SCRIPT}
            return window.__codexWrySnapshot({max_text_chars}, {max_elements}, {action_refs});
        }})()"#
    )
}

fn selection_script() -> String {
    r#"(() => {
        const selection = window.getSelection();
        const text = selection ? selection.toString().slice(0, 4000) : "";
        const range = selection && selection.rangeCount ? selection.getRangeAt(0) : null;
        const rect = range ? range.getBoundingClientRect() : null;
        return {
            overlay: Boolean(window.__codexWryOverlay),
            hasSelection: Boolean(text),
            selection: text,
            rect: rect ? {
                x: Math.round(rect.x),
                y: Math.round(rect.y),
                width: Math.round(rect.width),
                height: Math.round(rect.height)
            } : null
        };
    })()"#
        .to_string()
}

fn click_script(body: &Value) -> String {
    let ref_id = body
        .get("ref")
        .and_then(Value::as_str)
        .map(json_literal)
        .unwrap_or_else(|| "null".to_string());
    let x = body.get("x").and_then(Value::as_f64).unwrap_or(-1.0);
    let y = body.get("y").and_then(Value::as_f64).unwrap_or(-1.0);
    format!(
        r#"(() => {{
            {REF_SCRIPT}
            window.__codexWryEnsureActionRefs();
            const refId = {ref_id};
            let el = refId ? window.__codexWryRefElements.get(refId) : document.elementFromPoint({x}, {y});
            if (!el) return {{ ok: false, message: "element not found" }};
            const rect = el.getBoundingClientRect();
            const cx = refId ? rect.left + rect.width / 2 : {x};
            const cy = refId ? rect.top + rect.height / 2 : {y};
            for (const type of ["pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {{
                el.dispatchEvent(new MouseEvent(type, {{ bubbles: true, cancelable: true, clientX: cx, clientY: cy, view: window }}));
            }}
            if (el.focus) el.focus();
            return {{ ok: true }};
        }})()"#
    )
}

fn type_script(body: &Value) -> String {
    let ref_id = body
        .get("ref")
        .and_then(Value::as_str)
        .map(json_literal)
        .unwrap_or_else(|| "null".to_string());
    let text = json_literal(body.get("text").and_then(Value::as_str).unwrap_or(""));
    let clear = body.get("clear").and_then(Value::as_bool).unwrap_or(false);
    format!(
        r#"(() => {{
            {REF_SCRIPT}
            window.__codexWryEnsureActionRefs();
            const refId = {ref_id};
            const el = refId ? window.__codexWryRefElements.get(refId) : document.activeElement;
            if (!el) return {{ ok: false, message: "element not found" }};
            if (el.focus) el.focus();
            if ({clear}) el.value = "";
            const text = {text};
            if ("value" in el) {{
                el.value = (el.value || "") + text;
                el.dispatchEvent(new Event("input", {{ bubbles: true }}));
                el.dispatchEvent(new Event("change", {{ bubbles: true }}));
            }} else {{
                document.execCommand("insertText", false, text);
            }}
            return {{ ok: true }};
        }})()"#
    )
}

fn press_script(body: &Value) -> String {
    let key = json_literal(body.get("key").and_then(Value::as_str).unwrap_or(""));
    format!(
        r#"(() => {{
            const key = {key};
            const target = document.activeElement || document.body;
            for (const type of ["keydown", "keyup"]) {{
                target.dispatchEvent(new KeyboardEvent(type, {{ bubbles: true, cancelable: true, key }}));
            }}
            return {{ ok: true }};
        }})()"#
    )
}

fn scroll_script(body: &Value) -> String {
    let x = body.get("deltaX").and_then(Value::as_f64).unwrap_or(0.0);
    let y = body.get("deltaY").and_then(Value::as_f64).unwrap_or(0.0);
    format!(
        r#"(() => {{
            window.scrollBy({x}, {y});
            return {{ ok: true, scrollX: window.scrollX, scrollY: window.scrollY }};
        }})()"#
    )
}

fn highlight_script(body: &Value) -> String {
    let clear = body.get("clear").and_then(Value::as_bool).unwrap_or(false);
    let ref_id = body
        .get("ref")
        .and_then(Value::as_str)
        .map(json_literal)
        .unwrap_or_else(|| "null".to_string());
    let label = json_literal(body.get("label").and_then(Value::as_str).unwrap_or("Codex"));
    let color = json_literal(
        body.get("color")
            .and_then(Value::as_str)
            .unwrap_or("#d93025"),
    );
    let x = body.get("x").and_then(Value::as_f64).unwrap_or(-1.0);
    let y = body.get("y").and_then(Value::as_f64).unwrap_or(-1.0);
    let width = body.get("width").and_then(Value::as_f64).unwrap_or(120.0);
    let height = body.get("height").and_then(Value::as_f64).unwrap_or(32.0);
    format!(
        r#"(() => {{
            {REF_SCRIPT}
            window.__codexWryEnsureActionRefs();
            if ({clear}) {{
                document.querySelectorAll("[data-codex-wry-highlight]").forEach((node) => node.remove());
                return {{ ok: true, highlights: 0 }};
            }}
            const refId = {ref_id};
            let rect = refId && window.__codexWryRefElements.get(refId)
                ? window.__codexWryRefElements.get(refId).getBoundingClientRect()
                : {{ x: {x}, y: {y}, width: {width}, height: {height} }};
            const node = document.createElement("div");
            node.setAttribute("data-codex-wry-highlight", "");
            node.style.cssText = `position:fixed;z-index:2147483647;left:${{Math.max(0, rect.x)}}px;top:${{Math.max(0, rect.y)}}px;width:${{Math.max(1, rect.width)}}px;height:${{Math.max(1, rect.height)}}px;border:3px solid {color};background:color-mix(in srgb, {color} 14%, transparent);pointer-events:none;box-sizing:border-box`;
            const tag = document.createElement("div");
            tag.textContent = {label};
            tag.style.cssText = `position:absolute;left:-3px;top:-24px;max-width:360px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;background:{color};color:white;font:12px -apple-system,BlinkMacSystemFont,Segoe UI,sans-serif;padding:2px 6px`;
            node.appendChild(tag);
            document.documentElement.appendChild(node);
            return {{ ok: true, highlights: document.querySelectorAll("[data-codex-wry-highlight]").length }};
        }})()"#
    )
}

const INIT_SCRIPT: &str = r#"
window.__codexWryRefElements = new Map();
window.__codexWryElementRefs = new WeakMap();
window.__codexWryNextRef = 1;
window.__codexWryOverlay = true;
"#;

const REF_SCRIPT: &str = r#"
window.__codexWryEnsureActionRefs = () => {
  if (!window.__codexWryRefElements) window.__codexWryRefElements = new Map();
  if (!window.__codexWryElementRefs) window.__codexWryElementRefs = new WeakMap();
  if (!window.__codexWryNextRef) window.__codexWryNextRef = 1;
};
window.__codexWrySnapshot = (maxText, maxElements, actionRefs) => {
  const readOnlyRefs = new Map();
  if (actionRefs) {
    window.__codexWryEnsureActionRefs();
    window.__codexWryRefElements = new Map();
  }
  const pickLabel = (el) => (el.getAttribute("aria-label") || el.getAttribute("title") || el.alt || el.innerText || el.value || el.placeholder || "").trim().replace(/\s+/g, " ").slice(0, 220);
  const refFor = (el) => {
    if (!actionRefs) {
      let ref = readOnlyRefs.get(el);
      if (!ref) {
        ref = `e${readOnlyRefs.size + 1}`;
        readOnlyRefs.set(el, ref);
      }
      return ref;
    }
    let ref = window.__codexWryElementRefs.get(el);
    if (!ref) {
      ref = `e${window.__codexWryNextRef++}`;
      window.__codexWryElementRefs.set(el, ref);
    }
    window.__codexWryRefElements.set(ref, el);
    return ref;
  };
  const candidates = Array.from(document.querySelectorAll("a[href],button,input,textarea,select,[role=button],[role=link],[tabindex],img")).filter((el) => {
    const rect = el.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  }).slice(0, maxElements);
  const elements = candidates.map((el) => {
    const ref = refFor(el);
    const rect = el.getBoundingClientRect();
    return {
      ref,
      tag: (el.tagName || "").toLowerCase(),
      role: el.getAttribute("role"),
      label: pickLabel(el),
      href: el.href || el.getAttribute("href"),
      type: el.getAttribute("type"),
      rect: {
        x: Math.round(rect.x),
        y: Math.round(rect.y),
        width: Math.round(rect.width),
        height: Math.round(rect.height)
      }
    };
  });
  const selection = window.getSelection();
  return {
    url: location.href,
    title: document.title || "",
    readyState: document.readyState,
    text: (document.body && document.body.innerText || "").replace(/\n{3,}/g, "\n\n").slice(0, maxText),
    selection: selection ? selection.toString().slice(0, 4000) : "",
    elements,
    viewport: {
      width: window.innerWidth,
      height: window.innerHeight,
      scrollX: window.scrollX,
      scrollY: window.scrollY
    }
  };
};
"#;
