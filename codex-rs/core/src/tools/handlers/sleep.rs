use crate::context::CurrentTimeUnavailable;
use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments_with_integral_float_fallback;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_extension_items::ExtensionItem;
use codex_extension_items::sleep::SleepItem;
use codex_features::Feature;
use codex_protocol::items::TurnItem;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Duration;
use std::time::Instant;

const NAMESPACE: &str = "clock";
const TOOL_NAME: &str = "sleep";
const MAX_SLEEP_DURATION_MS: u64 = 12 * 60 * 60 * 1000;

pub struct SleepHandler;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SleepArgs {
    duration_ms: u64,
}

fn create_sleep_tool() -> ToolSpec {
    let properties = BTreeMap::from([(
        "duration_ms".to_string(),
        JsonSchema::number(Some(format!(
            "How long to sleep in milliseconds. Must be between 1 and {MAX_SLEEP_DURATION_MS}."
        ))),
    )]);

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: NAMESPACE.to_string(),
        description: "Tools for reading and waiting on time.".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Pause execution for a specified duration. The sleep ends early when new input arrives for the active turn. Returns the elapsed wall-clock time."
                .to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["duration_ms".to_string()]),
                /*additional_properties*/ Some(false.into()),
            ),
            output_schema: None,
        })],
    })
}

impl ToolExecutor<ToolInvocation> for SleepHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(NAMESPACE, TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_sleep_tool()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                step_context,
                cancellation_token,
                call_id,
                payload,
                ..
            } = invocation;
            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{TOOL_NAME} handler received unsupported payload"
                )));
            };
            let args: SleepArgs = parse_arguments_with_integral_float_fallback(&arguments)?;
            if !(1..=MAX_SLEEP_DURATION_MS).contains(&args.duration_ms) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "duration_ms must be between 1 and {MAX_SLEEP_DURATION_MS}"
                )));
            }

            let started = Instant::now();
            let is_team_lead = session.is_team_lead().await;
            if !is_team_lead {
                turn.lead_passive_poll.reset();
            }
            let passive_park = turn.lead_passive_poll.take_sleep_park_decision(&call_id);
            if let Some((substantive_work_rx, substantive_work_generation)) = passive_park {
                let active_workers = session
                    .services
                    .agent_control
                    .active_direct_worker_count(session.thread_id)
                    .await;
                if active_workers == 0 {
                    turn.lead_passive_poll.reset();
                } else {
                    let passive_wait_deadline = session
                        .arm_lead_oversight(crate::session::LeadIdleArmMode::PassivePoll)
                        .await
                        .map(|(_, deadline)| deadline);
                    let item = passive_wait_deadline.map(|deadline| {
                        let duration_ms = deadline
                            .instant
                            .saturating_duration_since(tokio::time::Instant::now())
                            .as_millis();
                        let duration_ms = u64::try_from(duration_ms).unwrap_or(u64::MAX).max(1);
                        TurnItem::Extension(ExtensionItem::Sleep(SleepItem {
                            id: call_id.clone(),
                            duration_ms,
                        }))
                    });
                    if let Some(item) = &item {
                        session.emit_turn_item_started(turn.as_ref(), item).await;
                    }
                    let outcome = crate::tools::handlers::multi_agents_v2::passive_wait::wait_for_lead_passive_poll(
                        &session,
                        &turn,
                        &step_context,
                        passive_wait_deadline,
                        substantive_work_rx,
                        substantive_work_generation,
                        &cancellation_token,
                    )
                    .await;
                    if let Some(item) = item {
                        session.emit_turn_item_completed(turn.as_ref(), item).await;
                    }
                    let message = passive_wait_message(outcome);
                    let wall_time_seconds = started.elapsed().as_secs_f64();
                    return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        format!("Wall time: {wall_time_seconds:.4} seconds\n{message}"),
                        /*success*/ Some(true),
                    )));
                }
            }

            let item = TurnItem::Extension(ExtensionItem::Sleep(SleepItem {
                id: call_id.clone(),
                duration_ms: args.duration_ms,
            }));
            session.emit_turn_item_started(turn.as_ref(), &item).await;
            let turn_state = session
                .input_queue
                .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
                .await;
            let (mut activity_rx, pending_activity) = session
                .input_queue
                .subscribe_activity(turn_state.as_deref())
                .await;
            let sleep_result = if pending_activity.is_some() {
                Ok(true)
            } else {
                let sleep = session
                    .services
                    .time_provider
                    .sleep(session.thread_id, Duration::from_millis(args.duration_ms));
                tokio::pin!(sleep);
                tokio::select! {
                    result = &mut sleep => result.map(|()| false),
                    result = activity_rx.changed() => {
                        if result.is_ok() {
                            Ok(true)
                        } else {
                            sleep.await.map(|()| false)
                        }
                    }
                }
            };
            session.emit_turn_item_completed(turn.as_ref(), item).await;
            let interrupted = sleep_result.map_err(|err| {
                if turn.config.features.enabled(Feature::NonfatalClockReadErrors) {
                    tracing::error!(
                        thread_id = %session.thread_id,
                        turn_id = %turn.sub_id,
                        "failed to read current time for the sleep tool; the clock provider may be stalled"
                    );
                    FunctionCallError::RespondToModel(CurrentTimeUnavailable::MESSAGE.to_string())
                } else {
                    FunctionCallError::Fatal(format!("failed to sleep: {err:#}"))
                }
            })?;

            let message = if interrupted {
                "Sleep interrupted by new input."
            } else {
                "Sleep completed."
            };
            let wall_time_seconds = started.elapsed().as_secs_f64();
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                format!("Wall time: {wall_time_seconds:.4} seconds\n{message}"),
                /*success*/ Some(true),
            )))
        })
    }
}

fn passive_wait_message(
    outcome: crate::tools::handlers::multi_agents_v2::wait::WaitOutcome,
) -> &'static str {
    use crate::tools::handlers::multi_agents_v2::wait::WaitOutcome;
    match outcome {
        WaitOutcome::MailboxActivity => "Worker or coordination activity arrived; review it now.",
        WaitOutcome::Steered => "Wait interrupted by new input.",
        WaitOutcome::TimedOut => "Lead oversight deadline reached; review active Workers now.",
        WaitOutcome::NoActiveWorkers => "No active Workers remain; wait ended.",
        WaitOutcome::LeadReviewRequired => {
            "Lead oversight already fired; complete the review before waiting again."
        }
        WaitOutcome::TeamPolicyChanged => {
            "The Lead work policy changed during this wait; reassess the current instructions."
        }
        WaitOutcome::Paused => "Wait paused by Team activity control.",
        WaitOutcome::Cancelled => "Wait cancelled.",
        WaitOutcome::SubstantiveWork => {
            "Independent Lead work started; reassess before waiting again."
        }
    }
}

impl CoreToolRuntime for SleepHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}
