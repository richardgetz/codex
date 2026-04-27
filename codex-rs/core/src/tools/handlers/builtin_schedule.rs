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
use uuid::Uuid;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

const SCHEDULE_NAMESPACE: &str = "schedule";
const TOOL_CREATE: &str = "create_scheduled_trigger";
const TOOL_GET: &str = "get_scheduled_trigger";
const TOOL_LIST: &str = "list_scheduled_triggers";
const TOOL_DUE: &str = "list_due_scheduled_triggers";
const TOOL_UPDATE: &str = "update_scheduled_trigger";
const TOOL_CLOSE: &str = "close_scheduled_trigger";
const TOOL_REOPEN: &str = "reopen_scheduled_trigger";
const TOOL_DELETE: &str = "delete_scheduled_trigger";
const TOOL_MARK_FIRED: &str = "mark_scheduled_trigger_fired";
const TOOL_SCHEMA: &str = "get_schedule_schema";

pub(crate) const BUILTIN_SCHEDULE_TOOL_NAMES: &[&str] = &[
    TOOL_CREATE,
    TOOL_GET,
    TOOL_LIST,
    TOOL_DUE,
    TOOL_UPDATE,
    TOOL_CLOSE,
    TOOL_REOPEN,
    TOOL_DELETE,
    TOOL_MARK_FIRED,
    TOOL_SCHEMA,
];

pub(crate) fn schedule_namespace_spec() -> ToolSpec {
    let tools = [
        (
            TOOL_CREATE,
            "Create a durable time-based or conditional scheduled trigger.",
        ),
        (TOOL_GET, "Fetch one scheduled trigger by id."),
        (
            TOOL_LIST,
            "List open, closed, or all scheduled triggers sorted by next due time.",
        ),
        (
            TOOL_DUE,
            "List open scheduled triggers whose next_due_at is now or earlier.",
        ),
        (
            TOOL_UPDATE,
            "Update a scheduled trigger's task, schedule, condition, or linked context.",
        ),
        (
            TOOL_CLOSE,
            "Close a scheduled trigger or recurring trigger without deleting it.",
        ),
        (TOOL_REOPEN, "Reopen a closed scheduled trigger."),
        (
            TOOL_DELETE,
            "Delete a scheduled trigger from the local schedule store.",
        ),
        (
            TOOL_MARK_FIRED,
            "Record that a scheduled trigger fired and advance or close it.",
        ),
        (
            TOOL_SCHEMA,
            "Return the built-in schedule schema and usage guidance.",
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
        name: SCHEDULE_NAMESPACE.to_string(),
        description: "Built-in durable scheduled-trigger tools for reminders, recurring routines, conditional checks, and recovery-linked time obligations.".to_string(),
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

pub(crate) struct BuiltinScheduleHandler;

impl ToolHandler for BuiltinScheduleHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        !matches!(
            invocation.tool_name.name.as_str(),
            TOOL_GET | TOOL_LIST | TOOL_DUE | TOOL_SCHEMA
        )
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let arguments = match invocation.payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "schedule handler received unsupported payload".to_string(),
                ));
            }
        };
        let args: Value = parse_arguments(&arguments)?;
        let config = invocation.session.get_config().await;
        let store = ScheduleStore::new(
            args.get("state_home").and_then(Value::as_str),
            config.codex_home.as_path(),
        );

        let result = match invocation.tool_name.name.as_str() {
            TOOL_CREATE => create_trigger(&store, &args),
            TOOL_GET => get_trigger(&store, &args),
            TOOL_LIST => list_triggers(&store, &args),
            TOOL_DUE => list_due_triggers(&store, &args),
            TOOL_UPDATE => update_trigger(&store, &args),
            TOOL_CLOSE => close_trigger(&store, &args),
            TOOL_REOPEN => reopen_trigger(&store, &args),
            TOOL_DELETE => delete_trigger(&store, &args),
            TOOL_MARK_FIRED => mark_trigger_fired(&store, &args),
            TOOL_SCHEMA => Ok(schema_payload()),
            tool_name => Err(FunctionCallError::RespondToModel(format!(
                "unknown schedule tool: {tool_name}"
            ))),
        }?;

        Ok(FunctionToolOutput::from_text(
            json_text(result)?,
            Some(true),
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ScheduledTrigger {
    trigger_id: String,
    title: String,
    instructions: String,
    status: String,
    next_due_at: Option<String>,
    #[serde(default)]
    schedule: BTreeMap<String, Value>,
    #[serde(default)]
    condition: BTreeMap<String, Value>,
    #[serde(default)]
    scratchpad: BTreeMap<String, Value>,
    #[serde(default)]
    orchestrator_memory: BTreeMap<String, Value>,
    #[serde(default)]
    recurrence: BTreeMap<String, Value>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    notes: Vec<ScheduleNote>,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    last_fired_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ScheduleNote {
    note_id: String,
    ts: String,
    summary: String,
}

struct ScheduleStore {
    entries_dir: PathBuf,
}

impl ScheduleStore {
    fn new(state_home: Option<&str>, codex_home: &Path) -> Self {
        let root = state_home
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| codex_home.join("schedule"));
        Self {
            entries_dir: root.join("triggers"),
        }
    }

    fn path(&self, trigger_id: &str) -> Result<PathBuf, FunctionCallError> {
        let safe_id = sanitize_id(trigger_id)?;
        Ok(self.entries_dir.join(format!("{safe_id}.json")))
    }

    fn read(&self, trigger_id: &str) -> Result<ScheduledTrigger, FunctionCallError> {
        let path = self.path(trigger_id)?;
        let text = fs::read_to_string(&path).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "scheduled trigger `{trigger_id}` not found or unreadable: {err}"
            ))
        })?;
        serde_json::from_str(&text).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "scheduled trigger `{trigger_id}` is invalid JSON: {err}"
            ))
        })
    }

    fn write(&self, trigger: &ScheduledTrigger) -> Result<(), FunctionCallError> {
        fs::create_dir_all(&self.entries_dir).map_err(io_error)?;
        let path = self.path(&trigger.trigger_id)?;
        let text = serde_json::to_string_pretty(trigger).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to serialize scheduled trigger: {err}"
            ))
        })?;
        fs::write(path, format!("{text}\n")).map_err(io_error)
    }

    fn delete(&self, trigger_id: &str) -> Result<(), FunctionCallError> {
        let path = self.path(trigger_id)?;
        fs::remove_file(path).map_err(io_error)
    }

    fn list(&self) -> Result<Vec<ScheduledTrigger>, FunctionCallError> {
        let mut triggers = Vec::new();
        match fs::read_dir(&self.entries_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(io_error)?;
                    if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
                        continue;
                    }
                    let text = fs::read_to_string(entry.path()).map_err(io_error)?;
                    let trigger =
                        serde_json::from_str::<ScheduledTrigger>(&text).map_err(|err| {
                            FunctionCallError::RespondToModel(format!(
                                "failed to parse scheduled trigger file `{}`: {err}",
                                entry.path().display()
                            ))
                        })?;
                    triggers.push(trigger);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error(err)),
        }
        triggers.sort_by(compare_triggers);
        Ok(triggers)
    }
}

fn create_trigger(store: &ScheduleStore, args: &Value) -> Result<Value, FunctionCallError> {
    let now = now();
    let trigger = ScheduledTrigger {
        trigger_id: optional_string_arg(args, "trigger_id")
            .unwrap_or_else(|| format!("sched-{}", Uuid::new_v4())),
        title: string_arg(args, "title")?,
        instructions: string_arg(args, "instructions")?,
        status: optional_string_arg(args, "status").unwrap_or_else(|| "open".to_string()),
        next_due_at: optional_string_arg(args, "next_due_at"),
        schedule: object_arg(args, "schedule"),
        condition: object_arg(args, "condition"),
        scratchpad: object_arg(args, "scratchpad"),
        orchestrator_memory: object_arg(args, "orchestrator_memory"),
        recurrence: object_arg(args, "recurrence"),
        tags: string_array_arg(args, "tags"),
        notes: Vec::new(),
        created_at: now.clone(),
        updated_at: now,
        closed_at: None,
        last_fired_at: None,
    };
    validate_trigger(&trigger)?;
    store.write(&trigger)?;
    Ok(serde_json::json!({ "trigger": trigger }))
}

fn get_trigger(store: &ScheduleStore, args: &Value) -> Result<Value, FunctionCallError> {
    let trigger_id = string_arg(args, "trigger_id")?;
    Ok(serde_json::json!({ "trigger": store.read(&trigger_id)? }))
}

fn list_triggers(store: &ScheduleStore, args: &Value) -> Result<Value, FunctionCallError> {
    let status = optional_string_arg(args, "status").unwrap_or_else(|| "open".to_string());
    let query = optional_string_arg(args, "query").map(|query| query.to_ascii_lowercase());
    let triggers = store
        .list()?
        .into_iter()
        .filter(|trigger| status == "all" || trigger.status == status)
        .filter(|trigger| {
            query.as_ref().is_none_or(|query| {
                trigger.trigger_id.to_ascii_lowercase().contains(query)
                    || trigger.title.to_ascii_lowercase().contains(query)
                    || trigger.instructions.to_ascii_lowercase().contains(query)
                    || trigger
                        .tags
                        .iter()
                        .any(|tag| tag.to_ascii_lowercase().contains(query))
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({ "triggers": triggers }))
}

fn list_due_triggers(store: &ScheduleStore, args: &Value) -> Result<Value, FunctionCallError> {
    let now = match optional_string_arg(args, "now") {
        Some(now) => parse_time(&now)?,
        None => Utc::now(),
    };
    let triggers = store
        .list()?
        .into_iter()
        .filter(|trigger| trigger.status == "open")
        .filter(|trigger| {
            trigger
                .next_due_at
                .as_deref()
                .is_some_and(|due| parse_time(due).is_ok_and(|due| due <= now))
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({ "triggers": triggers, "checked_at": now.to_rfc3339() }))
}

fn update_trigger(store: &ScheduleStore, args: &Value) -> Result<Value, FunctionCallError> {
    let trigger_id = string_arg(args, "trigger_id")?;
    let mut trigger = store.read(&trigger_id)?;
    merge_update(&mut trigger, args);
    validate_trigger(&trigger)?;
    touch(&mut trigger);
    store.write(&trigger)?;
    Ok(serde_json::json!({ "trigger": trigger }))
}

fn close_trigger(store: &ScheduleStore, args: &Value) -> Result<Value, FunctionCallError> {
    let trigger_id = string_arg(args, "trigger_id")?;
    let mut trigger = store.read(&trigger_id)?;
    trigger.status = "closed".to_string();
    let now = now();
    trigger.closed_at = Some(now.clone());
    trigger.updated_at = now.clone();
    if let Some(summary) = optional_string_arg(args, "summary") {
        trigger.notes.push(ScheduleNote {
            note_id: format!("note-{}", Uuid::new_v4()),
            ts: now,
            summary,
        });
    }
    store.write(&trigger)?;
    Ok(serde_json::json!({ "trigger": trigger }))
}

fn reopen_trigger(store: &ScheduleStore, args: &Value) -> Result<Value, FunctionCallError> {
    let trigger_id = string_arg(args, "trigger_id")?;
    let mut trigger = store.read(&trigger_id)?;
    trigger.status = "open".to_string();
    trigger.closed_at = None;
    merge_update(&mut trigger, args);
    validate_trigger(&trigger)?;
    touch(&mut trigger);
    store.write(&trigger)?;
    Ok(serde_json::json!({ "trigger": trigger }))
}

fn delete_trigger(store: &ScheduleStore, args: &Value) -> Result<Value, FunctionCallError> {
    let trigger_id = string_arg(args, "trigger_id")?;
    store.delete(&trigger_id)?;
    Ok(serde_json::json!({ "deleted": true, "trigger_id": trigger_id }))
}

fn mark_trigger_fired(store: &ScheduleStore, args: &Value) -> Result<Value, FunctionCallError> {
    let trigger_id = string_arg(args, "trigger_id")?;
    let mut trigger = store.read(&trigger_id)?;
    let fired_at = optional_string_arg(args, "fired_at").unwrap_or_else(now);
    let fired_time = parse_time(&fired_at)?;
    trigger.last_fired_at = Some(fired_time.to_rfc3339());
    if let Some(next_due_at) = optional_string_arg(args, "next_due_at") {
        trigger.next_due_at = Some(parse_time(&next_due_at)?.to_rfc3339());
    } else if let Some(interval_seconds) = trigger
        .recurrence
        .get("interval_seconds")
        .and_then(Value::as_i64)
    {
        if interval_seconds > 0 {
            trigger.next_due_at =
                Some((fired_time + Duration::seconds(interval_seconds)).to_rfc3339());
        }
    } else if bool_arg(args, "close_if_not_recurring") {
        trigger.status = "closed".to_string();
        trigger.closed_at = Some(fired_time.to_rfc3339());
    }
    if let Some(summary) = optional_string_arg(args, "summary") {
        trigger.notes.push(ScheduleNote {
            note_id: format!("note-{}", Uuid::new_v4()),
            ts: fired_time.to_rfc3339(),
            summary,
        });
    }
    touch(&mut trigger);
    store.write(&trigger)?;
    Ok(serde_json::json!({ "trigger": trigger }))
}

fn merge_update(trigger: &mut ScheduledTrigger, args: &Value) {
    if let Some(title) = optional_string_arg(args, "title") {
        trigger.title = title;
    }
    if let Some(instructions) = optional_string_arg(args, "instructions") {
        trigger.instructions = instructions;
    }
    if let Some(status) = optional_string_arg(args, "status") {
        trigger.status = status;
    }
    if args.get("next_due_at").is_some() {
        trigger.next_due_at = optional_string_arg(args, "next_due_at");
    }
    if args.get("schedule").is_some() {
        trigger.schedule = object_arg(args, "schedule");
    }
    if args.get("condition").is_some() {
        trigger.condition = object_arg(args, "condition");
    }
    if args.get("scratchpad").is_some() {
        trigger.scratchpad = object_arg(args, "scratchpad");
    }
    if args.get("orchestrator_memory").is_some() {
        trigger.orchestrator_memory = object_arg(args, "orchestrator_memory");
    }
    if args.get("recurrence").is_some() {
        trigger.recurrence = object_arg(args, "recurrence");
    }
    if args.get("tags").is_some() {
        trigger.tags = string_array_arg(args, "tags");
    }
}

fn validate_trigger(trigger: &ScheduledTrigger) -> Result<(), FunctionCallError> {
    if !matches!(trigger.status.as_str(), "open" | "closed") {
        return Err(FunctionCallError::RespondToModel(
            "status must be `open` or `closed`".to_string(),
        ));
    }
    if let Some(next_due_at) = &trigger.next_due_at {
        parse_time(next_due_at)?;
    }
    if trigger.next_due_at.is_none() && trigger.schedule.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "scheduled trigger requires `next_due_at` or `schedule`".to_string(),
        ));
    }
    Ok(())
}

fn schema_payload() -> Value {
    serde_json::json!({
        "storage": "Built-in scheduled triggers are JSON files under <codex_home>/schedule/triggers unless state_home is provided.",
        "default_modes": "The built-in schedule namespace is enabled by default only in Orchestrator mode; enable per mode with [schedule.modes.<mode>].enabled = true.",
        "when_to_use": [
            "Use for explicit reminders, recurring routines, and conditional follow-ups the user wants Codex to remember.",
            "Use when a future time, cadence, or conditional check should be tracked alongside scratchpad or orchestrator-memory context.",
            "Do not use for ordinary short waits within the current turn; use wait tools or scratchpad pending waits for those."
        ],
        "fields": {
            "trigger_id": "Optional stable id; generated if omitted.",
            "title": "Short human-readable label.",
            "instructions": "What the agent should do when due, including how to resolve ambiguity.",
            "next_due_at": "RFC3339 timestamp for the next due check.",
            "schedule": "Structured schedule details such as timezone, day names, local time, or source text.",
            "condition": "Optional condition to evaluate at due time, including tool/MCP hints when relevant.",
            "scratchpad": "Optional built-in scratchpad linkage, for example { scratchpad_id, note }.",
            "orchestrator_memory": "Optional memory linkage, for example { bucket, summary }.",
            "recurrence": "Optional recurrence details. interval_seconds is mechanically advanced by mark_scheduled_trigger_fired.",
            "status": "open or closed."
        }
    })
}

fn compare_triggers(left: &ScheduledTrigger, right: &ScheduledTrigger) -> std::cmp::Ordering {
    match (&left.next_due_at, &right.next_due_at) {
        (Some(left_due), Some(right_due)) => left_due.cmp(right_due),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => right.updated_at.cmp(&left.updated_at),
    }
}

fn sanitize_id(value: &str) -> Result<String, FunctionCallError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "trigger_id must not be empty".to_string(),
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

fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn parse_time(value: &str) -> Result<DateTime<Utc>, FunctionCallError> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("invalid RFC3339 timestamp `{value}`: {err}"))
        })
}

fn touch(trigger: &mut ScheduledTrigger) {
    trigger.updated_at = now();
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn io_error(err: std::io::Error) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!("schedule storage error: {err}"))
}

fn json_text(value: Value) -> Result<String, FunctionCallError> {
    serde_json::to_string_pretty(&value).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize result: {err}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[test]
    fn creates_lists_due_and_advances_interval_trigger() {
        let dir = TempDir::new().expect("temp dir");
        let store = ScheduleStore::new(Some(dir.path().to_str().expect("utf8 path")), dir.path());

        let created = create_trigger(
            &store,
            &serde_json::json!({
                "trigger_id": "trash-night",
                "title": "Trash bins",
                "instructions": "Remind the user to put the trash bins out.",
                "next_due_at": "2026-04-30T23:00:00Z",
                "condition": { "description": "Only remind if bins are not out." },
                "scratchpad": { "scratchpad_id": "thread-123" },
                "orchestrator_memory": { "bucket": "ongoing_threads" },
                "recurrence": { "interval_seconds": 604800 },
                "tags": ["home"]
            }),
        )
        .expect("create trigger");
        assert_eq!(created["trigger"]["trigger_id"], "trash-night");

        let not_due = list_due_triggers(
            &store,
            &serde_json::json!({ "now": "2026-04-30T22:00:00Z" }),
        )
        .expect("not due");
        assert_eq!(not_due["triggers"], serde_json::json!([]));

        let due = list_due_triggers(
            &store,
            &serde_json::json!({ "now": "2026-04-30T23:00:00Z" }),
        )
        .expect("due");
        assert_eq!(due["triggers"][0]["trigger_id"], "trash-night");

        let fired = mark_trigger_fired(
            &store,
            &serde_json::json!({
                "trigger_id": "trash-night",
                "fired_at": "2026-04-30T23:00:00Z"
            }),
        )
        .expect("mark fired");
        assert_eq!(fired["trigger"]["next_due_at"], "2026-05-07T23:00:00+00:00");
    }

    #[test]
    fn close_and_reopen_preserve_trigger() {
        let dir = TempDir::new().expect("temp dir");
        let store = ScheduleStore::new(Some(dir.path().to_str().expect("utf8 path")), dir.path());
        create_trigger(
            &store,
            &serde_json::json!({
                "trigger_id": "weekly-review",
                "title": "Weekly review",
                "instructions": "Review open loops.",
                "next_due_at": "2026-05-01T12:00:00Z"
            }),
        )
        .expect("create trigger");

        let closed = close_trigger(
            &store,
            &serde_json::json!({
                "trigger_id": "weekly-review",
                "summary": "Paused by user."
            }),
        )
        .expect("close");
        assert_eq!(closed["trigger"]["status"], "closed");

        let reopened = reopen_trigger(
            &store,
            &serde_json::json!({
                "trigger_id": "weekly-review",
                "next_due_at": "2026-05-08T12:00:00Z"
            }),
        )
        .expect("reopen");
        assert_eq!(reopened["trigger"]["status"], "open");
        assert_eq!(reopened["trigger"]["next_due_at"], "2026-05-08T12:00:00Z");
    }
}
