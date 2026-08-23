use std::collections::BTreeMap;

use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;

pub(crate) const TOOL_OPEN: &str = "open_scratchpad";
pub(crate) const TOOL_RESUME: &str = "resume_scratchpad";
pub(crate) const TOOL_GET: &str = "get_scratchpad";
pub(crate) const TOOL_SUMMARY: &str = "get_scratchpad_summary";
pub(crate) const TOOL_APPEND_NOTE: &str = "append_scratchpad_note";
pub(crate) const TOOL_SET_NEXT_STEPS: &str = "set_next_steps";
pub(crate) const TOOL_SET_PENDING_WAITS: &str = "set_pending_waits";
pub(crate) const TOOL_SET_ACTION_POLICY: &str = "set_action_policy";
pub(crate) const TOOL_MARK_WAIT_CHECKED: &str = "mark_wait_checked";
pub(crate) const TOOL_UPDATE: &str = "update_scratchpad";
pub(crate) const TOOL_ARCHIVE: &str = "archive_scratchpad";
pub(crate) const TOOL_UNARCHIVE: &str = "unarchive_scratchpad";
pub(crate) const TOOL_LOOKUP: &str = "lookup_scratchpads";
pub(crate) const TOOL_SCHEMA: &str = "get_scratchpad_schema";
pub(crate) const TOOL_CHECK_ACTION: &str = "check_action_allowed";
pub(crate) const TOOL_RECORD_OUTCOME: &str = "record_outcome";
pub(crate) const TOOL_EXPORT_OUTCOMES: &str = "export_outcomes";
pub(crate) const TOOL_RECORD_DELEGATION: &str = "record_delegation";
pub(crate) const ACTION_VALUES: &[&str] = &[
    "finalize",
    "end_followup",
    "merge",
    "pull_request",
    "release",
    "deploy",
    "ecs_benchmark_launch",
    "aws_write",
];
pub(crate) const DELEGATION_STATUS_VALUES: &[&str] = &[
    "active",
    "blocked",
    "cancelled",
    "complete",
    "deleted",
    "delegated",
    "failed",
];

pub(crate) const SCRATCHPAD_TOOL_DESCRIPTIONS: &[(&str, &str)] = &[
    (
        TOOL_OPEN,
        "Open the current thread scratchpad for the same objective/session, or create it.",
    ),
    (
        TOOL_RESUME,
        "Resume the current thread scratchpad without creating a new one.",
    ),
    (TOOL_GET, "Fetch the current thread scratchpad."),
    (
        TOOL_SUMMARY,
        "Fetch a compact current-state summary for the current thread scratchpad.",
    ),
    (
        TOOL_APPEND_NOTE,
        "Append a timestamped working note to a scratchpad.",
    ),
    (
        TOOL_SET_NEXT_STEPS,
        "Replace the scratchpad's current next-step list.",
    ),
    (
        TOOL_SET_PENDING_WAITS,
        "Replace the scratchpad's structured pending wait list.",
    ),
    (
        TOOL_SET_ACTION_POLICY,
        "Replace the scratchpad's structured action policy.",
    ),
    (
        TOOL_MARK_WAIT_CHECKED,
        "Mark one pending wait as checked, update its reuse details, or resolve it.",
    ),
    (TOOL_UPDATE, "Update structured scratchpad fields."),
    (
        TOOL_ARCHIVE,
        "Archive a scratchpad when the objective is finished.",
    ),
    (
        TOOL_UNARCHIVE,
        "Restore an archived scratchpad to active use.",
    ),
    (
        TOOL_LOOKUP,
        "Search active or archived scratchpads by id/objective/session/status text.",
    ),
    (
        TOOL_SCHEMA,
        "Return the canonical scratchpad schema and tool contract.",
    ),
    (
        TOOL_CHECK_ACTION,
        "Check whether an action appears allowed by the scratchpad action policy.",
    ),
    (
        TOOL_RECORD_OUTCOME,
        "Append a measured outcome/progress datapoint with scope, metric, value, provenance, and summary.",
    ),
    (
        TOOL_EXPORT_OUTCOMES,
        "Export scratchpad outcome measurements as portable JSON plus a markdown summary.",
    ),
    (
        TOOL_RECORD_DELEGATION,
        "Record or update parent scratchpad lineage for work delegated to a subagent.",
    ),
];

pub(crate) fn scratchpad_namespace_spec() -> ToolSpec {
    let tools = SCRATCHPAD_TOOL_DESCRIPTIONS
        .iter()
        .copied()
        .map(|(name, description)| {
            ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: name.to_string(),
                description: description.to_string(),
                strict: false,
                defer_loading: None,
                parameters: scratchpad_tool_schema(name),
                output_schema: None,
            })
        })
        .collect();

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: "scratchpad".to_string(),
        description:
            "Built-in durable scratchpad tools for active objective recovery and compaction resilience."
                .to_string(),
        tools,
    })
}

fn scratchpad_tool_schema(name: &str) -> JsonSchema {
    match name {
        TOOL_OPEN => object_schema(open_properties(), &["objective"]),
        TOOL_RESUME => object_schema(
            with_fields(
                string_fields(&["scratchpad_id"]),
                [(
                    "include_archived",
                    JsonSchema::boolean(/*description*/ None),
                )],
            ),
            &["scratchpad_id"],
        ),
        TOOL_GET | TOOL_SUMMARY | TOOL_EXPORT_OUTCOMES => {
            object_schema(string_fields(&["scratchpad_id"]), &["scratchpad_id"])
        }
        TOOL_APPEND_NOTE => object_schema(
            string_fields(&["scratchpad_id", "category", "summary", "outcome"]),
            &["scratchpad_id", "summary"],
        ),
        TOOL_SET_NEXT_STEPS => object_schema(
            with_fields(
                string_fields(&["scratchpad_id", "status"]),
                [("next_steps", string_array_schema())],
            ),
            &["scratchpad_id", "next_steps"],
        ),
        TOOL_SET_PENDING_WAITS => object_schema(
            with_fields(
                string_fields(&["scratchpad_id", "status"]),
                [(
                    "pending_waits",
                    JsonSchema::array(pending_wait_schema(), /*description*/ None),
                )],
            ),
            &["scratchpad_id", "pending_waits"],
        ),
        TOOL_SET_ACTION_POLICY => object_schema(
            with_fields(
                string_fields(&["scratchpad_id"]),
                [("action_policy", object_value_schema())],
            ),
            &["scratchpad_id", "action_policy"],
        ),
        TOOL_MARK_WAIT_CHECKED => {
            let mut schema = object_schema(
                with_fields(
                    string_fields(&[
                        "scratchpad_id",
                        "wait_id",
                        "target",
                        "next_check_at",
                        "reuse_session_id",
                        "check_method",
                    ]),
                    [
                        ("resolved", JsonSchema::boolean(/*description*/ None)),
                        ("fallback_work", string_array_schema()),
                    ],
                ),
                &["scratchpad_id"],
            );
            schema.any_of = Some(vec![
                JsonSchema::object(
                    BTreeMap::new(),
                    Some(vec!["wait_id".to_string()]),
                    Some(AdditionalProperties::Boolean(true)),
                ),
                JsonSchema::object(
                    BTreeMap::new(),
                    Some(vec!["target".to_string()]),
                    Some(AdditionalProperties::Boolean(true)),
                ),
            ]);
            schema
        }
        TOOL_UPDATE => object_schema(update_properties(), &["scratchpad_id"]),
        TOOL_ARCHIVE => object_schema(
            string_fields(&["scratchpad_id", "status", "summary", "outcome"]),
            &["scratchpad_id"],
        ),
        TOOL_UNARCHIVE => object_schema(
            string_fields(&["scratchpad_id", "status"]),
            &["scratchpad_id"],
        ),
        TOOL_LOOKUP => object_schema(
            with_fields(
                string_fields(&["query", "objective"]),
                [
                    (
                        "include_archived",
                        JsonSchema::boolean(/*description*/ None),
                    ),
                    ("limit", JsonSchema::integer(/*description*/ None)),
                ],
            ),
            &[],
        ),
        TOOL_SCHEMA => object_schema(BTreeMap::new(), &[]),
        TOOL_CHECK_ACTION => object_schema(
            with_fields(
                string_fields(&["scratchpad_id", "repo", "target_branch", "env", "channel"]),
                [
                    ("action", action_schema()),
                    (
                        "bypass_pr_requirements",
                        JsonSchema::boolean(/*description*/ None),
                    ),
                ],
            ),
            &["scratchpad_id", "action"],
        ),
        TOOL_RECORD_OUTCOME => outcome_schema(),
        TOOL_RECORD_DELEGATION => object_schema(delegation_properties(), &["scratchpad_id"]),
        _ => unreachable!("unknown scratchpad tool schema: {name}"),
    }
}

fn open_properties() -> BTreeMap<String, JsonSchema> {
    with_fields(
        update_properties(),
        [
            ("session_key", JsonSchema::string(/*description*/ None)),
            (
                "refresh_session_key",
                JsonSchema::boolean(/*description*/ None),
            ),
        ],
    )
}

fn update_properties() -> BTreeMap<String, JsonSchema> {
    let mut properties = string_fields(&[
        "scratchpad_id",
        "objective",
        "status",
        "resume_instructions",
        "interruption_policy",
        "final_guard",
    ]);
    properties.extend([
        (
            "worktrees".to_string(),
            JsonSchema::array(worktree_schema(), /*description*/ None),
        ),
        ("active_channels".to_string(), string_array_schema()),
        ("active_sessions".to_string(), object_value_schema()),
        ("action_policy".to_string(), object_value_schema()),
        ("run_policy".to_string(), object_value_schema()),
        ("communication_policy".to_string(), object_value_schema()),
        ("completed".to_string(), string_array_schema()),
        ("next_steps".to_string(), string_array_schema()),
        (
            "pending_waits".to_string(),
            JsonSchema::array(pending_wait_schema(), /*description*/ None),
        ),
        (
            "blocked".to_string(),
            JsonSchema::array(any_of_string_or_object(), /*description*/ None),
        ),
        (
            "git_refs".to_string(),
            JsonSchema::array(git_ref_schema(), /*description*/ None),
        ),
        (
            "artifacts".to_string(),
            JsonSchema::array(artifact_schema(), /*description*/ None),
        ),
        ("stop_conditions".to_string(), string_array_schema()),
        ("last_benchmark".to_string(), object_value_schema()),
        (
            "outcomes".to_string(),
            JsonSchema::array(any_value_object_schema(), /*description*/ None),
        ),
        (
            "delegations".to_string(),
            JsonSchema::array(any_value_object_schema(), /*description*/ None),
        ),
        (
            "notes".to_string(),
            JsonSchema::array(any_value_object_schema(), /*description*/ None),
        ),
    ]);
    properties
}

fn outcome_properties() -> BTreeMap<String, JsonSchema> {
    let mut properties = string_fields(&[
        "scratchpad_id",
        "outcome_id",
        "metric",
        "metric_name",
        "unit",
        "summary",
        "commit",
        "pr",
    ]);
    properties.extend([
        ("scope".to_string(), any_of_string_or_object()),
        ("baseline".to_string(), number_or_string_schema()),
        ("current".to_string(), number_or_string_schema()),
        ("value".to_string(), number_or_string_schema()),
        ("delta".to_string(), number_or_string_schema()),
        ("change".to_string(), number_or_string_schema()),
        ("direction".to_string(), direction_schema()),
        ("tradeoffs".to_string(), string_array_schema()),
        ("provenance".to_string(), object_value_schema()),
        (
            "artifacts".to_string(),
            JsonSchema::array(any_value_object_schema(), /*description*/ None),
        ),
        ("notes".to_string(), string_array_schema()),
    ]);
    properties
}

fn outcome_schema() -> JsonSchema {
    let mut schema = object_schema(outcome_properties(), &["scratchpad_id"]);
    schema.any_of = Some(vec![
        JsonSchema::object(
            BTreeMap::new(),
            Some(vec!["metric".to_string()]),
            Some(AdditionalProperties::Boolean(true)),
        ),
        JsonSchema::object(
            BTreeMap::new(),
            Some(vec!["metric_name".to_string()]),
            Some(AdditionalProperties::Boolean(true)),
        ),
    ]);
    schema
}

fn delegation_properties() -> BTreeMap<String, JsonSchema> {
    let mut properties = string_fields(&[
        "scratchpad_id",
        "delegation_id",
        "agent_id",
        "agent_label",
        "child_scratchpad_id",
        "summary",
    ]);
    properties.extend([
        ("status".to_string(), delegation_status_schema()),
        ("item_refs".to_string(), string_array_schema()),
        ("task_ids".to_string(), string_array_schema()),
        ("next_steps".to_string(), string_array_schema()),
        (
            "artifacts".to_string(),
            JsonSchema::array(any_value_object_schema(), /*description*/ None),
        ),
        ("notes".to_string(), string_array_schema()),
    ]);
    properties
}

fn pending_wait_schema() -> JsonSchema {
    let mut properties = string_fields(&[
        "wait_id",
        "target",
        "summary",
        "description",
        "reason",
        "wait_type",
        "next_check_at",
        "reuse_session_id",
        "check_method",
        "last_checked_at",
        "status",
    ]);
    properties.extend([
        ("fallback_work".to_string(), string_array_schema()),
        (
            "blocking".to_string(),
            JsonSchema::boolean(/*description*/ None),
        ),
    ]);
    JsonSchema::any_of(
        vec![
            JsonSchema::string(/*description*/ None),
            permissive_object_schema(properties, &[]),
        ],
        /*description*/ None,
    )
}

fn worktree_schema() -> JsonSchema {
    permissive_object_schema(string_fields(&["repo", "branch"]), &[])
}

fn git_ref_schema() -> JsonSchema {
    permissive_object_schema(
        string_fields(&["repo", "branch", "commit_sha", "role"]),
        &[],
    )
}

fn artifact_schema() -> JsonSchema {
    permissive_object_schema(string_fields(&["kind", "value", "repo", "label"]), &[])
}

fn action_schema() -> JsonSchema {
    JsonSchema::string_enum(
        ACTION_VALUES.iter().map(|value| json!(*value)).collect(),
        /*description*/ None,
    )
}

fn direction_schema() -> JsonSchema {
    JsonSchema::string_enum(
        ["higher_is_better", "lower_is_better", "neutral"]
            .into_iter()
            .map(|value| json!(value))
            .collect(),
        /*description*/ None,
    )
}

fn delegation_status_schema() -> JsonSchema {
    JsonSchema::string_enum(
        DELEGATION_STATUS_VALUES
            .iter()
            .map(|value| json!(*value))
            .collect(),
        /*description*/ None,
    )
}

fn object_schema(properties: BTreeMap<String, JsonSchema>, required: &[&str]) -> JsonSchema {
    JsonSchema::object(
        properties,
        Some(required.iter().map(ToString::to_string).collect()),
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn permissive_object_schema(
    properties: BTreeMap<String, JsonSchema>,
    required: &[&str],
) -> JsonSchema {
    JsonSchema::object(
        properties,
        Some(required.iter().map(ToString::to_string).collect()),
        Some(AdditionalProperties::Boolean(true)),
    )
}

fn string_fields(names: &[&str]) -> BTreeMap<String, JsonSchema> {
    names
        .iter()
        .map(|name| {
            (
                (*name).to_string(),
                JsonSchema::string(/*description*/ None),
            )
        })
        .collect()
}

fn with_fields<const N: usize>(
    mut properties: BTreeMap<String, JsonSchema>,
    fields: [(&str, JsonSchema); N],
) -> BTreeMap<String, JsonSchema> {
    properties.extend(
        fields
            .into_iter()
            .map(|(name, schema)| (name.to_string(), schema)),
    );
    properties
}

fn string_array_schema() -> JsonSchema {
    JsonSchema::array(
        JsonSchema::string(/*description*/ None),
        /*description*/ None,
    )
}

fn object_value_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::new(),
        Some(Vec::new()),
        Some(AdditionalProperties::Boolean(true)),
    )
}

fn any_value_object_schema() -> JsonSchema {
    object_value_schema()
}

fn any_of_string_or_object() -> JsonSchema {
    JsonSchema::any_of(
        vec![
            JsonSchema::string(/*description*/ None),
            object_value_schema(),
        ],
        /*description*/ None,
    )
}

fn number_or_string_schema() -> JsonSchema {
    JsonSchema::any_of(
        vec![
            JsonSchema::number(/*description*/ None),
            JsonSchema::string(/*description*/ None),
        ],
        /*description*/ None,
    )
}
