use super::*;
use crate::session::InputQueueActivity;
use crate::session::LeadIdleArmMode;
use crate::session::format_lead_wait_message;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_tools::ToolSpec;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout_at;

#[derive(Default)]
pub(crate) struct Handler {
    options: WaitAgentTimeoutOptions,
}

impl Handler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2(self.options)
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            payload,
            call_id,
            cancellation_token,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let min_timeout_ms = turn.config.multi_agent_v2.min_wait_timeout_ms;
        let max_timeout_ms = turn.config.multi_agent_v2.max_wait_timeout_ms;
        let default_timeout_ms = turn.config.multi_agent_v2.default_wait_timeout_ms;
        let sampled_lead_work_policy = *step_context.team_lead_work_policy.load_full();
        let requested_timeout_ms = args.timeout_ms;
        if let Some(ms) = requested_timeout_ms
            && ms > max_timeout_ms
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "timeout_ms must be at most {max_timeout_ms}"
            )));
        }
        let is_team_lead = session.is_team_lead().await;
        let lead_policy_changed_at_entry = is_team_lead
            && session
                .get_config()
                .await
                .effective_team_lead_work_policy()
                != sampled_lead_work_policy;
        let mut active_workers = if is_team_lead {
            Some(
                session
                    .services
                    .agent_control
                    .active_direct_worker_count(session.thread_id)
                    .await,
            )
        } else {
            None
        };
        let mut lead_has_active_workers = active_workers.is_some_and(|count| count > 0);
        let lead_deadline = if lead_has_active_workers {
            session
                .arm_lead_oversight(LeadIdleArmMode::ExplicitWait)
                .await
                .map(|(_, deadline)| deadline)
        } else {
            if is_team_lead {
                session.cancel_lead_oversight().await;
            }
            None
        };
        if is_team_lead && lead_deadline.is_none() && lead_has_active_workers {
            // A Worker can finish between the initial snapshot and the arm call. Refresh the
            // count before deciding whether this wait should end or require a Lead assessment.
            active_workers = Some(
                session
                    .services
                    .agent_control
                    .active_direct_worker_count(session.thread_id)
                    .await,
            );
            lead_has_active_workers = active_workers.is_some_and(|count| count > 0);
        }
        let lead_wait_requires_assessment = lead_has_active_workers && lead_deadline.is_none();
        let timeout_ms = if let Some(deadline) = lead_deadline {
            let remaining = deadline
                .instant
                .saturating_duration_since(Instant::now())
                .as_millis();
            i64::try_from(remaining).unwrap_or(i64::MAX).max(1)
        } else if lead_has_active_workers {
            0
        } else {
            match requested_timeout_ms {
                Some(ms) => ms.max(min_timeout_ms),
                None => default_timeout_ms,
            }
        };

        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let (mut activity_rx, pending_activity) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;
        // Subscribe before reading live config. A policy commit before subscription is detected
        // by this comparison; a later commit is delivered through the activity watch.
        let lead_policy_changed = lead_policy_changed_at_entry
            || (is_team_lead
                && session
                    .get_config()
                    .await
                    .effective_team_lead_work_policy()
                    != sampled_lead_work_policy);

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
            )
            .await;

        if let (Some(active_workers), Some(deadline)) = (active_workers, lead_deadline)
            && session.lead_idle_notifications_enabled().await
        {
            session
                .emit_lead_idle_event(format_lead_wait_message(active_workers, deadline.unix_secs))
                .await;
        }
        let lead_wait_cancelled = (is_team_lead && !session.is_team_lead().await)
            || if let Some(deadline) = lead_deadline {
                !session
                    .lead_oversight_deadline_is_current(deadline.instant)
                    .await
            } else {
                false
            };
        let wait_deadline = if let Some(deadline) = lead_deadline {
            deadline.instant
        } else {
            Instant::now() + Duration::from_millis(timeout_ms as u64)
        };
        let wait_outcome = async {
            let mut pending_activity = pending_activity;
            let mut policy_changed_at_entry = lead_policy_changed;
            loop {
                let outcome = if let Some(activity) = pending_activity.take() {
                    match activity {
                        InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
                        InputQueueActivity::Steer => WaitOutcome::Steered,
                        InputQueueActivity::TeamPolicyChanged => WaitOutcome::TeamPolicyChanged,
                    }
                } else if policy_changed_at_entry {
                    policy_changed_at_entry = false;
                    WaitOutcome::TeamPolicyChanged
                } else if is_team_lead && !lead_has_active_workers {
                    WaitOutcome::NoActiveWorkers
                } else if lead_wait_cancelled {
                    WaitOutcome::Steered
                } else if lead_wait_requires_assessment {
                    WaitOutcome::LeadReviewRequired
                } else {
                    wait_for_activity(&mut activity_rx, None, wait_deadline).await
                };
                if outcome != WaitOutcome::TeamPolicyChanged
                    || !is_team_lead
                    || session
                        .get_config()
                        .await
                        .effective_team_lead_work_policy()
                        != sampled_lead_work_policy
                {
                    break outcome;
                }
            }
        };
        tokio::pin!(wait_outcome);
        let handoff = session.maybe_handoff_dependency_free_wait(
            &turn.sub_id,
            pending_activity.is_some(),
            &cancellation_token,
        );
        tokio::pin!(handoff);
        let outcome = tokio::select! {
            _ = &mut handoff => wait_outcome.await,
            outcome = &mut wait_outcome => outcome,
        };
        let result = WaitAgentResult::from_outcome(outcome, requested_timeout_ms, timeout_ms);

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: HashMap::new(),
                }),
            )
            .await;

        Ok(boxed_tool_output(result))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    timeout_ms: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
}

impl WaitAgentResult {
    fn from_outcome(
        outcome: WaitOutcome,
        requested_timeout_ms: Option<i64>,
        timeout_ms: i64,
    ) -> Self {
        let message = match outcome {
            WaitOutcome::MailboxActivity => "Wait completed.",
            WaitOutcome::Steered => "Wait interrupted by new input.",
            WaitOutcome::TimedOut => "Wait timed out.",
            WaitOutcome::NoActiveWorkers => "No active Workers remain; wait ended.",
            WaitOutcome::LeadReviewRequired => {
                "Lead oversight already fired for this parking interval; complete the review before waiting again."
            }
            WaitOutcome::TeamPolicyChanged => {
                "The Lead work policy changed during this wait; reassess the current instructions and continue the task as needed."
            }
        };
        let message = match requested_timeout_ms {
            Some(requested_timeout_ms) if requested_timeout_ms < timeout_ms => format!(
                "{message}\n\nRequested timeout of {requested_timeout_ms}ms was clamped to the minimum of {timeout_ms}ms."
            ),
            Some(_) | None => message.to_string(),
        };
        Self {
            message,
            timed_out: outcome == WaitOutcome::TimedOut,
        }
    }
}

impl ToolOutput for WaitAgentResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "wait_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, /*success*/ None, "wait_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitOutcome {
    MailboxActivity,
    Steered,
    TimedOut,
    NoActiveWorkers,
    LeadReviewRequired,
    TeamPolicyChanged,
}

async fn wait_for_activity(
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    deadline: Instant,
) -> WaitOutcome {
    if let Some(activity) = pending_activity {
        return match activity {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
            InputQueueActivity::TeamPolicyChanged => WaitOutcome::TeamPolicyChanged,
        };
    }
    // Worker completion/failure paths publish actionable parent activity after applying the
    // configured completion policy. Polling level-triggered child status here can replay an old
    // terminal value on every subsequent wait, and can bypass the ManagerOnly completion batch.
    match timeout_at(deadline, activity_rx.changed()).await {
        Ok(Ok(())) => match *activity_rx.borrow_and_update() {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
            InputQueueActivity::TeamPolicyChanged => WaitOutcome::TeamPolicyChanged,
        }
        Ok(Err(_)) | Err(_) => WaitOutcome::TimedOut,
    }
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod tests;
