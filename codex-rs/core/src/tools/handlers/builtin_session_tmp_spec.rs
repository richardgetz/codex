use std::collections::BTreeMap;

use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;

pub(crate) const TOOL_CREATE: &str = "create";
pub(crate) const TOOL_REGISTER: &str = "register";
pub(crate) const TOOL_LIST: &str = "list";
pub(crate) const TOOL_RETAIN: &str = "retain";
pub(crate) const TOOL_SCHEMA: &str = "get_schema";

pub(crate) const SESSION_TMP_TOOL_DESCRIPTIONS: &[(&str, &str)] = &[
    (
        TOOL_CREATE,
        "Create and durably register a session-owned temporary file or directory.",
    ),
    (
        TOOL_REGISTER,
        "Register an existing path created inside the current agent's session temporary directory.",
    ),
    (
        TOOL_LIST,
        "List tracked and untracked temporary paths for the current session and their owners.",
    ),
    (
        TOOL_RETAIN,
        "Change retention for a temporary entry owned by the current agent.",
    ),
    (
        TOOL_SCHEMA,
        "Return the session temporary storage contract and retention values.",
    ),
];

pub(crate) fn session_tmp_namespace_spec() -> ToolSpec {
    let tools = SESSION_TMP_TOOL_DESCRIPTIONS
        .iter()
        .copied()
        .map(|(name, description)| {
            ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: name.to_string(),
                description: description.to_string(),
                strict: false,
                defer_loading: None,
                parameters: schema_for(name),
                output_schema: None,
            })
        })
        .collect();

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: "session_tmp".to_string(),
        description:
            "Built-in persistent temporary storage with session/thread lineage and safe cleanup."
                .to_string(),
        tools,
    })
}

fn schema_for(name: &str) -> JsonSchema {
    match name {
        TOOL_CREATE => object_schema(
            [
                ("name", JsonSchema::string(None)),
                ("purpose", JsonSchema::string(None)),
                (
                    "retention",
                    JsonSchema::string(Some(
                        "Use `session`, `manual`, or `ttl:<seconds>` for a numeric TTL."
                            .to_string(),
                    )),
                ),
                (
                    "kind",
                    JsonSchema::string_enum(
                        ["file", "directory"]
                            .into_iter()
                            .map(|value| json!(value))
                            .collect(),
                        None,
                    ),
                ),
            ],
            &["purpose"],
        ),
        TOOL_REGISTER => object_schema(
            [
                ("path", JsonSchema::string(None)),
                ("purpose", JsonSchema::string(None)),
                (
                    "retention",
                    JsonSchema::string(Some(
                        "Use `session`, `manual`, or `ttl:<seconds>` for a numeric TTL."
                            .to_string(),
                    )),
                ),
            ],
            &["path", "purpose"],
        ),
        TOOL_LIST => object_schema([], &[]),
        TOOL_RETAIN => object_schema(
            [
                ("entry_id", JsonSchema::string(None)),
                (
                    "retention",
                    JsonSchema::string(Some(
                        "Use `session`, `manual`, or `ttl:<seconds>` for a numeric TTL."
                            .to_string(),
                    )),
                ),
            ],
            &["entry_id", "retention"],
        ),
        TOOL_SCHEMA => object_schema([], &[]),
        _ => unreachable!("unknown session temporary tool schema: {name}"),
    }
}

fn object_schema<const N: usize>(
    fields: [(&'static str, JsonSchema); N],
    required: &[&str],
) -> JsonSchema {
    let properties = fields
        .into_iter()
        .map(|(name, schema)| (name.to_string(), schema))
        .collect::<BTreeMap<_, _>>();
    JsonSchema::object(
        properties,
        Some(required.iter().map(|value| (*value).to_string()).collect()),
        Some(AdditionalProperties::Boolean(false)),
    )
}
