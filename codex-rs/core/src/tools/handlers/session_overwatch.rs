use std::collections::BTreeMap;
use std::collections::HashSet;

use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::user_input::UserInput;
use codex_state::SortDirection;
use codex_state::SortKey;
use codex_state::ThreadControlMode;
use codex_state::ThreadControlRecord;
use codex_state::ThreadFilterOptions;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde::Serialize;
use serde_json::Value;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

const SESSION_OVERWATCH_NAMESPACE: &str = "session_overwatch";
const TOOL_LIST: &str = "list_sessions";
const TOOL_WATCH: &str = "watch_session";
const TOOL_UNWATCH: &str = "unwatch_session";
const TOOL_MESSAGE: &str = "message_session";

pub(crate) const SESSION_OVERWATCH_TOOL_NAMES: &[&str] =
    &[TOOL_LIST, TOOL_WATCH, TOOL_UNWATCH, TOOL_MESSAGE];

pub(crate) fn session_overwatch_namespace_spec() -> ToolSpec {
    let tools = [
        (
            TOOL_LIST,
            "List recent Codex sessions from the local state database, annotated with whether they are live in this process.",
        ),
        (
            TOOL_WATCH,
            "Attach this Orchestrator thread as an overwatch controller for an existing session.",
        ),
        (
            TOOL_UNWATCH,
            "Detach this Orchestrator thread from an overwatched session.",
        ),
        (
            TOOL_MESSAGE,
            "Send a message to a watched session when it is live in this Codex process.",
        ),
    ]
    .into_iter()
    .map(|(name, description)| {
        ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: name.to_string(),
            description: description.to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::new(),
                /*required*/ None,
                /*additional_properties*/ Some(AdditionalProperties::Boolean(true)),
            ),
            output_schema: None,
        })
    })
    .collect();

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: SESSION_OVERWATCH_NAMESPACE.to_string(),
        description:
            "Built-in Orchestrator tools for watching and contacting existing Codex sessions."
                .to_string(),
        tools,
    })
}

pub(crate) struct SessionOverwatchHandler;

impl ToolHandler for SessionOverwatchHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        invocation.tool_name.name.as_str() != TOOL_LIST
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        if invocation.turn.collaboration_mode.mode != ModeKind::Orchestrator {
            return Err(FunctionCallError::RespondToModel(
                "session_overwatch tools are only available in Orchestrator mode".to_string(),
            ));
        }

        let arguments = match &invocation.payload {
            ToolPayload::Function { arguments } => arguments.clone(),
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "session_overwatch handler received unsupported payload".to_string(),
                ));
            }
        };
        let args: Value = parse_arguments(&arguments)?;
        let result = match invocation.tool_name.name.as_str() {
            TOOL_LIST => to_value(handle_list(&invocation, args).await?)?,
            TOOL_WATCH => to_value(handle_watch(&invocation, args).await?)?,
            TOOL_UNWATCH => to_value(handle_unwatch(&invocation, args).await?)?,
            TOOL_MESSAGE => to_value(handle_message(&invocation, args).await?)?,
            other => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "unknown session_overwatch tool `{other}`"
                )));
            }
        };

        Ok(FunctionToolOutput::from_text(
            serde_json::to_string_pretty(&result).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to serialize session_overwatch result: {err}"
                ))
            })?,
            /*success*/ Some(true),
        ))
    }
}

fn to_value<T: Serialize>(value: T) -> Result<Value, FunctionCallError> {
    serde_json::to_value(value).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to serialize session_overwatch result: {err}"
        ))
    })
}

async fn handle_list(
    invocation: &ToolInvocation,
    args: Value,
) -> Result<SessionListResult, FunctionCallError> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(20)
        .clamp(1, 100);
    let search_term = args.get("search").and_then(Value::as_str);
    let state_db = invocation.session.state_db().ok_or_else(|| {
        FunctionCallError::RespondToModel("state database is not available".to_string())
    })?;
    let live_thread_ids = invocation
        .session
        .services
        .agent_control
        .live_thread_ids()
        .await
        .unwrap_or_default()
        .into_iter()
        .collect::<HashSet<_>>();
    let page = state_db
        .list_threads(
            limit,
            ThreadFilterOptions {
                archived_only: false,
                allowed_sources: &[],
                model_providers: None,
                cwd_filters: None,
                anchor: None,
                sort_key: SortKey::UpdatedAt,
                sort_direction: SortDirection::Desc,
                search_term,
            },
        )
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to list sessions: {err}"))
        })?;

    Ok(SessionListResult {
        sessions: page
            .items
            .into_iter()
            .map(|thread| SessionListItem {
                thread_id: thread.id.to_string(),
                title: thread.title,
                cwd: thread.cwd.display().to_string(),
                updated_at: thread.updated_at.to_rfc3339(),
                source: thread.source,
                model: thread.model,
                live: live_thread_ids.contains(&thread.id),
            })
            .collect(),
    })
}

async fn handle_watch(
    invocation: &ToolInvocation,
    args: Value,
) -> Result<WatchSessionResult, FunctionCallError> {
    let target_thread_id = parse_thread_id(&args)?;
    let state_db = invocation.session.state_db().ok_or_else(|| {
        FunctionCallError::RespondToModel("state database is not available".to_string())
    })?;
    let metadata = state_db.get_thread(target_thread_id).await.map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to load target session: {err}"))
    })?;
    let Some(metadata) = metadata else {
        return Err(FunctionCallError::RespondToModel(format!(
            "session `{target_thread_id}` was not found"
        )));
    };

    let mut control = invocation
        .session
        .active_thread_control()
        .await
        .unwrap_or_else(|| ThreadControlRecord {
            thread_id: invocation.session.conversation_id,
            mode: ThreadControlMode::Router,
            reason: "Manual session overwatch".to_string(),
            release_channel: None,
            watch_interval_seconds: Some(60),
            released_at: None,
            updated_at: Utc::now(),
            target_thread_ids: Vec::new(),
        });
    control.mode = ThreadControlMode::Router;
    control.updated_at = Utc::now();
    if !control.target_thread_ids.contains(&target_thread_id) {
        control.target_thread_ids.push(target_thread_id);
    }
    state_db
        .upsert_thread_control(&control)
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to persist overwatch control: {err}"))
        })?;
    invocation
        .session
        .set_active_thread_control(Some(control.clone()))
        .await;
    invocation
        .session
        .services
        .orchestrator_supervision
        .register_watched_session(
            invocation.session.conversation_id,
            target_thread_id,
            Some(metadata.title.clone()),
            Some(metadata.cwd.display().to_string()),
        )
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to update overwatch ledger: {err}"))
        })?;

    Ok(WatchSessionResult {
        watched: true,
        thread_id: target_thread_id.to_string(),
        title: metadata.title,
        live: invocation
            .session
            .services
            .agent_control
            .live_thread_ids()
            .await
            .unwrap_or_default()
            .contains(&target_thread_id),
    })
}

async fn handle_unwatch(
    invocation: &ToolInvocation,
    args: Value,
) -> Result<UnwatchSessionResult, FunctionCallError> {
    let target_thread_id = parse_thread_id(&args)?;
    let state_db = invocation.session.state_db().ok_or_else(|| {
        FunctionCallError::RespondToModel("state database is not available".to_string())
    })?;
    let mut control = invocation
        .session
        .active_thread_control()
        .await
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "this thread does not have active Orchestrator control".to_string(),
            )
        })?;
    let before = control.target_thread_ids.len();
    control
        .target_thread_ids
        .retain(|thread_id| *thread_id != target_thread_id);
    let removed = before != control.target_thread_ids.len();
    control.updated_at = Utc::now();
    state_db
        .upsert_thread_control(&control)
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to persist overwatch control: {err}"))
        })?;
    invocation
        .session
        .set_active_thread_control(Some(control))
        .await;
    invocation
        .session
        .services
        .orchestrator_supervision
        .remove_watched_session(invocation.session.conversation_id, target_thread_id)
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to update overwatch ledger: {err}"))
        })?;

    Ok(UnwatchSessionResult {
        unwatched: removed,
        thread_id: target_thread_id.to_string(),
    })
}

async fn handle_message(
    invocation: &ToolInvocation,
    args: Value,
) -> Result<MessageSessionResult, FunctionCallError> {
    let target_thread_id = parse_thread_id(&args)?;
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .ok_or_else(|| FunctionCallError::RespondToModel("message is required".to_string()))?
        .to_string();
    let interrupt = args
        .get("interrupt")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let live_thread_ids = invocation
        .session
        .services
        .agent_control
        .live_thread_ids()
        .await
        .unwrap_or_default();
    if !live_thread_ids.contains(&target_thread_id) {
        return Ok(MessageSessionResult {
            delivered: false,
            thread_id: target_thread_id.to_string(),
            submission_id: None,
            reason: Some(
                "target session is not live in this Codex process; overwatch can observe durable completion signals but cannot inject cross-process messages yet"
                    .to_string(),
            ),
        });
    }
    if interrupt {
        invocation
            .session
            .services
            .agent_control
            .interrupt_agent(target_thread_id)
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to interrupt target session: {err}"
                ))
            })?;
    }
    let submission_id = invocation
        .session
        .services
        .agent_control
        .send_user_input_to_thread(
            target_thread_id,
            vec![UserInput::Text {
                text: message,
                text_elements: Vec::new(),
            }],
        )
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to send message to target session: {err}"
            ))
        })?;
    invocation
        .session
        .services
        .orchestrator_supervision
        .note_watched_session_instruction(invocation.session.conversation_id, target_thread_id)
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to update overwatch ledger: {err}"))
        })?;

    Ok(MessageSessionResult {
        delivered: true,
        thread_id: target_thread_id.to_string(),
        submission_id: Some(submission_id),
        reason: None,
    })
}

fn parse_thread_id(args: &Value) -> Result<ThreadId, FunctionCallError> {
    let thread_id = args
        .get("thread_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|thread_id| !thread_id.is_empty())
        .ok_or_else(|| FunctionCallError::RespondToModel("thread_id is required".to_string()))?;
    ThreadId::from_string(thread_id).map_err(|err| {
        FunctionCallError::RespondToModel(format!("invalid thread_id `{thread_id}`: {err}"))
    })
}

#[derive(Debug, Serialize)]
struct SessionListResult {
    sessions: Vec<SessionListItem>,
}

#[derive(Debug, Serialize)]
struct SessionListItem {
    thread_id: String,
    title: String,
    cwd: String,
    updated_at: String,
    source: String,
    model: Option<String>,
    live: bool,
}

#[derive(Debug, Serialize)]
struct WatchSessionResult {
    watched: bool,
    thread_id: String,
    title: String,
    live: bool,
}

#[derive(Debug, Serialize)]
struct UnwatchSessionResult {
    unwatched: bool,
    thread_id: String,
}

#[derive(Debug, Serialize)]
struct MessageSessionResult {
    delivered: bool,
    thread_id: String,
    submission_id: Option<String>,
    reason: Option<String>,
}
