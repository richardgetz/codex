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
pub const TOOL_PRESS: &str = "press";
pub const TOOL_SCROLL: &str = "scroll";
pub const TOOL_SELECTION: &str = "selection_overview";
pub const TOOL_HIGHLIGHT: &str = "highlight";
pub const TOOL_BENCHMARK: &str = "benchmark";

pub const AGENT_BROWSER_TOOL_NAMES: &[&str] = &[
    TOOL_OPEN,
    TOOL_CLOSE,
    TOOL_NAVIGATE,
    TOOL_SNAPSHOT,
    TOOL_SCREENSHOT,
    TOOL_CLICK,
    TOOL_TYPE,
    TOOL_PRESS,
    TOOL_SCROLL,
    TOOL_SELECTION,
    TOOL_HIGHLIGHT,
    TOOL_BENCHMARK,
];

pub fn create_agent_browser_tool() -> ToolSpec {
    let tools = vec![
        tool(
            TOOL_OPEN,
            "Launch or attach to an agent browser session.",
            open_schema(),
        ),
        tool(
            TOOL_CLOSE,
            "Close an agent browser session.",
            session_schema(),
        ),
        tool(
            TOOL_NAVIGATE,
            "Navigate a browser session to a URL.",
            navigate_schema(),
        ),
        tool(
            TOOL_SNAPSHOT,
            "Return page text, selection state, and stable element refs.",
            snapshot_schema(),
        ),
        tool(
            TOOL_SCREENSHOT,
            "Capture the current viewport as a PNG.",
            screenshot_schema(),
        ),
        tool(
            TOOL_CLICK,
            "Click a viewport point or element ref.",
            click_schema(),
        ),
        tool(
            TOOL_TYPE,
            "Type text into the focused field or element ref.",
            type_schema(),
        ),
        tool(TOOL_PRESS, "Press a common keyboard key.", press_schema()),
        tool(
            TOOL_SCROLL,
            "Scroll the page by a pixel delta.",
            scroll_schema(),
        ),
        tool(
            TOOL_SELECTION,
            "Read or enable the collaborative selection overlay.",
            selection_schema(),
        ),
        tool(
            TOOL_HIGHLIGHT,
            "Mark or clear a collaborative page highlight.",
            highlight_schema(),
        ),
        tool(
            TOOL_BENCHMARK,
            "Benchmark launch, navigation, snapshot, and screenshot latency.",
            benchmark_schema(),
        ),
    ];

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: AGENT_BROWSER_NAMESPACE.to_string(),
        description: "Built-in browser automation with headful/headless modes, snapshots, screenshots, input, selection capture, and highlights.".to_string(),
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
            ("url".to_string(), string_param("URL to open immediately.")),
            (
                "mode".to_string(),
                JsonSchema::string_enum(
                    vec![json!("headful"), json!("headless")],
                    Some("Display mode. Defaults to headful.".to_string()),
                ),
            ),
            (
                "stealth".to_string(),
                JsonSchema::boolean(Some(
                    "Enable stealth profile. Defaults to true.".to_string(),
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
                string_param("Locale hint such as en-US."),
            ),
            (
                "timezone".to_string(),
                string_param("Timezone id such as America/New_York."),
            ),
            (
                "user_agent".to_string(),
                string_param("User agent override."),
            ),
            (
                "remote_debugging_url".to_string(),
                string_param("Existing Chrome/Chromium debugging endpoint."),
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
            string_param("Session id. Defaults to the active session."),
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
                string_param("Browser session id."),
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
                string_param("Browser session id."),
            ),
            (
                "max_text_chars".to_string(),
                JsonSchema::integer(Some("Max page text chars. Defaults to 12000.".to_string())),
            ),
            (
                "max_elements".to_string(),
                JsonSchema::integer(Some("Max element refs. Defaults to 80.".to_string())),
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
                string_param("Browser session id."),
            ),
            (
                "full_page".to_string(),
                JsonSchema::boolean(Some("Capture full page instead of viewport.".to_string())),
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
                string_param("Browser session id."),
            ),
            (
                "ref".to_string(),
                string_param("Stable element ref, such as e3."),
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
                string_param("Browser session id."),
            ),
            (
                "ref".to_string(),
                string_param("Stable element ref, such as e3."),
            ),
            (
                "text".to_string(),
                JsonSchema::string(Some("Text to type.".to_string())),
            ),
            (
                "clear".to_string(),
                JsonSchema::boolean(Some(
                    "Clear target before typing. Defaults to false.".to_string(),
                )),
            ),
        ]),
        Some(vec!["text".to_string()]),
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn press_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "session_id".to_string(),
                string_param("Browser session id."),
            ),
            (
                "key".to_string(),
                JsonSchema::string(Some(
                    "Key to press, such as Enter, Tab, Escape, Backspace, Delete, Space, ArrowUp, ArrowDown, ArrowLeft, ArrowRight, Home, End, PageUp, or PageDown."
                        .to_string(),
                )),
            ),
        ]),
        Some(vec!["key".to_string()]),
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn scroll_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "session_id".to_string(),
                string_param("Browser session id."),
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
                string_param("Browser session id."),
            ),
            (
                "enable_overlay".to_string(),
                JsonSchema::boolean(Some("Enable overlay before reading state.".to_string())),
            ),
        ]),
        None,
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn highlight_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "session_id".to_string(),
                string_param("Browser session id."),
            ),
            (
                "ref".to_string(),
                string_param("Element ref to highlight, such as e3."),
            ),
            (
                "x".to_string(),
                JsonSchema::number(Some("Viewport x coordinate.".to_string())),
            ),
            (
                "y".to_string(),
                JsonSchema::number(Some("Viewport y coordinate.".to_string())),
            ),
            (
                "width".to_string(),
                JsonSchema::number(Some("Highlight width in pixels.".to_string())),
            ),
            (
                "height".to_string(),
                JsonSchema::number(Some("Highlight height in pixels.".to_string())),
            ),
            ("label".to_string(), string_param("Short visible label.")),
            (
                "color".to_string(),
                string_param("CSS color. Defaults to #d93025."),
            ),
            (
                "clear".to_string(),
                JsonSchema::boolean(Some("Clear existing highlights.".to_string())),
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
                    Some("Display mode. Defaults to headless.".to_string()),
                ),
            ),
            (
                "iterations".to_string(),
                JsonSchema::integer(Some(
                    "Snapshot/screenshot iterations. Defaults to 3.".to_string(),
                )),
            ),
            (
                "stealth".to_string(),
                JsonSchema::boolean(Some("Use stealth profile. Defaults to true.".to_string())),
            ),
            (
                "remote_debugging_url".to_string(),
                string_param("Existing Chrome/Chromium debugging endpoint."),
            ),
        ]),
        None,
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn string_param(description: &str) -> JsonSchema {
    JsonSchema::string(Some(description.to_string()))
}
