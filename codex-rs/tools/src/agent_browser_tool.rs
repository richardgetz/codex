use crate::AdditionalProperties;
use crate::JsonSchema;
use crate::ResponsesApiNamespace;
use crate::ResponsesApiNamespaceTool;
use crate::ResponsesApiTool;
use crate::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub const AGENT_BROWSER_NAMESPACE: &str = "agent_browser";
pub const TOOL_OPEN: &str = "open";
pub const TOOL_CLOSE: &str = "close";
pub const TOOL_NAVIGATE: &str = "navigate";
pub const TOOL_SNAPSHOT: &str = "snapshot";
pub const TOOL_SCREENSHOT: &str = "screenshot";
pub const TOOL_CLICK: &str = "click";
pub const TOOL_TYPE: &str = "type";
pub const TOOL_SCROLL: &str = "scroll";
pub const TOOL_SELECTION: &str = "selection_overview";
pub const TOOL_BENCHMARK: &str = "benchmark";

pub const AGENT_BROWSER_TOOL_NAMES: &[&str] = &[
    TOOL_OPEN,
    TOOL_CLOSE,
    TOOL_NAVIGATE,
    TOOL_SNAPSHOT,
    TOOL_SCREENSHOT,
    TOOL_CLICK,
    TOOL_TYPE,
    TOOL_SCROLL,
    TOOL_SELECTION,
    TOOL_BENCHMARK,
];

pub fn create_agent_browser_tool() -> ToolSpec {
    let tools = vec![
        tool(
            TOOL_OPEN,
            "Launch or attach to a built-in agent browser session. Supports headful, headless, and stealth launch profiles.",
            open_schema(),
        ),
        tool(
            TOOL_CLOSE,
            "Close an agent browser session and terminate any browser process that Codex launched.",
            session_schema(),
        ),
        tool(
            TOOL_NAVIGATE,
            "Navigate an agent browser session to a URL and wait briefly for the page to become ready.",
            navigate_schema(),
        ),
        tool(
            TOOL_SNAPSHOT,
            "Return a compact page snapshot with stable element refs, text, URL, title, and selection state.",
            snapshot_schema(),
        ),
        tool(
            TOOL_SCREENSHOT,
            "Capture the current viewport as a PNG image that the agent can inspect.",
            screenshot_schema(),
        ),
        tool(
            TOOL_CLICK,
            "Click a page point or stable element ref from a prior snapshot.",
            click_schema(),
        ),
        tool(
            TOOL_TYPE,
            "Type text into the focused field, active element, or a stable element ref from a prior snapshot.",
            type_schema(),
        ),
        tool(
            TOOL_SCROLL,
            "Scroll the page by a pixel delta.",
            scroll_schema(),
        ),
        tool(
            TOOL_SELECTION,
            "Inject or read the collaborative selection overlay and return a model-visible overview of the highlighted region.",
            selection_schema(),
        ),
        tool(
            TOOL_BENCHMARK,
            "Run a local browser latency benchmark for launch, navigation, snapshot, and screenshot operations.",
            benchmark_schema(),
        ),
    ];

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: AGENT_BROWSER_NAMESPACE.to_string(),
        description: "Built-in browser automation for agents. It is designed for authorized UI review and web workflows, with headful/headless modes, compact snapshots, screenshots, and collaborative selection capture.".to_string(),
        tools,
    })
}

fn tool(name: &str, description: &str, parameters: JsonSchema) -> ResponsesApiNamespaceTool {
    ResponsesApiNamespaceTool::Function(ResponsesApiTool {
        name: name.to_string(),
        description: description.to_string(),
        strict: false,
        parameters,
        output_schema: None,
        defer_loading: None,
    })
}

fn open_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "url".to_string(),
                nullable_string("Optional URL to open immediately."),
            ),
            (
                "mode".to_string(),
                JsonSchema::string_enum(
                    vec![json!("headful"), json!("headless")],
                    Some("Browser display mode. Defaults to headful.".to_string()),
                ),
            ),
            (
                "stealth".to_string(),
                JsonSchema::boolean(Some(
                    "Enable the built-in low-noise browser profile. Defaults to true.".to_string(),
                )),
            ),
            (
                "viewport_width".to_string(),
                JsonSchema::integer(Some("Viewport width in CSS pixels.".to_string())),
            ),
            (
                "viewport_height".to_string(),
                JsonSchema::integer(Some("Viewport height in CSS pixels.".to_string())),
            ),
            (
                "locale".to_string(),
                nullable_string("Browser locale hint such as en-US."),
            ),
            (
                "timezone".to_string(),
                nullable_string("Browser timezone id such as America/New_York."),
            ),
            (
                "user_agent".to_string(),
                nullable_string("Optional explicit user agent override."),
            ),
            (
                "remote_debugging_url".to_string(),
                nullable_string(
                    "Attach to an existing Chrome/Chromium debugging endpoint instead of launching a process.",
                ),
            ),
        ]),
        None,
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn session_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([(
            "session_id".to_string(),
            nullable_string("Browser session id. Defaults to the active session."),
        )]),
        None,
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn navigate_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "session_id".to_string(),
                nullable_string("Browser session id."),
            ),
            (
                "url".to_string(),
                JsonSchema::string(Some("URL to navigate to.".to_string())),
            ),
        ]),
        Some(vec!["url".to_string()]),
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn snapshot_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "session_id".to_string(),
                nullable_string("Browser session id."),
            ),
            (
                "max_text_chars".to_string(),
                JsonSchema::integer(Some(
                    "Maximum characters of page text to include. Defaults to 12000.".to_string(),
                )),
            ),
            (
                "max_elements".to_string(),
                JsonSchema::integer(Some(
                    "Maximum interactive element refs to include. Defaults to 80.".to_string(),
                )),
            ),
        ]),
        None,
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn screenshot_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "session_id".to_string(),
                nullable_string("Browser session id."),
            ),
            (
                "full_page".to_string(),
                JsonSchema::boolean(Some(
                    "Capture the full page instead of only the viewport.".to_string(),
                )),
            ),
        ]),
        None,
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn click_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "session_id".to_string(),
                nullable_string("Browser session id."),
            ),
            (
                "ref".to_string(),
                nullable_string("Stable element ref from a prior snapshot, such as e3."),
            ),
            (
                "x".to_string(),
                JsonSchema::number(Some("Viewport x coordinate.".to_string())),
            ),
            (
                "y".to_string(),
                JsonSchema::number(Some("Viewport y coordinate.".to_string())),
            ),
        ]),
        None,
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn type_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "session_id".to_string(),
                nullable_string("Browser session id."),
            ),
            (
                "ref".to_string(),
                nullable_string("Stable element ref from a prior snapshot, such as e3."),
            ),
            (
                "text".to_string(),
                JsonSchema::string(Some("Text to type.".to_string())),
            ),
            (
                "clear".to_string(),
                JsonSchema::boolean(Some(
                    "Clear the target field before typing. Defaults to false.".to_string(),
                )),
            ),
        ]),
        Some(vec!["text".to_string()]),
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn scroll_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "session_id".to_string(),
                nullable_string("Browser session id."),
            ),
            (
                "delta_x".to_string(),
                JsonSchema::number(Some("Horizontal scroll delta in pixels.".to_string())),
            ),
            (
                "delta_y".to_string(),
                JsonSchema::number(Some("Vertical scroll delta in pixels.".to_string())),
            ),
        ]),
        None,
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn selection_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "session_id".to_string(),
                nullable_string("Browser session id."),
            ),
            (
                "enable_overlay".to_string(),
                JsonSchema::boolean(Some(
                    "Enable the visible collaborative highlight overlay before reading state."
                        .to_string(),
                )),
            ),
        ]),
        None,
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn benchmark_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "mode".to_string(),
                JsonSchema::string_enum(
                    vec![json!("headful"), json!("headless")],
                    Some("Browser display mode to benchmark. Defaults to headless.".to_string()),
                ),
            ),
            (
                "iterations".to_string(),
                JsonSchema::integer(Some(
                    "Number of snapshot/screenshot iterations. Defaults to 3.".to_string(),
                )),
            ),
            (
                "stealth".to_string(),
                JsonSchema::boolean(Some(
                    "Benchmark with the low-noise browser profile. Defaults to true.".to_string(),
                )),
            ),
        ]),
        None,
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn nullable_string(description: &str) -> JsonSchema {
    JsonSchema::any_of(
        vec![
            JsonSchema::string(Some(description.to_string())),
            JsonSchema::null(None),
        ],
        Some(description.to_string()),
    )
}
