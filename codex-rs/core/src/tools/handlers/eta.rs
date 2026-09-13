use crate::function_tool::FunctionCallError;
use crate::session::new_submission_id;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadEtaOverallUpdatedEvent;
use codex_protocol::protocol::ThreadEtaRevisionUpdatedEvent;
use codex_protocol::protocol::ThreadEtaTaskUpdatedEvent;
use codex_protocol::protocol::ThreadEtaUpdatedEvent;
use codex_state::TaskEstimateAction;
use codex_state::TaskEstimateMutation;
use codex_state::TaskEstimateRange;
use codex_state::TaskEstimateStatus;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;

const TOOL_NAME: &str = "update_eta";
const MAX_OUTPUT_TASKS: usize = 64;

#[derive(Debug, Deserialize)]
struct EtaUpdateArgs {
    operations: Vec<EtaOperationArgs>,
}

#[derive(Debug, Deserialize)]
struct EtaOperationArgs {
    action: TaskEstimateAction,
    task_id: Option<String>,
    title: Option<String>,
    parent_task_id: Option<String>,
    depends_on_task_ids: Option<Vec<String>>,
    estimate_lower_seconds: Option<i64>,
    estimate_upper_seconds: Option<i64>,
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct EtaToolResult {
    sequence: i64,
    changed_tasks: Vec<EtaToolTaskSummary>,
    omitted_task_count: usize,
    overall: EtaToolOverallSummary,
}

#[derive(Debug, Serialize)]
struct EtaToolTaskSummary {
    task_id: String,
    status: TaskEstimateStatus,
}

#[derive(Debug, Serialize)]
struct EtaToolOverallSummary {
    finish_at: Option<i64>,
    remaining_lower_seconds: Option<i64>,
    remaining_upper_seconds: Option<i64>,
    unknown_reason: Option<String>,
}

/// Model-facing tool for recording explicit task estimates and lifecycle transitions.
pub struct EtaHandler;

impl ToolExecutor<ToolInvocation> for EtaHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let operation_properties = BTreeMap::from([
            (
                "action".to_string(),
                JsonSchema::string_enum(
                    vec![
                        json!("create"),
                        json!("start"),
                        json!("revise"),
                        json!("block"),
                        json!("complete"),
                        json!("cancel"),
                    ],
                    Some("Lifecycle operation to apply.".to_string()),
                ),
            ),
            (
                "task_id".to_string(),
                JsonSchema::string(Some("Stable task id; omit for create to allocate one.".to_string())),
            ),
            (
                "title".to_string(),
                JsonSchema::string(Some("Task title; required for create.".to_string())),
            ),
            (
                "parent_task_id".to_string(),
                JsonSchema::string(Some("Optional grouping parent task id.".to_string())),
            ),
            (
                "depends_on_task_ids".to_string(),
                JsonSchema::array(
                    JsonSchema::string(None),
                    Some("Executable dependencies; cycles are rejected.".to_string()),
                ),
            ),
            (
                "estimate_lower_seconds".to_string(),
                JsonSchema::number(Some("Inclusive lower duration bound in seconds.".to_string())),
            ),
            (
                "estimate_upper_seconds".to_string(),
                JsonSchema::number(Some("Inclusive upper duration bound in seconds.".to_string())),
            ),
            (
                "reason".to_string(),
                JsonSchema::string(Some("Short reason for a revision or scope change.".to_string())),
            ),
        ]);
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Record bounded task estimates and explicit lifecycle updates for the current root session. Keep estimates current when work starts, scope changes, blockers appear, and work completes. Completion is explicit; idle or elapsed ETA never completes a task. Estimates are lower/upper seconds ranges and may be omitted when unknown.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "operations".to_string(),
                    JsonSchema::array(
                        JsonSchema::object(
                            operation_properties,
                            Some(vec!["action".to_string()]),
                            Some(AdditionalProperties::Boolean(false)),
                        ),
                        Some("Atomic task updates to apply in order.".to_string()),
                    ),
                )]),
                Some(vec!["operations".to_string()]),
                Some(AdditionalProperties::Boolean(false)),
            ),
            output_schema: None,
        })
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move { self.handle_call(invocation).await })
    }
}

impl EtaHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            payload,
            ..
        } = invocation;
        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "update_eta handler received unsupported payload".to_string(),
                ));
            }
        };
        let args: EtaUpdateArgs = parse_arguments(&arguments)?;
        let Some(state_db) = session.state_db() else {
            return Err(FunctionCallError::Fatal(
                "ETA state database is unavailable".to_string(),
            ));
        };
        let actor_thread_id = session.thread_id();
        let session_parent_thread_id = session.session_source().await.parent_thread_id();
        let root_thread_id = state_db
            .root_thread_id(session_parent_thread_id.unwrap_or(actor_thread_id))
            .await
            .map_err(|err| FunctionCallError::Fatal(format!("failed to resolve ETA root: {err}")))?;
        let mutations = args
            .operations
            .iter()
            .map(mutation_from_args)
            .collect::<Result<Vec<_>, _>>()?;
        let result = state_db
            .apply_task_estimate_mutations(
                root_thread_id,
                actor_thread_id,
                &mutations,
                chrono::Utc::now(),
            )
            .await
            .map_err(|err| FunctionCallError::RespondToModel(format!("ETA update failed: {err}")))?;
        if !result.changed_tasks.is_empty() {
            session
                .send_event_raw_ephemeral(Event {
                    id: new_submission_id(),
                    msg: EventMsg::ThreadEtaUpdated(event_from_result(&result)),
                })
                .await;
        }
        let output = EtaToolResult {
            sequence: result.sequence,
            changed_tasks: result
                .changed_tasks
                .iter()
                .take(MAX_OUTPUT_TASKS)
                .map(|task| EtaToolTaskSummary {
                    task_id: task.task_id.clone(),
                    status: task.status,
                })
                .collect(),
            omitted_task_count: result.changed_tasks.len().saturating_sub(MAX_OUTPUT_TASKS),
            overall: EtaToolOverallSummary {
                finish_at: result.overall.finish_at.map(|value| value.timestamp()),
                remaining_lower_seconds: result.overall.remaining_lower_seconds,
                remaining_upper_seconds: result.overall.remaining_upper_seconds,
                unknown_reason: result.overall.unknown_reason.clone(),
            },
        };
        let output = serde_json::to_string(&output).map_err(|err| {
            FunctionCallError::Fatal(format!("failed to encode ETA update result: {err}"))
        })?;
        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            output,
            Some(true),
        )))
    }
}

impl CoreToolRuntime for EtaHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

fn mutation_from_args(
    operation: &EtaOperationArgs,
) -> Result<TaskEstimateMutation, FunctionCallError> {
    let estimate = match (
        operation.estimate_lower_seconds,
        operation.estimate_upper_seconds,
    ) {
        (None, None) => None,
        (lower_seconds, upper_seconds) => Some(TaskEstimateRange {
            lower_seconds,
            upper_seconds,
        }),
    };
    Ok(TaskEstimateMutation {
        action: operation.action,
        task_id: operation.task_id.clone(),
        title: operation.title.clone(),
        parent_task_id: operation.parent_task_id.clone(),
        depends_on_task_ids: operation.depends_on_task_ids.clone(),
        estimate,
        reason: operation.reason.clone(),
    })
}

fn event_from_result(result: &codex_state::TaskEstimateUpdateResult) -> ThreadEtaUpdatedEvent {
    ThreadEtaUpdatedEvent {
        root_thread_id: result.root_thread_id,
        generated_at: result.generated_at.timestamp(),
        sequence: result.sequence,
        changed_tasks: result
            .changed_tasks
            .iter()
            .map(|task| ThreadEtaTaskUpdatedEvent {
                // Active task estimates are exposed as remaining durations. History retains the
                // original/current values captured at the terminal harness timestamp.
                current_lower_seconds: if task.status.is_terminal() {
                    task.current_lower_seconds
                } else {
                    task.remaining_range(result.generated_at).lower_seconds
                },
                current_upper_seconds: if task.status.is_terminal() {
                    task.current_upper_seconds
                } else {
                    task.remaining_range(result.generated_at).upper_seconds
                },
                task_id: task.task_id.clone(),
                root_thread_id: task.root_thread_id,
                owner_thread_id: task.owner_thread_id,
                parent_task_id: task.parent_task_id.clone(),
                depends_on_task_ids: task.depends_on_task_ids.clone(),
                title: task.title.clone(),
                status: task.status.as_str().to_string(),
                original_lower_seconds: task.original_lower_seconds,
                original_upper_seconds: task.original_upper_seconds,
                created_at: task.created_at.timestamp(),
                started_at: task.started_at.map(|value| value.timestamp()),
                terminal_at: task.terminal_at.map(|value| value.timestamp()),
                actual_elapsed_seconds: task.actual_elapsed_seconds,
                updated_at: task.updated_at.timestamp(),
                revisions: task
                    .revisions
                    .iter()
                    .map(|revision| ThreadEtaRevisionUpdatedEvent {
                        lower_seconds: revision.lower_seconds,
                        upper_seconds: revision.upper_seconds,
                        reason: revision.reason.clone(),
                        updated_at: revision.updated_at.timestamp(),
                        actor_thread_id: revision.actor_thread_id,
                    })
                    .collect(),
            })
            .collect(),
        overall: ThreadEtaOverallUpdatedEvent {
            finish_at: result.overall.finish_at.map(|value| value.timestamp()),
            remaining_lower_seconds: result.overall.remaining_lower_seconds,
            remaining_upper_seconds: result.overall.remaining_upper_seconds,
            unknown_reason: result.overall.unknown_reason.clone(),
        },
    }
}
