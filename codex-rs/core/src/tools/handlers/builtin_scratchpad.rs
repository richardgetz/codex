use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

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
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ScratchpadUpdateEvent;

const SCRATCHPAD_NAMESPACE: &str = "scratchpad";
static SCRATCHPAD_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
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
const TOOL_RECORD_OUTCOME: &str = "record_outcome";
const TOOL_EXPORT_OUTCOMES: &str = "export_outcomes";
const TOOL_RECORD_DELEGATION: &str = "record_delegation";

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
    TOOL_RECORD_OUTCOME,
    TOOL_EXPORT_OUTCOMES,
    TOOL_RECORD_DELEGATION,
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
            TOOL_RESUME
                | TOOL_GET
                | TOOL_SUMMARY
                | TOOL_LOOKUP
                | TOOL_SCHEMA
                | TOOL_CHECK_ACTION
                | TOOL_EXPORT_OUTCOMES
        )
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            tool_name,
            ..
        } = invocation;
        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "scratchpad handler received unsupported payload".to_string(),
                ));
            }
        };
        let args: Value = parse_arguments(&arguments)?;
        let config = session.get_config().await;
        let store = ScratchpadStore::new(
            args.get("state_home").and_then(Value::as_str),
            config.codex_home.as_path(),
        )?;
        let default_scratchpad_id = session.conversation_id.to_string();

        let tool_name = tool_name.name.as_str();
        let result = match tool_name {
            TOOL_OPEN => open_scratchpad(&store, &args, &default_scratchpad_id),
            TOOL_RESUME => resume_scratchpad(&store, &args, &default_scratchpad_id),
            TOOL_GET => get_scratchpad(&store, &args, &default_scratchpad_id),
            TOOL_SUMMARY => get_scratchpad_summary(&store, &args, &default_scratchpad_id),
            TOOL_APPEND_NOTE => append_scratchpad_note(&store, &args, &default_scratchpad_id),
            TOOL_SET_NEXT_STEPS => set_next_steps(&store, &args, &default_scratchpad_id),
            TOOL_SET_PENDING_WAITS => set_pending_waits(&store, &args, &default_scratchpad_id),
            TOOL_SET_ACTION_POLICY => set_action_policy(&store, &args, &default_scratchpad_id),
            TOOL_MARK_WAIT_CHECKED => mark_wait_checked(&store, &args, &default_scratchpad_id),
            TOOL_UPDATE => update_scratchpad(&store, &args, &default_scratchpad_id),
            TOOL_ARCHIVE => archive_scratchpad(&store, &args, &default_scratchpad_id),
            TOOL_UNARCHIVE => unarchive_scratchpad(&store, &args, &default_scratchpad_id),
            TOOL_LOOKUP => lookup_scratchpads(&store, &args, &default_scratchpad_id),
            TOOL_SCHEMA => Ok(schema_payload()),
            TOOL_CHECK_ACTION => check_action_allowed(&store, &args, &default_scratchpad_id),
            TOOL_RECORD_OUTCOME => {
                if !config.scratchpad.outcomes_enabled {
                    Err(FunctionCallError::RespondToModel(
                        "scratchpad outcome tracking is disabled; enable it with `/outcomes on` or `[scratchpad].outcomes_enabled = true` before recording outcomes.".to_string(),
                    ))
                } else {
                    record_outcome(&store, &args, &default_scratchpad_id)
                }
            }
            TOOL_EXPORT_OUTCOMES => export_outcomes(&store, &args, &default_scratchpad_id),
            TOOL_RECORD_DELEGATION => record_delegation(&store, &args, &default_scratchpad_id),
            tool_name => Err(FunctionCallError::RespondToModel(format!(
                "unknown scratchpad tool: {tool_name}"
            ))),
        }?;

        if should_emit_scratchpad_update(tool_name)
            && let Some(event) = scratchpad_update_event_from_result(&result)
        {
            session
                .send_event(turn.as_ref(), EventMsg::ScratchpadUpdate(event))
                .await;
        }

        Ok(FunctionToolOutput::from_text(
            json_text(result)?,
            Some(true),
        ))
    }
}

fn should_emit_scratchpad_update(tool_name: &str) -> bool {
    matches!(
        tool_name,
        TOOL_OPEN
            | TOOL_RESUME
            | TOOL_APPEND_NOTE
            | TOOL_SET_NEXT_STEPS
            | TOOL_SET_PENDING_WAITS
            | TOOL_SET_ACTION_POLICY
            | TOOL_MARK_WAIT_CHECKED
            | TOOL_UPDATE
            | TOOL_ARCHIVE
            | TOOL_UNARCHIVE
            | TOOL_RECORD_DELEGATION
    )
}

pub(crate) fn scratchpad_update_event_from_result(result: &Value) -> Option<ScratchpadUpdateEvent> {
    let scratchpad = result.get("scratchpad")?;
    Some(ScratchpadUpdateEvent {
        scratchpad_id: scratchpad.get("scratchpad_id")?.as_str()?.to_string(),
        objective: scratchpad
            .get("objective")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        status: scratchpad
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        completed: string_array_value(scratchpad.get("completed")),
        next_steps: string_array_value(scratchpad.get("next_steps")),
        pending_waits: scratchpad
            .get("pending_waits")
            .and_then(Value::as_array)
            .map(|waits| waits.iter().map(format_pending_wait).collect())
            .unwrap_or_default(),
        updated_at: scratchpad
            .get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        archived_at: scratchpad
            .get("archived_at")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    })
}

fn string_array_value(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn format_pending_wait(wait: &Value) -> String {
    if let Some(text) = wait.as_str() {
        return text.to_string();
    }
    let Some(object) = wait.as_object() else {
        return wait.to_string();
    };
    ["summary", "target", "wait_id", "reason", "next_check_at"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .unwrap_or("pending wait")
        .to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Scratchpad {
    scratchpad_id: String,
    objective: String,
    status: String,
    session_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin_thread_id: Option<String>,
    #[serde(default)]
    worktrees: Vec<Value>,
    #[serde(default)]
    active_channels: Vec<String>,
    #[serde(default)]
    active_sessions: BTreeMap<String, Value>,
    #[serde(default)]
    action_policy: BTreeMap<String, Value>,
    #[serde(default)]
    run_policy: BTreeMap<String, Value>,
    #[serde(default)]
    communication_policy: BTreeMap<String, Value>,
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
    outcomes: Vec<Value>,
    #[serde(default)]
    delegations: Vec<Value>,
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
        atomic_write_json(&path, &format!("{text}\n"))?;
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
        atomic_write_json(&self.root.join("index.json"), &format!("{text}\n"))
    }
}

fn atomic_write_json(path: &Path, text: &str) -> Result<(), FunctionCallError> {
    let parent = path.parent().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "scratchpad path `{}` has no parent directory",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let tmp_path = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("scratchpad"),
        uuid::Uuid::new_v4().simple()
    ));
    fs::write(&tmp_path, text).map_err(io_error)?;
    if cfg!(windows) && path.exists() {
        fs::remove_file(path).map_err(|err| {
            let _ = fs::remove_file(&tmp_path);
            io_error(err)
        })?;
    }
    fs::rename(&tmp_path, path).map_err(|err| {
        let _ = fs::remove_file(&tmp_path);
        io_error(err)
    })
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

    let _guard = scratchpad_write_guard()?;
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

pub(crate) fn set_thread_continuous_policy(
    codex_home: &Path,
    scratchpad_id: &str,
    enabled: bool,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let store = ScratchpadStore::new(/*state_home*/ None, codex_home)?;
    let mut scratchpad = match store.read(scratchpad_id) {
        Ok(scratchpad) => scratchpad,
        Err(FunctionCallError::RespondToModel(message)) if message.contains("not found") => {
            let now = now();
            Scratchpad {
                scratchpad_id: scratchpad_id.to_string(),
                objective: "Session continuous run policy".to_string(),
                status: "active".to_string(),
                session_key: None,
                origin_thread_id: Some(scratchpad_id.to_string()),
                worktrees: Vec::new(),
                active_channels: Vec::new(),
                active_sessions: BTreeMap::new(),
                action_policy: BTreeMap::new(),
                run_policy: BTreeMap::new(),
                communication_policy: BTreeMap::new(),
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
                outcomes: Vec::new(),
                delegations: Vec::new(),
                notes: Vec::new(),
                created_at: now.clone(),
                updated_at: now,
                archived_at: None,
            }
        }
        Err(err) => return Err(err),
    };
    if scratchpad.origin_thread_id.is_none() {
        scratchpad.origin_thread_id = Some(scratchpad_id.to_string());
    }
    set_continuous_policy_fields(&mut scratchpad, enabled);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn open_scratchpad(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let objective = string_arg(args, "objective")?;
    let session_key = optional_string_arg(args, "session_key");
    let scratchpad_id = optional_string_arg(args, "scratchpad_id")
        .unwrap_or_else(|| default_scratchpad_id.to_string());
    if let Some(existing) = read_existing_scratchpad(store, &scratchpad_id)? {
        ensure_open_compatible(&existing, &objective, session_key.as_deref())?;
        ensure_thread_owner(&existing, default_scratchpad_id)?;
        return Ok(serde_json::json!({ "scratchpad": existing }));
    }

    let now = now();
    let mut scratchpad = Scratchpad {
        scratchpad_id,
        objective,
        status: optional_string_arg(args, "status").unwrap_or_else(|| "active".to_string()),
        session_key,
        origin_thread_id: Some(default_scratchpad_id.to_string()),
        worktrees: array_arg(args, "worktrees"),
        active_channels: string_array_arg(args, "active_channels"),
        active_sessions: object_arg(args, "active_sessions"),
        action_policy: object_arg(args, "action_policy"),
        run_policy: object_arg(args, "run_policy"),
        communication_policy: object_arg(args, "communication_policy"),
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
        outcomes: array_arg(args, "outcomes"),
        delegations: array_arg(args, "delegations"),
        notes: Vec::new(),
        created_at: now.clone(),
        updated_at: now,
        archived_at: None,
    };
    merge_update(&mut scratchpad, args);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn read_existing_scratchpad(
    store: &ScratchpadStore,
    scratchpad_id: &str,
) -> Result<Option<Scratchpad>, FunctionCallError> {
    if !store.path(scratchpad_id)?.exists() {
        return Ok(None);
    }
    store.read(scratchpad_id).map(Some)
}

fn ensure_open_compatible(
    scratchpad: &Scratchpad,
    objective: &str,
    session_key: Option<&str>,
) -> Result<(), FunctionCallError> {
    if scratchpad.archived_at.is_some() {
        return Err(FunctionCallError::RespondToModel(format!(
            "scratchpad `{}` is archived; use resume_scratchpad with include_archived=true or unarchive_scratchpad instead of reopening it",
            scratchpad.scratchpad_id
        )));
    }
    if scratchpad.objective != objective {
        return Err(FunctionCallError::RespondToModel(format!(
            "scratchpad `{}` is already bound to objective `{}` and cannot be rebound to `{objective}`",
            scratchpad.scratchpad_id, scratchpad.objective
        )));
    }
    if scratchpad.session_key.as_deref() != session_key {
        return Err(FunctionCallError::RespondToModel(format!(
            "scratchpad `{}` is already bound to a different session_key",
            scratchpad.scratchpad_id
        )));
    }
    Ok(())
}

fn ensure_thread_owner(
    scratchpad: &Scratchpad,
    default_scratchpad_id: &str,
) -> Result<(), FunctionCallError> {
    match scratchpad.origin_thread_id.as_deref() {
        Some(origin_thread_id) if origin_thread_id != default_scratchpad_id => {
            return Err(FunctionCallError::RespondToModel(format!(
                "scratchpad `{}` is owned by thread `{origin_thread_id}` and cannot be accessed from thread `{default_scratchpad_id}`",
                scratchpad.scratchpad_id
            )));
        }
        Some(_) => {}
        None if scratchpad.scratchpad_id != default_scratchpad_id => {
            return Err(FunctionCallError::RespondToModel(format!(
                "scratchpad `{}` has no thread owner metadata and cannot be accessed from thread `{default_scratchpad_id}`",
                scratchpad.scratchpad_id
            )));
        }
        None => {}
    }
    Ok(())
}

fn scratchpad_visible_to_thread(scratchpad: &Scratchpad, default_scratchpad_id: &str) -> bool {
    match scratchpad.origin_thread_id.as_deref() {
        Some(origin_thread_id) => origin_thread_id == default_scratchpad_id,
        None => scratchpad.scratchpad_id == default_scratchpad_id,
    }
}

fn prepare_scratchpad_for_write(
    scratchpad: &mut Scratchpad,
    default_scratchpad_id: &str,
) -> Result<(), FunctionCallError> {
    ensure_thread_owner(scratchpad, default_scratchpad_id)?;
    if scratchpad.origin_thread_id.is_none() {
        scratchpad.origin_thread_id = Some(default_scratchpad_id.to_string());
    }
    Ok(())
}

fn get_scratchpad(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let scratchpad = store.read(&scratchpad_id)?;
    ensure_thread_owner(&scratchpad, default_scratchpad_id)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn resume_scratchpad(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let scratchpad = store.read(&scratchpad_id)?;
    ensure_thread_owner(&scratchpad, default_scratchpad_id)?;
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
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let scratchpad = store.read(&scratchpad_id)?;
    ensure_thread_owner(&scratchpad, default_scratchpad_id)?;
    Ok(serde_json::json!({ "summary": summary(&scratchpad) }))
}

fn append_scratchpad_note(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    prepare_scratchpad_for_write(&mut scratchpad, default_scratchpad_id)?;
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

fn set_next_steps(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    prepare_scratchpad_for_write(&mut scratchpad, default_scratchpad_id)?;
    scratchpad.next_steps = string_array_arg(args, "next_steps");
    if let Some(status) = optional_string_arg(args, "status") {
        scratchpad.status = status;
    }
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn set_pending_waits(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    prepare_scratchpad_for_write(&mut scratchpad, default_scratchpad_id)?;
    scratchpad.pending_waits = array_arg(args, "pending_waits");
    if let Some(status) = optional_string_arg(args, "status") {
        scratchpad.status = status;
    }
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn set_action_policy(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    prepare_scratchpad_for_write(&mut scratchpad, default_scratchpad_id)?;
    scratchpad.action_policy = object_arg(args, "action_policy");
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn mark_wait_checked(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    prepare_scratchpad_for_write(&mut scratchpad, default_scratchpad_id)?;
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

fn update_scratchpad(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    prepare_scratchpad_for_write(&mut scratchpad, default_scratchpad_id)?;
    merge_update(&mut scratchpad, args);
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn set_continuous_policy_fields(scratchpad: &mut Scratchpad, enabled: bool) {
    let updated_at = now();
    scratchpad.run_policy.insert(
        "continuous".to_string(),
        serde_json::json!({
            "enabled": enabled,
            "updated_at": updated_at,
        }),
    );
    let fallback = scratchpad
        .communication_policy
        .entry("fallback".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !fallback.is_object() {
        *fallback = serde_json::json!({});
    }
    if let Some(fallback) = fallback.as_object_mut() {
        fallback.insert(
            "final_response_on_channel_failure".to_string(),
            Value::Bool(false),
        );
    }
    scratchpad.updated_at = updated_at;
}

fn record_outcome(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    prepare_scratchpad_for_write(&mut scratchpad, default_scratchpad_id)?;
    let mut outcome = serde_json::Map::new();
    outcome.insert(
        "outcome_id".to_string(),
        Value::String(format!("outcome-{}", uuid::Uuid::new_v4().simple())),
    );
    outcome.insert("recorded_at".to_string(), Value::String(now()));
    for key in [
        "scope",
        "metric",
        "unit",
        "baseline",
        "current",
        "delta",
        "change",
        "summary",
        "tradeoffs",
        "provenance",
        "commit",
        "pr",
        "artifacts",
        "notes",
    ] {
        if let Some(value) = args.get(key) {
            outcome.insert(key.to_string(), value.clone());
        }
    }
    if !outcome.contains_key("scope") {
        outcome.insert("scope".to_string(), Value::String("general".to_string()));
    }
    if !outcome.contains_key("metric") {
        outcome.insert(
            "metric".to_string(),
            string_arg(args, "metric_name")?.into(),
        );
    }
    if let Some(outcome_id) = optional_string_arg(args, "outcome_id") {
        outcome.insert("outcome_id".to_string(), Value::String(outcome_id.clone()));
        if let Some(existing) = scratchpad.outcomes.iter_mut().find(|item| {
            item.get("outcome_id")
                .and_then(Value::as_str)
                .is_some_and(|value| value == outcome_id)
        }) {
            let mut merged = existing.as_object().cloned().unwrap_or_default();
            for (key, value) in outcome {
                merged.insert(key, value);
            }
            *existing = Value::Object(merged);
        } else {
            scratchpad.outcomes.push(Value::Object(outcome));
        }
    } else {
        scratchpad.outcomes.push(Value::Object(outcome));
    }
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn export_outcomes(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let scratchpad = store.read(&scratchpad_id)?;
    ensure_thread_owner(&scratchpad, default_scratchpad_id)?;
    let markdown = outcome_markdown(&scratchpad);
    Ok(serde_json::json!({
        "scratchpad_id": scratchpad.scratchpad_id,
        "objective": scratchpad.objective,
        "outcomes": scratchpad.outcomes,
        "markdown": markdown,
    }))
}

fn record_delegation(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    prepare_scratchpad_for_write(&mut scratchpad, default_scratchpad_id)?;
    let delegation_id = optional_string_arg(args, "delegation_id")
        .unwrap_or_else(|| format!("delegation-{}", uuid::Uuid::new_v4().simple()));
    let now = now();
    let mut delegation = serde_json::Map::new();
    delegation.insert(
        "delegation_id".to_string(),
        Value::String(delegation_id.clone()),
    );
    delegation.insert("updated_at".to_string(), Value::String(now.clone()));
    for key in [
        "agent_id",
        "agent_label",
        "child_scratchpad_id",
        "status",
        "summary",
        "item_refs",
        "task_ids",
        "next_steps",
        "artifacts",
        "notes",
    ] {
        if let Some(value) = args.get(key) {
            delegation.insert(key.to_string(), value.clone());
        }
    }
    if !delegation.contains_key("status") {
        delegation.insert("status".to_string(), Value::String("delegated".to_string()));
    }
    if let Some(existing) = scratchpad.delegations.iter_mut().find(|item| {
        item.get("delegation_id")
            .and_then(Value::as_str)
            .is_some_and(|value| value == delegation_id)
    }) {
        let mut merged = existing.as_object().cloned().unwrap_or_default();
        for (key, value) in delegation {
            merged.insert(key, value);
        }
        *existing = Value::Object(merged);
    } else {
        delegation.insert("created_at".to_string(), Value::String(now));
        scratchpad.delegations.push(Value::Object(delegation));
    }
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn archive_scratchpad(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    prepare_scratchpad_for_write(&mut scratchpad, default_scratchpad_id)?;
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

fn unarchive_scratchpad(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let _guard = scratchpad_write_guard()?;
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let mut scratchpad = store.read(&scratchpad_id)?;
    prepare_scratchpad_for_write(&mut scratchpad, default_scratchpad_id)?;
    scratchpad.status = optional_string_arg(args, "status").unwrap_or_else(|| "active".to_string());
    scratchpad.archived_at = None;
    touch(&mut scratchpad);
    store.write(&scratchpad)?;
    Ok(serde_json::json!({ "scratchpad": scratchpad }))
}

fn lookup_scratchpads(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let query = optional_string_arg(args, "query")
        .or_else(|| optional_string_arg(args, "objective"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let include_archived = bool_arg(args, "include_archived");
    let limit = usize_arg(args, "limit").unwrap_or(20).clamp(1, 100);
    let matches = store
        .list()?
        .into_iter()
        .filter(|scratchpad| scratchpad_visible_to_thread(scratchpad, default_scratchpad_id))
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

fn check_action_allowed(
    store: &ScratchpadStore,
    args: &Value,
    default_scratchpad_id: &str,
) -> Result<Value, FunctionCallError> {
    let scratchpad_id = string_arg(args, "scratchpad_id")?;
    let scratchpad = store.read(&scratchpad_id)?;
    ensure_thread_owner(&scratchpad, default_scratchpad_id)?;
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
            "origin_thread_id",
            "worktrees",
            "active_channels",
            "active_sessions",
            "action_policy",
            "run_policy",
            "communication_policy",
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
            "outcomes",
            "delegations",
            "notes",
            "archived_at"
        ],
        "storage": "Built-in scratchpads are JSON files under <codex_home>/scratchpad/entries unless state_home is provided.",
        "thread_id_default": "open_scratchpad defaults scratchpad_id to the current Codex thread/session id when scratchpad_id is omitted, and thread-owned scratchpads cannot be rebound or mutated from another thread.",
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
            TOOL_CHECK_ACTION,
            TOOL_RECORD_OUTCOME,
            TOOL_EXPORT_OUTCOMES,
            TOOL_RECORD_DELEGATION
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
    if args.get("run_policy").is_some() {
        scratchpad.run_policy = object_arg(args, "run_policy");
    }
    if args.get("communication_policy").is_some() {
        scratchpad.communication_policy = object_arg(args, "communication_policy");
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
    if args.get("outcomes").is_some() {
        scratchpad.outcomes = array_arg(args, "outcomes");
    }
    if args.get("delegations").is_some() {
        scratchpad.delegations = array_arg(args, "delegations");
    }
}

fn summary(scratchpad: &Scratchpad) -> Value {
    serde_json::json!({
        "scratchpad_id": scratchpad.scratchpad_id,
        "objective": scratchpad.objective,
        "status": scratchpad.status,
        "session_key": scratchpad.session_key,
        "origin_thread_id": scratchpad.origin_thread_id,
        "next_steps": scratchpad.next_steps,
        "pending_waits": scratchpad.pending_waits,
        "stop_conditions": scratchpad.stop_conditions,
        "run_policy": scratchpad.run_policy,
        "communication_policy": scratchpad.communication_policy,
        "outcomes_count": scratchpad.outcomes.len(),
        "latest_outcome": scratchpad.outcomes.last(),
        "delegations": &scratchpad.delegations,
        "resume_instructions": scratchpad.resume_instructions,
        "final_guard": scratchpad.final_guard,
        "notes_count": scratchpad.notes.len(),
        "updated_at": scratchpad.updated_at,
        "archived_at": scratchpad.archived_at,
    })
}

fn outcome_markdown(scratchpad: &Scratchpad) -> String {
    let mut lines = vec![
        format!("# Outcomes for {}", scratchpad.objective),
        String::new(),
        format!("Scratchpad: `{}`", scratchpad.scratchpad_id),
        String::new(),
    ];
    if scratchpad.outcomes.is_empty() {
        lines.push("No outcomes recorded.".to_string());
        return lines.join("\n");
    }

    for outcome in &scratchpad.outcomes {
        let object = outcome.as_object();
        let scope = object
            .and_then(|object| object.get("scope"))
            .map(format_outcome_value)
            .unwrap_or_else(|| "general".to_string());
        let metric = object
            .and_then(|object| object.get("metric"))
            .map(format_outcome_value)
            .unwrap_or_else(|| "metric".to_string());
        lines.push(format!("## {scope} - {metric}"));
        for key in [
            "baseline",
            "current",
            "delta",
            "unit",
            "summary",
            "tradeoffs",
            "commit",
            "pr",
            "recorded_at",
        ] {
            if let Some(value) = object.and_then(|object| object.get(key)) {
                lines.push(format!("- {key}: {}", format_outcome_value(value)));
            }
        }
        if let Some(provenance) = object.and_then(|object| object.get("provenance")) {
            lines.push(format!(
                "- provenance: {}",
                format_outcome_value(provenance)
            ));
        }
        lines.push(String::new());
    }

    lines.join("\n")
}

fn format_outcome_value(value: &Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| value.to_string())
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

fn scratchpad_write_guard() -> Result<MutexGuard<'static, ()>, FunctionCallError> {
    SCRATCHPAD_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| {
            FunctionCallError::RespondToModel("scratchpad write lock is poisoned".to_string())
        })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn open_defaults_to_thread_id_and_reopens_same_thread_active_pad() {
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
        let second = open_scratchpad(&store, &args, "thread-123").unwrap();

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
    fn open_scratchpad_does_not_reuse_other_thread_by_objective() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(
            /*state_home*/ Some(tmp.path().to_str().unwrap()),
            tmp.path(),
        )
        .unwrap();
        let args = serde_json::json!({
            "objective": "same objective",
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
            serde_json::json!("thread-456")
        );
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn unaware_threads_with_same_objective_and_session_key_get_isolated_pads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(
            /*state_home*/ Some(tmp.path().to_str().unwrap()),
            tmp.path(),
        )
        .unwrap();
        let args = serde_json::json!({
            "objective": "shared live loopback proof objective",
            "session_key": "shared live loopback proof session"
        });

        let alpha = open_scratchpad(&store, &args, "thread-alpha").unwrap();
        append_scratchpad_note(
            &store,
            &serde_json::json!({
                "scratchpad_id": "thread-alpha",
                "summary": "CANARY_ALPHA_019DE918_UNAWARE_AGENT"
            }),
            "thread-alpha",
        )
        .unwrap();
        let bravo = open_scratchpad(&store, &args, "thread-bravo").unwrap();
        append_scratchpad_note(
            &store,
            &serde_json::json!({
                "scratchpad_id": "thread-bravo",
                "summary": "CANARY_BRAVO_019DE918_UNAWARE_AGENT"
            }),
            "thread-bravo",
        )
        .unwrap();

        assert_eq!(
            alpha["scratchpad"]["scratchpad_id"],
            serde_json::json!("thread-alpha")
        );
        assert_eq!(
            bravo["scratchpad"]["scratchpad_id"],
            serde_json::json!("thread-bravo")
        );
        let alpha_stored = store.read("thread-alpha").unwrap();
        let bravo_stored = store.read("thread-bravo").unwrap();
        assert_eq!(
            alpha_stored.notes,
            vec![ScratchpadNote {
                note_id: alpha_stored.notes[0].note_id.clone(),
                ts: alpha_stored.notes[0].ts.clone(),
                category: None,
                summary: "CANARY_ALPHA_019DE918_UNAWARE_AGENT".to_string(),
                outcome: None,
            }]
        );
        assert_eq!(
            bravo_stored.notes,
            vec![ScratchpadNote {
                note_id: bravo_stored.notes[0].note_id.clone(),
                ts: bravo_stored.notes[0].ts.clone(),
                category: None,
                summary: "CANARY_BRAVO_019DE918_UNAWARE_AGENT".to_string(),
                outcome: None,
            }]
        );
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn open_scratchpad_rejects_rebinding_thread_default_to_different_objective() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(
            /*state_home*/ Some(tmp.path().to_str().unwrap()),
            tmp.path(),
        )
        .unwrap();

        open_scratchpad(
            &store,
            &serde_json::json!({
                "objective": "vector-search benchmark loop",
                "next_steps": ["continue vector-search tests"]
            }),
            "thread-vector",
        )
        .unwrap();

        let result = open_scratchpad(
            &store,
            &serde_json::json!({
                "objective": "c4game board game build",
                "next_steps": ["continue c4game UI work"]
            }),
            "thread-vector",
        );

        assert!(result.is_err());
        let stored = store.read("thread-vector").unwrap();
        assert_eq!(stored.objective, "vector-search benchmark loop");
        assert_eq!(
            stored.next_steps,
            vec!["continue vector-search tests".to_string()]
        );
    }

    #[test]
    fn mutating_scratchpad_tools_reject_other_thread_owner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(
            /*state_home*/ Some(tmp.path().to_str().unwrap()),
            tmp.path(),
        )
        .unwrap();
        open_scratchpad(
            &store,
            &serde_json::json!({
                "objective": "thread a work",
                "next_steps": ["only thread a should mutate"]
            }),
            "thread-a",
        )
        .unwrap();

        let result = update_scratchpad(
            &store,
            &serde_json::json!({
                "scratchpad_id": "thread-a",
                "next_steps": ["thread b poisoned this"]
            }),
            "thread-b",
        );

        assert!(result.is_err());
        let stored = store.read("thread-a").unwrap();
        assert_eq!(
            stored.next_steps,
            vec!["only thread a should mutate".to_string()]
        );
    }

    #[test]
    fn read_scratchpad_tools_reject_other_thread_owner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(
            /*state_home*/ Some(tmp.path().to_str().unwrap()),
            tmp.path(),
        )
        .unwrap();
        open_scratchpad(
            &store,
            &serde_json::json!({
                "objective": "thread a private work",
                "scratchpad_id": "shared-explicit-id",
                "next_steps": ["CANARY_READ_ALPHA_NEXTSTEP_FRESH_BUILD_019DE918"]
            }),
            "thread-a",
        )
        .unwrap();
        append_scratchpad_note(
            &store,
            &serde_json::json!({
                "scratchpad_id": "shared-explicit-id",
                "summary": "CANARY_READ_ALPHA_NOTE_FRESH_BUILD_019DE918"
            }),
            "thread-a",
        )
        .unwrap();

        assert!(
            get_scratchpad(
                &store,
                &serde_json::json!({ "scratchpad_id": "shared-explicit-id" }),
                "thread-b",
            )
            .is_err()
        );
        assert!(
            get_scratchpad_summary(
                &store,
                &serde_json::json!({ "scratchpad_id": "shared-explicit-id" }),
                "thread-b",
            )
            .is_err()
        );
        assert!(
            resume_scratchpad(
                &store,
                &serde_json::json!({ "scratchpad_id": "shared-explicit-id" }),
                "thread-b",
            )
            .is_err()
        );
        assert!(
            export_outcomes(
                &store,
                &serde_json::json!({ "scratchpad_id": "shared-explicit-id" }),
                "thread-b",
            )
            .is_err()
        );
        assert!(
            check_action_allowed(
                &store,
                &serde_json::json!({
                    "scratchpad_id": "shared-explicit-id",
                    "action": "deploy"
                }),
                "thread-b",
            )
            .is_err()
        );
    }

    #[test]
    fn lookup_scratchpads_filters_other_thread_owned_pads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(
            /*state_home*/ Some(tmp.path().to_str().unwrap()),
            tmp.path(),
        )
        .unwrap();
        open_scratchpad(
            &store,
            &serde_json::json!({
                "objective": "FRESH_LIVE_PROOF_LOOKUP_PRIVATE_ALPHA",
                "next_steps": ["CANARY_LOOKUP_ALPHA_NEXTSTEP_FRESH_BUILD_019DE918"]
            }),
            "thread-a",
        )
        .unwrap();
        open_scratchpad(
            &store,
            &serde_json::json!({
                "objective": "FRESH_LIVE_PROOF_LOOKUP_PRIVATE_BRAVO",
                "next_steps": ["CANARY_LOOKUP_BRAVO_NEXTSTEP_FRESH_BUILD_019DE918"]
            }),
            "thread-b",
        )
        .unwrap();

        let alpha_lookup = lookup_scratchpads(
            &store,
            &serde_json::json!({ "query": "FRESH_LIVE_PROOF_LOOKUP" }),
            "thread-a",
        )
        .unwrap();
        let bravo_lookup = lookup_scratchpads(
            &store,
            &serde_json::json!({ "query": "FRESH_LIVE_PROOF_LOOKUP" }),
            "thread-b",
        )
        .unwrap();

        assert_eq!(
            alpha_lookup["matches"][0]["scratchpad_id"],
            serde_json::json!("thread-a")
        );
        assert_eq!(alpha_lookup["matches"].as_array().unwrap().len(), 1);
        assert_eq!(
            bravo_lookup["matches"][0]["scratchpad_id"],
            serde_json::json!("thread-b")
        );
        assert_eq!(bravo_lookup["matches"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn legacy_unowned_custom_scratchpad_is_not_cross_thread_readable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(
            /*state_home*/ Some(tmp.path().to_str().unwrap()),
            tmp.path(),
        )
        .unwrap();
        open_scratchpad(
            &store,
            &serde_json::json!({
                "objective": "legacy private work",
                "scratchpad_id": "legacy-custom",
                "next_steps": ["CANARY_LEGACY_CUSTOM_FRESH_BUILD_019DE918"]
            }),
            "thread-a",
        )
        .unwrap();
        let mut scratchpad = store.read("legacy-custom").unwrap();
        scratchpad.origin_thread_id = None;
        scratchpad.next_steps = vec!["CANARY_LEGACY_CUSTOM_FRESH_BUILD_019DE918".to_string()];
        store.write(&scratchpad).unwrap();

        assert!(
            get_scratchpad(
                &store,
                &serde_json::json!({ "scratchpad_id": "legacy-custom" }),
                "thread-b",
            )
            .is_err()
        );
        let lookup = lookup_scratchpads(
            &store,
            &serde_json::json!({ "query": "legacy private work" }),
            "thread-b",
        )
        .unwrap();
        assert_eq!(lookup["matches"].as_array().unwrap().len(), 0);
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
            "thread-123",
        )
        .unwrap();
        let active_lookup = lookup_scratchpads(
            &store,
            &serde_json::json!({ "query": "recover" }),
            "thread-123",
        )
        .unwrap();
        let archived_lookup = lookup_scratchpads(
            &store,
            &serde_json::json!({ "query": "recover", "include_archived": true }),
            "thread-123",
        )
        .unwrap();

        assert_eq!(active_lookup["matches"].as_array().unwrap().len(), 0);
        assert_eq!(archived_lookup["matches"].as_array().unwrap().len(), 1);

        unarchive_scratchpad(
            &store,
            &serde_json::json!({ "scratchpad_id": "sp-test", "status": "active" }),
            "thread-123",
        )
        .unwrap();
        let active_lookup = lookup_scratchpads(
            &store,
            &serde_json::json!({ "query": "recover" }),
            "thread-123",
        )
        .unwrap();
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

        let resumed = resume_scratchpad(
            &store,
            &serde_json::json!({ "scratchpad_id": "sp-resume" }),
            "thread-123",
        )
        .unwrap();
        assert_eq!(
            resumed["summary"]["next_steps"],
            serde_json::json!(["continue"])
        );
        assert_eq!(resumed["resumed"], serde_json::json!(true));

        archive_scratchpad(
            &store,
            &serde_json::json!({ "scratchpad_id": "sp-resume" }),
            "thread-123",
        )
        .unwrap();
        let archived_default = resume_scratchpad(
            &store,
            &serde_json::json!({ "scratchpad_id": "sp-resume" }),
            "thread-123",
        );
        assert!(archived_default.is_err());

        let archived_explicit = resume_scratchpad(
            &store,
            &serde_json::json!({ "scratchpad_id": "sp-resume", "include_archived": true }),
            "thread-123",
        )
        .unwrap();
        assert!(archived_explicit["summary"]["archived_at"].is_string());
    }

    #[test]
    fn record_and_export_outcomes_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(Some(tmp.path().to_str().unwrap()), tmp.path()).unwrap();
        open_scratchpad(
            &store,
            &serde_json::json!({
                "objective": "Improve vector-search throughput",
                "scratchpad_id": "sp-outcomes"
            }),
            "thread-123",
        )
        .unwrap();

        let recorded = record_outcome(
            &store,
            &serde_json::json!({
                "scratchpad_id": "sp-outcomes",
                "scope": {
                    "service": "vector-search",
                    "surface": "hot query fanout"
                },
                "metric": "QPS",
                "unit": "requests/second",
                "baseline": 2,
                "current": 244,
                "delta": "+242 QPS",
                "summary": "Removed serialization bottleneck in the hot path.",
                "tradeoffs": ["Higher batch memory during benchmark windows"],
                "commit": "abc1234",
                "pr": "https://github.com/openai/codex/pull/123",
                "outcome_id": "outcome-hot-query-qps"
            }),
            "thread-123",
        )
        .unwrap();

        assert_eq!(
            recorded["scratchpad"]["outcomes"][0]["current"],
            serde_json::json!(244)
        );

        let updated = record_outcome(
            &store,
            &serde_json::json!({
                "scratchpad_id": "sp-outcomes",
                "outcome_id": "outcome-hot-query-qps",
                "metric": "QPS",
                "current": 245,
                "summary": "Retested with a warm cache."
            }),
            "thread-123",
        )
        .unwrap();
        assert_eq!(
            updated["scratchpad"]["outcomes"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            updated["scratchpad"]["outcomes"][0]["current"],
            serde_json::json!(245)
        );

        let exported = export_outcomes(
            &store,
            &serde_json::json!({ "scratchpad_id": "sp-outcomes" }),
            "thread-123",
        )
        .unwrap();
        assert_eq!(exported["outcomes"].as_array().unwrap().len(), 1);
        let markdown = exported["markdown"].as_str().unwrap();
        assert!(markdown.contains("vector-search"));
        assert!(markdown.contains("QPS"));
        assert!(markdown.contains("abc1234"));
    }

    #[test]
    fn record_delegation_upserts_subagent_lineage() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ScratchpadStore::new(Some(tmp.path().to_str().unwrap()), tmp.path()).unwrap();
        open_scratchpad(
            &store,
            &serde_json::json!({
                "objective": "Divide up continuous-mode migration",
                "scratchpad_id": "sp-delegation"
            }),
            "thread-123",
        )
        .unwrap();

        record_delegation(
            &store,
            &serde_json::json!({
                "scratchpad_id": "sp-delegation",
                "delegation_id": "delegation-reviewer-1",
                "agent_id": "agent-123",
                "agent_label": "Reviewer 1",
                "child_scratchpad_id": "child-sp-1",
                "item_refs": ["next_steps[0]"],
                "summary": "Review continuous run policy race conditions"
            }),
            "thread-123",
        )
        .unwrap();
        let updated = record_delegation(
            &store,
            &serde_json::json!({
                "scratchpad_id": "sp-delegation",
                "delegation_id": "delegation-reviewer-1",
                "agent_id": "agent-123",
                "status": "complete",
                "notes": ["No remaining race findings"]
            }),
            "thread-123",
        )
        .unwrap();

        assert_eq!(
            updated["scratchpad"]["delegations"],
            serde_json::json!([{
                "agent_id": "agent-123",
                "agent_label": "Reviewer 1",
                "child_scratchpad_id": "child-sp-1",
                "created_at": updated["scratchpad"]["delegations"][0]["created_at"],
                "delegation_id": "delegation-reviewer-1",
                "item_refs": ["next_steps[0]"],
                "notes": ["No remaining race findings"],
                "status": "complete",
                "summary": "Review continuous run policy race conditions",
                "updated_at": updated["scratchpad"]["delegations"][0]["updated_at"]
            }])
        );
    }

    #[test]
    fn set_thread_continuous_policy_creates_pad_and_forces_safe_fallback() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let created =
            set_thread_continuous_policy(tmp.path(), "thread-123", /*enabled*/ true).unwrap();

        assert_eq!(
            created["scratchpad"]["run_policy"]["continuous"]["enabled"],
            serde_json::json!(true)
        );
        assert_eq!(
            created["scratchpad"]["communication_policy"]["fallback"]["final_response_on_channel_failure"],
            serde_json::json!(false)
        );

        let store = ScratchpadStore::new(/*state_home*/ None, tmp.path()).unwrap();
        let mut scratchpad = store.read("thread-123").unwrap();
        scratchpad.communication_policy.insert(
            "fallback".to_string(),
            serde_json::json!({ "final_response_on_channel_failure": true }),
        );
        store.write(&scratchpad).unwrap();

        let disabled =
            set_thread_continuous_policy(tmp.path(), "thread-123", /*enabled*/ false).unwrap();

        assert_eq!(
            disabled["scratchpad"]["run_policy"]["continuous"]["enabled"],
            serde_json::json!(false)
        );
        assert_eq!(
            disabled["scratchpad"]["communication_policy"]["fallback"]["final_response_on_channel_failure"],
            serde_json::json!(false)
        );
        assert!(tmp.path().join("scratchpad/index.json").exists());
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
                origin_thread_id: None,
                worktrees: Vec::new(),
                active_channels: Vec::new(),
                active_sessions: BTreeMap::new(),
                action_policy: BTreeMap::new(),
                run_policy: BTreeMap::new(),
                communication_policy: BTreeMap::new(),
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
                outcomes: Vec::new(),
                delegations: Vec::new(),
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
                origin_thread_id: None,
                worktrees: Vec::new(),
                active_channels: Vec::new(),
                active_sessions: BTreeMap::new(),
                action_policy: BTreeMap::new(),
                run_policy: BTreeMap::new(),
                communication_policy: BTreeMap::new(),
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
                outcomes: Vec::new(),
                delegations: Vec::new(),
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
                origin_thread_id: None,
                worktrees: Vec::new(),
                active_channels: Vec::new(),
                active_sessions: BTreeMap::new(),
                action_policy: BTreeMap::new(),
                run_policy: BTreeMap::new(),
                communication_policy: BTreeMap::new(),
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
                outcomes: Vec::new(),
                delegations: Vec::new(),
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
            "thread-123",
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
            "thread-123",
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
            "thread-123",
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
            "thread-123",
        )
        .unwrap();
        assert_eq!(
            resolved["scratchpad"]["pending_waits"],
            serde_json::json!([])
        );
    }
}
