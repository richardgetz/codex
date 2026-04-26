use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use chrono::Utc;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

const SCRATCHPAD_NAMESPACE: &str = "scratchpad";
const TOOL_OPEN: &str = "open_scratchpad";
const TOOL_GET: &str = "get_scratchpad";
const TOOL_SUMMARY: &str = "get_scratchpad_summary";
const TOOL_APPEND_NOTE: &str = "append_scratchpad_note";
const TOOL_SET_NEXT_STEPS: &str = "set_next_steps";
const TOOL_SET_PENDING_WAITS: &str = "set_pending_waits";
const TOOL_UPDATE: &str = "update_scratchpad";
const TOOL_ARCHIVE: &str = "archive_scratchpad";
const TOOL_UNARCHIVE: &str = "unarchive_scratchpad";
const TOOL_LOOKUP: &str = "lookup_scratchpads";
const TOOL_SCHEMA: &str = "get_scratchpad_schema";
const TOOL_CHECK_ACTION: &str = "check_action_allowed";

pub(crate) fn scratchpad_namespace_spec() -> ToolSpec {
    let tools = [
        (
            TOOL_OPEN,
            "Open an existing active scratchpad for the same objective/session, or create one.",
        ),
        (TOOL_GET, "Fetch one scratchpad by id."),
        (
            TOOL_SUMMARY,
            "Fetch a compact current-state summary for one scratchpad.",
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
            "Replace the scratchpad's structured pending waits list.",
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
    ]
    .into_iter()
    .map(|(name, description)| {
        ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: name.to_string(),
            description: description.to_string(),
            strict: false,
            defer_loading: None,
            parameters: loose_object_schema(),
            output_schema: None,
        })
    })
    .collect();

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: SCRATCHPAD_NAMESPACE.to_string(),
        description:
            "Built-in durable scratchpad tools for active objective recovery and compaction resilience."
                .to_string(),
        tools,
    })
}

fn loose_object_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::new(),
        /*required*/ None,
        Some(AdditionalProperties::Boolean(true)),
    )
}

pub(crate) struct BuiltinScratchpadHandler;

impl ToolHandler for BuiltinScratchpadHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        !matches!(
            invocation.tool_name.name.as_str(),
            TOOL_GET | TOOL_SUMMARY | TOOL_LOOKUP | TOOL_SCHEMA | TOOL_CHECK_ACTION
        )
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let arguments = match invocation.payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "scratchpad handler received unsupported payload".to_string(),
                ));
            }
        };
        let args: Value = parse_arguments(&arguments)?;
        let config = invocation.session.get_config().await;
        let store = ScratchpadStore::new(
            args.get("state_home").and_then(Value::as_str),
            config.codex_home.as_path(),
        )?;
        let default_scratchpad_id = invocation.session.conversation_id.to_string();

        let result = match invocation.tool_name.name.as_str() {
            TOOL_OPEN => open_scratchpad(&store, &args, &default_scratchpad_id),
            TOOL_GET => get_scratchpad(&store, &args),
            TOOL_SUMMARY => get_scratchpad_summary(&store, &args),
            TOOL_APPEND_NOTE => append_scratchpad_note(&store, &args),
            TOOL_SET_NEXT_STEPS => set_next_steps(&store, &args),
            TOOL_SET_PENDING_WAITS => set_pending_waits(&store, &args),
            TOOL_UPDATE => update_scratchpad(&store, &args),
            TOOL_ARCHIVE => archive_scratchpad(&store, &args),
            TOOL_UNARCHIVE => unarchive_scratchpad(&store, &args),
            TOOL_LOOKUP => lookup_scratchpads(&store, &args),
            TOOL_SCHEMA => Ok(schema_payload()),
            TOOL_CHECK_ACTION => check_action_allowed(&store, &args),
            tool_name => Err(FunctionCallError::RespondToModel(format!(
                "unknown scratchpad tool: {tool_name}"
            ))),
        }?;

        Ok(FunctionToolOutput::from_text(
            json_text(result)?,
            Some(true),
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Scratchpad {
    scratchpad_id: String,
    objective: String,
    status: String,
    session_key: Option<String>,
    #[serde(default)]
    worktrees: Vec<Value>,
    #[serde(default)]
    active_channels: Vec<String>,
    #[serde(default)]
    active_sessions: BTreeMap<String, Value>,
    #[serde(default)]
    action_policy: BTreeMap<String, Value>,
    #[serde(default)]
    completed: Vec<String>,
    #[serde(default)]
    next_steps: Vec<String>,
    #[serde(default)]
    pending_waits: Vec<Value>,
    #[serde(default)]
    git_refs: Vec<Value>,
    #[serde(default)]
    artifacts: Vec<Value>,
    #[serde(default)]
    stop_conditions: Vec<String>,
    #[serde(default)]
    resume_instructions: String,
    #[serde(default)]
    interruption_policy: String,
    #[serde(default)]
    final_guard: String,
    #[serde(default)]
    last_benchmark: Option<Value>,
    #[serde(default)]
    notes: Vec<ScratchpadNote>,
    created_at: String,
    updated_at: String,
    archived_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ScratchpadNote {
    note_id: String,
    ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
}

struct ScratchpadStore {
    entries_dir: PathBuf,
}

impl ScratchpadStore {
    fn new(state_home: Option<&str>, codex_home: &Path) -> Result<Self, FunctionCallError> {
        let root = state_home
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| codex_home.join("scratchpad"));
        Ok(Self {
            entries_dir: root.join("entries"),
        })
    }

    fn path(&self, scratchpad_id: &str) -> Result<PathBuf, FunctionCallError> {
        let safe_id = sanitize_id(scratchpad_id)?;
        Ok(self.entries_dir.join(format!("{safe_id}.json")))
    }

    fn read(&self, scratchpad_id: &str) -> Result<Scratchpad, FunctionCallError> {
        let path = self.path(scratchpad_id)?;
        let text = fs::read_to_string(&path).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "scratchpad `{scratchpad_id}` not found or unreadable: {err}"
            ))
        })?;
        serde_json::from_str(&text).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "scratchpad `{scratchpad_id}` is invalid JSON: {err}"
            ))
        })
    }

    fn write(&self, scratchpad: &Scratchpad) -> Result<(), FunctionCallError> {
        fs::create_dir_all(&self.entries_dir).map_err(io_error)?;
        let path = self.path(&scratchpad.scratchpad_id)?;
        let text = serde_json::to_string_pretty(scratchpad).map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to serialize scratchpad: {err}"))
        })?;
        fs::write(path, format!("{text}\n")).map_err(io_error)
    }

    fn list(&self) -> Result<Vec<Scratchpad>, FunctionCallError> {
        let mut scratchpads = Vec::new();
        match fs::read_dir(&self.entries_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(io_error)?;
                    if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
                        continue;
                    }
                    let text = fs::read_to_string(entry.path()).map_err(io_error)?;
                    let scratchpad = serde_json::from_str::<Scratchpad>(&text).map_err(|err| {
                        FunctionCallError::RespondToModel(format!(
                            "failed to parse scratchpad file `{}`: {err}",
                            entry.path().display()
                        ))
                    })?;
                    scratchpads.push(scratchpad);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error(err)),
        }
        scratchpads.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(scratchpads)
    }
}

fn open_scratchpad(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let objective = string_arg(args, "objective")?;
    let session_key = optional_string_arg(args, "session_key");
    if let Some(existing) = find_existing_active(store, &objective, session_key.as_deref())? {
        return Ok(serde_json::json!({ "scratchpad": existing }));
    }

    let now = now();
    let scratchpad_id = optional_string_arg(args, "scratchpad_id")
        .unwrap_or_else(|| default_scratchpad_id.to_string());
    let mut scratchpad = Scratchpad {
        scratchpad_id,
        objective,
        status: optional_string_arg(args, "status").unwrap_or_else(|| "active".to_string()),
        session_key,
        worktrees: array_arg(args, "worktrees"),
        active_channels: string_array_arg(args, "active_channels"),
        active_sessions: object_arg(args, "active_sessions"),
        action_policy: object_arg(args, "action_policy"),
        completed: string_array_arg(args, "completed"),
        next_steps: string_array_arg(args, "next_steps"),
        pending_waits: array_arg(args, "pending_waits"),
        git_refs: array_arg(args, "git_refs"),
        artifacts: array_arg(args, "artifacts"),
        stop_conditions: string_array_arg(args, "stop_conditions"),
        resume_instructions: optional_string_arg(args, "resume_instructions").unwrap_or_default(),
        interruption_policy: optional_string_arg(args, "interruption_policy").unwrap_or_default(),
        final_guard: optional_string_arg(args, "final_guard").unwrap_or_default(),
        last_benchmark: args.get("last_benchmark").cloned(),
        notes: Vec::new(),
        created_at: now.clone(),
        updated_at: now,
        archived_at: None,
    };
    merge_update(&mut scratchpad, args);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn find_existing_active(
    store: &ScratchpadStore,
    objective: &str,
    session_key: Option<&str>,
) -> Result<Option<Scratchpad>, FunctionCallError> {
    Ok(store.list()?.into_iter().find(|scratchpad| {
        scratchpad.archived_at.is_none()
            && scratchpad.objective == objective
            && scratchpad.session_key.as_deref() == session_key
    }))
}

fn get_scratchpad(store: &ScratchpadStore, args: &Value) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    Ok(serde_json::json!({ "scratchpad": store.read(&scratchpad_id)? }))
}

fn get_scratchpad_summary(
    store: &ScratchpadStore,
    args: &Value,
) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let scratchpad = store.read(&scratchpad_id)?;
    Ok(serde_json::json!({ "summary": summary(&scratchpad) }))
}

fn append_scratchpad_note(
    store: &ScratchpadStore,
    args: &Value,
) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    scratchpad.notes.push(ScratchpadNote {
        note_id: format!("note-{}", uuid::Uuid::new_v4().simple()),
        ts: now(),
        category: optional_string_arg(args, "category"),
        summary: string_arg(args, "summary")?,
        outcome: optional_string_arg(args, "outcome"),
    });
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn set_next_steps(store: &ScratchpadStore, args: &Value) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    scratchpad.next_steps = string_array_arg(args, "next_steps");
    if let Some(status) = optional_string_arg(args, "status") {
        scratchpad.status = status;
    }
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn set_pending_waits(store: &ScratchpadStore, args: &Value) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    scratchpad.pending_waits = array_arg(args, "pending_waits");
    if let Some(status) = optional_string_arg(args, "status") {
        scratchpad.status = status;
    }
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn update_scratchpad(store: &ScratchpadStore, args: &Value) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    merge_update(&mut scratchpad, args);
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn archive_scratchpad(store: &ScratchpadStore, args: &Value) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    let now = now();
    scratchpad.status =
        optional_string_arg(args, "status").unwrap_or_else(|| "archived".to_string());
    scratchpad.archived_at = Some(now.clone());
    scratchpad.updated_at = now;
    if let Some(summary) = optional_string_arg(args, "summary") {
        scratchpad.notes.push(ScratchpadNote {
            note_id: format!("note-{}", uuid::Uuid::new_v4().simple()),
            ts: scratchpad.updated_at.clone(),
            category: Some("archive".to_string()),
            summary,
            outcome: optional_string_arg(args, "outcome"),
        });
    }
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn unarchive_scratchpad(store: &ScratchpadStore, args: &Value) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    scratchpad.status = optional_string_arg(args, "status").unwrap_or_else(|| "active".to_string());
    scratchpad.archived_at = None;
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn lookup_scratchpads(store: &ScratchpadStore, args: &Value) -> Result<Value, FunctionCallError> {
    let query = optional_string_arg(args, "query")
        .or_else(|| optional_string_arg(args, "objective"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let include_archived = bool_arg(args, "include_archived");
    let limit = usize_arg(args, "limit").unwrap_or(20).clamp(1, 100);
    let matches = store
        .list()?
        .into_iter()
        .filter(|scratchpad| include_archived || scratchpad.archived_at.is_none())
        .filter(|scratchpad| {
            query.is_empty()
                || scratchpad
                    .scratchpad_id
                    .to_ascii_lowercase()
                    .contains(&query)
                || scratchpad.objective.to_ascii_lowercase().contains(&query)
                || scratchpad
                    .session_key
                    .as_deref()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .contains(&query)
                || scratchpad.status.to_ascii_lowercase().contains(&query)
        })
        .take(limit)
        .map(|scratchpad| summary(&scratchpad))
        .collect::<Vec<_>>();
    Ok(serde_json::json!({ "matches": matches }))
}

fn check_action_allowed(store: &ScratchpadStore, args: &Value) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let scratchpad = store.read(&scratchpad_id)?;
    let action = optional_string_arg(args, "action").unwrap_or_default();
    let denied = scratchpad
        .action_policy
        .get("deny")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .any(|item| item.eq_ignore_ascii_case(&action))
        });
    Ok(serde_json::json!({
        "allowed": !denied,
        "scratchpad_id": scratchpad.scratchpad_id,
        "action": action,
    }))
}

fn schema_payload() -> Value {
    serde_json::json!({
        "required": ["scratchpad_id", "objective", "status", "created_at", "updated_at"],
        "optional": [
            "session_key",
            "worktrees",
            "active_channels",
            "active_sessions",
            "action_policy",
            "completed",
            "next_steps",
            "pending_waits",
            "git_refs",
            "artifacts",
            "stop_conditions",
            "resume_instructions",
            "interruption_policy",
            "final_guard",
            "last_benchmark",
            "notes",
            "archived_at"
        ],
        "storage": "Built-in scratchpads are JSON files under <codex_home>/scratchpad/entries unless state_home is provided.",
        "thread_id_default": "open_scratchpad defaults scratchpad_id to the current Codex thread/session id when scratchpad_id is omitted.",
        "tools": [
            TOOL_OPEN,
            TOOL_GET,
            TOOL_SUMMARY,
            TOOL_APPEND_NOTE,
            TOOL_SET_NEXT_STEPS,
            TOOL_SET_PENDING_WAITS,
            TOOL_UPDATE,
            TOOL_ARCHIVE,
            TOOL_UNARCHIVE,
            TOOL_LOOKUP,
            TOOL_SCHEMA,
            TOOL_CHECK_ACTION
        ]
    })
}

fn merge_update(scratchpad: &mut Scratchpad, args: &Value) {
    if let Some(status) = optional_string_arg(args, "status") {
        scratchpad.status = status;
    }
    if let Some(resume_instructions) = optional_string_arg(args, "resume_instructions") {
        scratchpad.resume_instructions = resume_instructions;
    }
    if let Some(interruption_policy) = optional_string_arg(args, "interruption_policy") {
        scratchpad.interruption_policy = interruption_policy;
    }
    if let Some(final_guard) = optional_string_arg(args, "final_guard") {
        scratchpad.final_guard = final_guard;
    }
    if args.get("worktrees").is_some() {
        scratchpad.worktrees = array_arg(args, "worktrees");
    }
    if args.get("active_channels").is_some() {
        scratchpad.active_channels = string_array_arg(args, "active_channels");
    }
    if args.get("active_sessions").is_some() {
        scratchpad.active_sessions = object_arg(args, "active_sessions");
    }
    if args.get("action_policy").is_some() {
        scratchpad.action_policy = object_arg(args, "action_policy");
    }
    if args.get("completed").is_some() {
        scratchpad.completed = string_array_arg(args, "completed");
    }
    if args.get("next_steps").is_some() {
        scratchpad.next_steps = string_array_arg(args, "next_steps");
    }
    if args.get("pending_waits").is_some() {
        scratchpad.pending_waits = array_arg(args, "pending_waits");
    }
    if args.get("git_refs").is_some() {
        scratchpad.git_refs = array_arg(args, "git_refs");
    }
    if args.get("artifacts").is_some() {
        scratchpad.artifacts = array_arg(args, "artifacts");
    }
    if args.get("stop_conditions").is_some() {
        scratchpad.stop_conditions = string_array_arg(args, "stop_conditions");
    }
    if args.get("last_benchmark").is_some() {
        scratchpad.last_benchmark = args.get("last_benchmark").cloned();
    }
}

fn summary(scratchpad: &Scratchpad) -> Value {
    serde_json::json!({
        "scratchpad_id": scratchpad.scratchpad_id,
        "objective": scratchpad.objective,
        "status": scratchpad.status,
        "session_key": scratchpad.session_key,
        "next_steps": scratchpad.next_steps,
        "pending_waits": scratchpad.pending_waits,
        "stop_conditions": scratchpad.stop_conditions,
        "resume_instructions": scratchpad.resume_instructions,
        "final_guard": scratchpad.final_guard,
        "notes_count": scratchpad.notes.len(),
        "updated_at": scratchpad.updated_at,
        "archived_at": scratchpad.archived_at,
    })
}

fn sanitize_id(value: &str) -> Result<String, FunctionCallError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "scratchpad_id must not be empty".to_string(),
        ));
    }
    Ok(value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect())
}

fn string_arg(args: &Value, key: &str) -> Result<String, FunctionCallError> {
    optional_string_arg(args, key).ok_or_else(|| {
        FunctionCallError::RespondToModel(format!("missing required argument `{key}`"))
    })
}

fn optional_string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn array_arg(args: &Value, key: &str) -> Vec<Value> {
    args.get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn string_array_arg(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn object_arg(args: &Value, key: &str) -> BTreeMap<String, Value> {
    args.get(key)
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn usize_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn touch(scratchpad: &mut Scratchpad) {
    scratchpad.updated_at = now();
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn io_error(err: std::io::Error) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!("scratchpad storage error: {err}"))
}

fn json_text(value: Value) -> Result<String, FunctionCallError> {
    serde_json::to_string_pretty(&value).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize result: {err}"))
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn open_defaults_to_thread_id_and_reopens_existing_active_pad() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(Some(tmp.path().to_str().unwrap()), tmp.path()).unwrap();
        let args = serde_json::json!({
            "objective": "test objective",
            "session_key": "session-a",
            "next_steps": ["ship it"]
        });

        let first = open_scratchpad(&store, &args, "thread-123").unwrap();
        let second = open_scratchpad(&store, &args, "thread-456").unwrap();

        assert_eq!(
            first["scratchpad"]["scratchpad_id"],
            serde_json::json!("thread-123")
        );
        assert_eq!(
            second["scratchpad"]["scratchpad_id"],
            first["scratchpad"]["scratchpad_id"]
        );
    }

    #[test]
    fn archive_lookup_and_unarchive_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(Some(tmp.path().to_str().unwrap()), tmp.path()).unwrap();
        let args = serde_json::json!({ "objective": "recover me", "scratchpad_id": "sp-test" });
        open_scratchpad(&store, &args, "thread-123").unwrap();

        archive_scratchpad(
            &store,
            &serde_json::json!({ "scratchpad_id": "sp-test", "summary": "done" }),
        )
        .unwrap();
        let active_lookup =
            lookup_scratchpads(&store, &serde_json::json!({ "query": "recover" })).unwrap();
        let archived_lookup = lookup_scratchpads(
            &store,
            &serde_json::json!({ "query": "recover", "include_archived": true }),
        )
        .unwrap();

        assert_eq!(active_lookup["matches"].as_array().unwrap().len(), 0);
        assert_eq!(archived_lookup["matches"].as_array().unwrap().len(), 1);

        unarchive_scratchpad(
            &store,
            &serde_json::json!({ "scratchpad_id": "sp-test", "status": "active" }),
        )
        .unwrap();
        let active_lookup =
            lookup_scratchpads(&store, &serde_json::json!({ "query": "recover" })).unwrap();
        assert_eq!(active_lookup["matches"].as_array().unwrap().len(), 1);
    }
}
