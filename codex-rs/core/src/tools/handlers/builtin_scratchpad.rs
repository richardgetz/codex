use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Duration;
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
const TOOL_RESUME: &str = "resume_scratchpad";
const TOOL_GET: &str = "get_scratchpad";
const TOOL_SUMMARY: &str = "get_scratchpad_summary";
const TOOL_APPEND_NOTE: &str = "append_scratchpad_note";
const TOOL_SET_NEXT_STEPS: &str = "set_next_steps";
const TOOL_SET_PENDING_WAITS: &str = "set_pending_waits";
const TOOL_SET_ACTION_POLICY: &str = "set_action_policy";
const TOOL_MARK_WAIT_CHECKED: &str = "mark_wait_checked";
const TOOL_UPDATE: &str = "update_scratchpad";
const TOOL_ARCHIVE: &str = "archive_scratchpad";
const TOOL_UNARCHIVE: &str = "unarchive_scratchpad";
const TOOL_LOOKUP: &str = "lookup_scratchpads";
const TOOL_SCHEMA: &str = "get_scratchpad_schema";
const TOOL_CHECK_ACTION: &str = "check_action_allowed";

pub(crate) const BUILTIN_SCRATCHPAD_TOOL_NAMES: &[&str] = &[
    TOOL_OPEN,
    TOOL_RESUME,
    TOOL_GET,
    TOOL_SUMMARY,
    TOOL_APPEND_NOTE,
    TOOL_SET_NEXT_STEPS,
    TOOL_SET_PENDING_WAITS,
    TOOL_SET_ACTION_POLICY,
    TOOL_MARK_WAIT_CHECKED,
    TOOL_UPDATE,
    TOOL_ARCHIVE,
    TOOL_UNARCHIVE,
    TOOL_LOOKUP,
    TOOL_SCHEMA,
    TOOL_CHECK_ACTION,
];

pub(crate) fn scratchpad_namespace_spec() -> ToolSpec {
    let tools = [
        (
            TOOL_OPEN,
            "Open an existing active scratchpad for the same objective/session, or create one.",
        ),
        (
            TOOL_RESUME,
            "Resume an existing scratchpad by id without creating a new one.",
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
            TOOL_RESUME | TOOL_GET | TOOL_SUMMARY | TOOL_LOOKUP | TOOL_SCHEMA | TOOL_CHECK_ACTION
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
            TOOL_RESUME => resume_scratchpad(&store, &args),
            TOOL_GET => get_scratchpad(&store, &args),
            TOOL_SUMMARY => get_scratchpad_summary(&store, &args),
            TOOL_APPEND_NOTE => append_scratchpad_note(&store, &args),
            TOOL_SET_NEXT_STEPS => set_next_steps(&store, &args),
            TOOL_SET_PENDING_WAITS => set_pending_waits(&store, &args),
            TOOL_SET_ACTION_POLICY => set_action_policy(&store, &args),
            TOOL_MARK_WAIT_CHECKED => mark_wait_checked(&store, &args),
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
    root: PathBuf,
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
            root,
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
        fs::write(path, format!("{text}\n")).map_err(io_error)?;
        self.write_index()
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

    fn remove(&self, scratchpad_id: &str) -> Result<(), FunctionCallError> {
        let path = self.path(scratchpad_id)?;
        fs::remove_file(path).map_err(io_error)?;
        self.write_index()
    }

    fn write_index(&self) -> Result<(), FunctionCallError> {
        fs::create_dir_all(&self.root).map_err(io_error)?;
        let entries: Vec<ScratchpadIndexEntry> = self
            .list()?
            .into_iter()
            .map(|scratchpad| ScratchpadIndexEntry {
                scratchpad_id: scratchpad.scratchpad_id,
                objective: scratchpad.objective,
                status: scratchpad.status,
                session_key: scratchpad.session_key,
                created_at: scratchpad.created_at,
                updated_at: scratchpad.updated_at,
                archived_at: scratchpad.archived_at,
            })
            .collect();
        let index = ScratchpadIndex {
            generated_at: now(),
            entries,
        };
        let text = serde_json::to_string_pretty(&index).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to serialize scratchpad index: {err}"
            ))
        })?;
        fs::write(self.root.join("index.json"), format!("{text}\n")).map_err(io_error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ScratchpadIndex {
    generated_at: String,
    entries: Vec<ScratchpadIndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ScratchpadIndexEntry {
    scratchpad_id: String,
    objective: String,
    status: String,
    session_key: Option<String>,
    created_at: String,
    updated_at: String,
    archived_at: Option<String>,
}

/// Archives inactive built-in scratchpads and deletes old archived scratchpads.
///
/// `*_after_days = 0` disables the corresponding cleanup phase.
pub(crate) fn run_lifecycle_cleanup(
    codex_home: &Path,
    auto_archive_after_days: u64,
    delete_archived_after_days: u64,
) -> Result<(), FunctionCallError> {
    if auto_archive_after_days == 0 && delete_archived_after_days == 0 {
        return Ok(());
    }

    let store = ScratchpadStore::new(/*state_home*/ None, codex_home)?;
    let now = Utc::now();
    for mut scratchpad in store.list()? {
        if scratchpad.archived_at.is_some() {
            if delete_archived_after_days > 0
                && let Some(archived_at) = parse_timestamp(&scratchpad.archived_at)
                && now.signed_duration_since(archived_at)
                    >= Duration::days(days_to_i64(delete_archived_after_days))
            {
                store.remove(&scratchpad.scratchpad_id)?;
            }
            continue;
        }

        if auto_archive_after_days > 0
            && let Some(updated_at) = parse_timestamp(&Some(scratchpad.updated_at.clone()))
            && now.signed_duration_since(updated_at)
                >= Duration::days(days_to_i64(auto_archive_after_days))
        {
            let archived_at = now.to_rfc3339();
            scratchpad.status = "archived".to_string();
            scratchpad.archived_at = Some(archived_at.clone());
            scratchpad.updated_at = archived_at.clone();
            scratchpad.notes.push(ScratchpadNote {
                note_id: format!("note-{}", uuid::Uuid::new_v4().simple()),
                ts: archived_at,
                category: Some("lifecycle".to_string()),
                summary: format!(
                    "Automatically archived after {auto_archive_after_days} days without updates."
                ),
                outcome: Some("archived".to_string()),
            });
            store.write(&scratchpad)?;
        }
    }
    store.write_index()
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

fn resume_scratchpad(store: &ScratchpadStore, args: &Value) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let scratchpad = store.read(&scratchpad_id)?;
    let include_archived = bool_arg(args, "include_archived");
    if scratchpad.archived_at.is_some() && !include_archived {
        return Err(FunctionCallError::RespondToModel(format!(
            "scratchpad `{scratchpad_id}` is archived; pass include_archived=true to inspect it or unarchive_scratchpad to reactivate it"
        )));
    }
    Ok(serde_json::json!({
        "scratchpad": scratchpad,
        "summary": summary(&scratchpad),
        "resumed": true,
    }))
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

fn set_action_policy(store: &ScratchpadStore, args: &Value) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    scratchpad.action_policy = object_arg(args, "action_policy");
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn mark_wait_checked(store: &ScratchpadStore, args: &Value) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    let wait_id = optional_string_arg(args, "wait_id");
    let target = optional_string_arg(args, "target");
    let index = find_pending_wait_index(
        &scratchpad.pending_waits,
        wait_id.as_deref(),
        target.as_deref(),
    )?;
    if bool_arg(args, "resolved") {
        scratchpad.pending_waits.remove(index);
    } else {
        let mut wait = scratchpad.pending_waits[index]
            .as_object()
            .cloned()
            .ok_or_else(|| {
                FunctionCallError::RespondToModel("pending wait is not an object".to_string())
            })?;
        wait.insert("last_checked_at".to_string(), serde_json::json!(now()));
        for key in ["next_check_at", "reuse_session_id", "check_method"] {
            if let Some(value) = args.get(key) {
                wait.insert(key.to_string(), value.clone());
            }
        }
        if let Some(value) = args.get("fallback_work") {
            wait.insert("fallback_work".to_string(), value.clone());
        }
        scratchpad.pending_waits[index] = serde_json::Value::Object(wait);
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
    let action = string_arg(args, "action")?.to_ascii_lowercase();
    let decision = evaluate_action_policy(&scratchpad, args, &action);
    Ok(serde_json::json!({
        "allowed": decision.allowed,
        "reason": decision.reason,
        "message": decision.message,
        "decision": {
            "allowed": decision.allowed,
            "reason": decision.reason,
            "message": decision.message,
            "scratchpad_id": scratchpad.scratchpad_id,
            "status": scratchpad.status,
            "action": action,
            "evaluated_policy": decision.evaluated_policy,
        }
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
            TOOL_RESUME,
            TOOL_GET,
            TOOL_SUMMARY,
            TOOL_APPEND_NOTE,
            TOOL_SET_NEXT_STEPS,
            TOOL_SET_PENDING_WAITS,
            TOOL_SET_ACTION_POLICY,
            TOOL_MARK_WAIT_CHECKED,
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

struct ActionPolicyDecision {
    allowed: bool,
    reason: String,
    message: String,
    evaluated_policy: Value,
}

fn evaluate_action_policy(
    scratchpad: &Scratchpad,
    args: &Value,
    action: &str,
) -> ActionPolicyDecision {
    let policy = &scratchpad.action_policy;
    let effective_policy = effective_action_policy(policy, args);
    let mut reason = None;
    let mut message = None;

    if action == "finalize"
        && matches!(scratchpad.status.as_str(), "active" | "waiting")
        && policy_bool(
            &effective_policy,
            "guard_finalization_while_active",
            /*default*/ true,
        )
    {
        reason = Some("objective_in_progress".to_string());
        message = Some(format!(
            "cannot finalize while scratchpad status is '{}'",
            scratchpad.status
        ));
    }

    if reason.is_none()
        && action == "end_followup"
        && matches!(scratchpad.status.as_str(), "active" | "waiting")
        && policy_bool(
            &effective_policy,
            "guard_followup_shutdown_while_active",
            /*default*/ true,
        )
    {
        let channel =
            optional_string_arg(args, "channel").unwrap_or_else(|| "follow-up".to_string());
        reason = Some("objective_in_progress".to_string());
        message = Some(format!(
            "cannot end {channel} while scratchpad status is '{}'",
            scratchpad.status
        ));
    }

    if reason.is_none() && matches!(action, "merge" | "pull_request") {
        if bool_arg(args, "bypass_pr_requirements")
            && !policy_bool(
                &effective_policy,
                "allow_pr_requirement_bypass",
                /*default*/ false,
            )
        {
            reason = Some("pr_bypass_forbidden".to_string());
            message = Some("PR requirement bypass is forbidden by action policy".to_string());
        }
        let branch = optional_string_arg(args, "target_branch").unwrap_or_default();
        let forbidden = policy_list(&effective_policy, &["forbidden_base_branches"]);
        let allowed = policy_list(&effective_policy, &["allowed_base_branches"]);
        if reason.is_none() && branch.is_empty() && (!forbidden.is_empty() || !allowed.is_empty()) {
            reason = Some("target_branch_required".to_string());
            message =
                Some("target_branch is required when branch policy is configured".to_string());
        } else if reason.is_none() && !branch.is_empty() {
            if forbidden.iter().any(|item| item == &branch) {
                reason = Some("branch_forbidden".to_string());
                message = Some(format!("target branch '{branch}' is forbidden"));
            } else if !allowed.is_empty() && !allowed.iter().any(|item| item == &branch) {
                reason = Some("branch_not_allowed".to_string());
                message = Some(format!("target branch '{branch}' is not allowed"));
            }
        }
    }

    if reason.is_none() && matches!(action, "deploy" | "ecs_benchmark_launch") {
        let env = optional_string_arg(args, "env").unwrap_or_default();
        if !env.is_empty() {
            let (forbidden_keys, allowed_keys): (&[&str], &[&str]) =
                if action == "ecs_benchmark_launch" {
                    (
                        &["forbidden_benchmark_envs", "forbidden_envs"],
                        &["allowed_benchmark_envs", "allowed_envs"],
                    )
                } else {
                    (
                        &["forbidden_deploy_envs", "forbidden_envs"],
                        &["allowed_deploy_envs", "allowed_envs"],
                    )
                };
            let forbidden = policy_list(&effective_policy, forbidden_keys);
            let allowed = policy_list(&effective_policy, allowed_keys);
            if forbidden.iter().any(|item| item == &env) {
                reason = Some("env_forbidden".to_string());
                message = Some(format!(
                    "target env '{env}' is forbidden for action '{action}'"
                ));
            } else if !allowed.is_empty() && !allowed.iter().any(|item| item == &env) {
                reason = Some("env_not_allowed".to_string());
                message = Some(format!(
                    "target env '{env}' is not allowed for action '{action}'"
                ));
            }
        }
    }

    if reason.is_none()
        && action == "aws_write"
        && !policy_bool(
            &effective_policy,
            "allow_aws_writes",
            /*default*/ false,
        )
    {
        reason = Some("aws_write_forbidden".to_string());
        message = Some("AWS write path is forbidden by action policy".to_string());
    }

    let allowed = reason.is_none();
    ActionPolicyDecision {
        allowed,
        reason: reason.unwrap_or_else(|| "allowed".to_string()),
        message: message
            .unwrap_or_else(|| "action is allowed by current scratchpad policy".to_string()),
        evaluated_policy: effective_policy,
    }
}

fn effective_action_policy(policy: &BTreeMap<String, Value>, args: &Value) -> Value {
    let mut effective = serde_json::Map::new();
    if let Some(defaults) = policy.get("defaults").and_then(Value::as_object) {
        effective.extend(defaults.clone());
    }
    if let Some(repo_policy) = resolve_repo_policy(policy, args) {
        effective.extend(repo_policy.clone());
    }
    Value::Object(effective)
}

fn resolve_repo_policy<'a>(
    policy: &'a BTreeMap<String, Value>,
    args: &Value,
) -> Option<&'a serde_json::Map<String, Value>> {
    let repo = optional_string_arg(args, "repo")?;
    let repos = policy.get("repos").and_then(Value::as_object)?;
    repos.iter().find_map(|(key, value)| {
        (repo == *key || repo.ends_with(key))
            .then(|| value.as_object())
            .flatten()
    })
}

fn policy_bool(policy: &Value, key: &str, default: bool) -> bool {
    policy.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn policy_list(policy: &Value, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .find_map(|key| policy.get(*key).and_then(Value::as_array))
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn find_pending_wait_index(
    waits: &[Value],
    wait_id: Option<&str>,
    target: Option<&str>,
) -> Result<usize, FunctionCallError> {
    let wait_id = wait_id.unwrap_or_default();
    let target = target.unwrap_or_default();
    if wait_id.is_empty() && target.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "mark_wait_checked requires wait_id or target".to_string(),
        ));
    }
    waits
        .iter()
        .position(|wait| {
            wait.get("wait_id").and_then(Value::as_str) == Some(wait_id)
                || wait.get("target").and_then(Value::as_str) == Some(target)
        })
        .ok_or_else(|| FunctionCallError::RespondToModel("pending wait not found".to_string()))
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

fn parse_timestamp(timestamp: &Option<String>) -> Option<DateTime<Utc>> {
    timestamp
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(DateTime::<Utc>::from)
}

fn days_to_i64(days: u64) -> i64 {
    i64::try_from(days).unwrap_or(i64::MAX / 86_400)
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
        let store = ScratchpadStore::new(
            /*state_home*/ Some(tmp.path().to_str().unwrap()),
            tmp.path(),
        )
        .unwrap();
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
        let store = ScratchpadStore::new(
            /*state_home*/ Some(tmp.path().to_str().unwrap()),
            tmp.path(),
        )
        .unwrap();
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

    #[test]
    fn resume_requires_existing_pad_and_rejects_archived_by_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(Some(tmp.path().to_str().unwrap()), tmp.path()).unwrap();
        let args = serde_json::json!({
            "objective": "resume me",
            "scratchpad_id": "sp-resume",
            "next_steps": ["continue"]
        });
        open_scratchpad(&store, &args, "thread-123").unwrap();

        let resumed =
            resume_scratchpad(&store, &serde_json::json!({ "scratchpad_id": "sp-resume" }))
                .unwrap();
        assert_eq!(
            resumed["summary"]["next_steps"],
            serde_json::json!(["continue"])
        );
        assert_eq!(resumed["resumed"], serde_json::json!(true));

        archive_scratchpad(&store, &serde_json::json!({ "scratchpad_id": "sp-resume" })).unwrap();
        let archived_default =
            resume_scratchpad(&store, &serde_json::json!({ "scratchpad_id": "sp-resume" }));
        assert!(archived_default.is_err());

        let archived_explicit = resume_scratchpad(
            &store,
            &serde_json::json!({ "scratchpad_id": "sp-resume", "include_archived": true }),
        )
        .unwrap();
        assert!(archived_explicit["summary"]["archived_at"].is_string());
    }

    #[test]
    fn lifecycle_cleanup_archives_inactive_and_deletes_old_archived_scratchpads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(/*state_home*/ None, tmp.path()).unwrap();
        let old_updated_at = (Utc::now() - Duration::days(31)).to_rfc3339();
        let old_archived_at = (Utc::now() - Duration::days(91)).to_rfc3339();
        let recent_archived_at = (Utc::now() - Duration::days(10)).to_rfc3339();

        store
            .write(&Scratchpad {
                scratchpad_id: "sp-inactive".to_string(),
                objective: "inactive".to_string(),
                status: "active".to_string(),
                session_key: None,
                worktrees: Vec::new(),
                active_channels: Vec::new(),
                active_sessions: BTreeMap::new(),
                action_policy: BTreeMap::new(),
                completed: Vec::new(),
                next_steps: Vec::new(),
                pending_waits: Vec::new(),
                git_refs: Vec::new(),
                artifacts: Vec::new(),
                stop_conditions: Vec::new(),
                resume_instructions: String::new(),
                interruption_policy: String::new(),
                final_guard: String::new(),
                last_benchmark: None,
                notes: Vec::new(),
                created_at: old_updated_at.clone(),
                updated_at: old_updated_at,
                archived_at: None,
            })
            .unwrap();
        store
            .write(&Scratchpad {
                scratchpad_id: "sp-old-archived".to_string(),
                objective: "delete me".to_string(),
                status: "done".to_string(),
                session_key: None,
                worktrees: Vec::new(),
                active_channels: Vec::new(),
                active_sessions: BTreeMap::new(),
                action_policy: BTreeMap::new(),
                completed: Vec::new(),
                next_steps: Vec::new(),
                pending_waits: Vec::new(),
                git_refs: Vec::new(),
                artifacts: Vec::new(),
                stop_conditions: Vec::new(),
                resume_instructions: String::new(),
                interruption_policy: String::new(),
                final_guard: String::new(),
                last_benchmark: None,
                notes: Vec::new(),
                created_at: old_archived_at.clone(),
                updated_at: old_archived_at.clone(),
                archived_at: Some(old_archived_at),
            })
            .unwrap();
        store
            .write(&Scratchpad {
                scratchpad_id: "sp-recent-archived".to_string(),
                objective: "keep me".to_string(),
                status: "done".to_string(),
                session_key: None,
                worktrees: Vec::new(),
                active_channels: Vec::new(),
                active_sessions: BTreeMap::new(),
                action_policy: BTreeMap::new(),
                completed: Vec::new(),
                next_steps: Vec::new(),
                pending_waits: Vec::new(),
                git_refs: Vec::new(),
                artifacts: Vec::new(),
                stop_conditions: Vec::new(),
                resume_instructions: String::new(),
                interruption_policy: String::new(),
                final_guard: String::new(),
                last_benchmark: None,
                notes: Vec::new(),
                created_at: recent_archived_at.clone(),
                updated_at: recent_archived_at.clone(),
                archived_at: Some(recent_archived_at),
            })
            .unwrap();

        run_lifecycle_cleanup(
            tmp.path(),
            /*auto_archive_after_days*/ 30,
            /*delete_archived_after_days*/ 90,
        )
        .unwrap();

        let inactive = store.read("sp-inactive").unwrap();
        assert_eq!(inactive.status, "archived");
        assert!(inactive.archived_at.is_some());
        assert_eq!(inactive.notes.len(), 1);
        assert!(store.read("sp-old-archived").is_err());
        assert!(store.read("sp-recent-archived").is_ok());

        let index_text = fs::read_to_string(tmp.path().join("scratchpad/index.json")).unwrap();
        let index: ScratchpadIndex = serde_json::from_str(&index_text).unwrap();
        let index_ids: Vec<String> = index
            .entries
            .into_iter()
            .map(|entry| entry.scratchpad_id)
            .collect();
        assert_eq!(
            index_ids,
            vec!["sp-inactive".to_string(), "sp-recent-archived".to_string()]
        );
    }

    #[test]
    fn action_policy_checks_effective_repo_policy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(Some(tmp.path().to_str().unwrap()), tmp.path()).unwrap();
        open_scratchpad(
            &store,
            &serde_json::json!({
                "objective": "guard release",
                "scratchpad_id": "sp-policy",
                "status": "active",
                "action_policy": {
                    "defaults": {
                        "forbidden_base_branches": ["main"],
                        "allow_aws_writes": false
                    },
                    "repos": {
                        "codex": {
                            "allowed_deploy_envs": ["dev"]
                        }
                    }
                }
            }),
            "thread-123",
        )
        .unwrap();

        let merge_decision = check_action_allowed(
            &store,
            &serde_json::json!({
                "scratchpad_id": "sp-policy",
                "action": "merge",
                "target_branch": "main"
            }),
        )
        .unwrap();
        let deploy_decision = check_action_allowed(
            &store,
            &serde_json::json!({
                "scratchpad_id": "sp-policy",
                "action": "deploy",
                "repo": "/Users/example/codex",
                "env": "prod"
            }),
        )
        .unwrap();

        assert_eq!(merge_decision["allowed"], serde_json::json!(false));
        assert_eq!(
            merge_decision["reason"],
            serde_json::json!("branch_forbidden")
        );
        assert_eq!(deploy_decision["allowed"], serde_json::json!(false));
        assert_eq!(
            deploy_decision["reason"],
            serde_json::json!("env_not_allowed")
        );
    }

    #[test]
    fn mark_wait_checked_updates_or_resolves_pending_wait() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(Some(tmp.path().to_str().unwrap()), tmp.path()).unwrap();
        open_scratchpad(
            &store,
            &serde_json::json!({
                "objective": "watch ci",
                "scratchpad_id": "sp-wait",
                "pending_waits": [
                    {"wait_id": "ci-1", "target": "ci", "status": "pending"}
                ]
            }),
            "thread-123",
        )
        .unwrap();

        let checked = mark_wait_checked(
            &store,
            &serde_json::json!({
                "scratchpad_id": "sp-wait",
                "wait_id": "ci-1",
                "next_check_at": "2026-04-27T12:00:00Z",
                "reuse_session_id": "agent-1"
            }),
        )
        .unwrap();
        assert!(
            checked["scratchpad"]["pending_waits"][0]["last_checked_at"].is_string(),
            "expected wait check timestamp"
        );
        assert_eq!(
            checked["scratchpad"]["pending_waits"][0]["reuse_session_id"],
            serde_json::json!("agent-1")
        );

        let resolved = mark_wait_checked(
            &store,
            &serde_json::json!({
                "scratchpad_id": "sp-wait",
                "target": "ci",
                "resolved": true
            }),
        )
        .unwrap();
        assert_eq!(
            resolved["scratchpad"]["pending_waits"],
            serde_json::json!([])
        );
    }
}
