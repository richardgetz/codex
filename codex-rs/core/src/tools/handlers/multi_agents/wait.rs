use super::*;
use crate::agent::status::is_final;
use crate::session::InputQueueActivity;
use crate::session::LeadIdleArmMode;
use crate::session::format_lead_wait_message;
use crate::session::session::Session;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v1;
use codex_protocol::error::CodexErrorDetails;
use codex_tools::ToolSpec;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch::Receiver;
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
        ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v1(self.options)
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        multi_agent_tool_search_info(
            "wait_agent wait agent subagent status final result complete timeout targets",
            self.spec(),
        )
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
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let receiver_thread_ids = parse_agent_id_targets(args.targets)?;
        let mut receiver_agents = Vec::with_capacity(receiver_thread_ids.len());
        let mut target_by_thread_id = HashMap::with_capacity(receiver_thread_ids.len());
        for receiver_thread_id in &receiver_thread_ids {
            let agent_metadata = session
                .services
                .agent_control
                .get_agent_metadata(*receiver_thread_id)
                .unwrap_or_default();
            target_by_thread_id.insert(
                *receiver_thread_id,
                agent_metadata
                    .agent_path
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| receiver_thread_id.to_string()),
            );
            receiver_agents.push(CollabAgentRef {
                thread_id: *receiver_thread_id,
                agent_nickname: agent_metadata.agent_nickname,
                agent_role: agent_metadata.agent_role,
            });
        }

        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
        let timeout_ms = match timeout_ms {
            ms if ms <= 0 => {
                return Err(FunctionCallError::RespondToModel(
                    "timeout_ms must be greater than zero".to_owned(),
                ));
            }
            ms => ms.clamp(MIN_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS),
        };
        let is_team_lead = session.is_team_lead().await;
        let mut active_direct_workers = if is_team_lead {
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
        let mut lead_deadline = None;
        if is_team_lead && active_direct_workers.is_some_and(|count| count > 0) {
            lead_deadline = session
                .arm_lead_oversight(LeadIdleArmMode::ExplicitWait)
                .await
                .map(|(_, deadline)| deadline);
            if lead_deadline.is_none() {
                // A Worker can finish between the initial count and the arm call. Refresh before
                // deciding whether this wait should park until a future action or completion.
                active_direct_workers = Some(
                    session
                        .services
                        .agent_control
                        .active_direct_worker_count(session.thread_id)
                        .await,
                );
            }
        } else if is_team_lead {
            session.cancel_lead_oversight().await;
        }

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: receiver_thread_ids.clone(),
                    receiver_agents: receiver_agents.clone(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
            )
            .await;

        if let (Some(active_workers), Some(deadline)) = (active_direct_workers, lead_deadline)
            && session.lead_idle_notifications_enabled().await
        {
            session
                .emit_lead_idle_event(format_lead_wait_message(active_workers, deadline.unix_secs))
                .await;
        }

        let mut status_rxs = Vec::with_capacity(receiver_thread_ids.len());
        let mut initial_final_statuses = Vec::new();
        for id in &receiver_thread_ids {
            match session.services.agent_control.subscribe_status(*id).await {
                Ok(rx) => {
                    let status = rx.borrow().clone();
                    if is_final(&status) {
                        initial_final_statuses.push((*id, status));
                    }
                    status_rxs.push((*id, rx));
                }
                Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                    initial_final_statuses.push((*id, AgentStatus::NotFound));
                }
                Err(err) => {
                    let mut statuses = HashMap::with_capacity(1);
                    statuses.insert(*id, session.services.agent_control.get_status(*id).await);
                    session
                        .emit_turn_item_completed(
                            &turn,
                            TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                                id: call_id.clone(),
                                tool: CollabAgentTool::Wait,
                                status: wait_tool_call_status(&statuses),
                                sender_thread_id: session.thread_id,
                                receiver_thread_ids: statuses.keys().copied().collect(),
                                receiver_agents: wait_receiver_agents(&statuses, &receiver_agents),
                                prompt: None,
                                model: None,
                                reasoning_effort: None,
                                agents_states: statuses,
                            }),
                        )
                        .await;
                    return Err(collab_agent_error(*id, err));
                }
            }
        }

        let turn_state = if is_team_lead {
            session
                .input_queue
                .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
                .await
        } else {
            None
        };
        let (mut activity_rx, pending_activity) = if is_team_lead {
            let (activity_rx, pending_activity) = session
                .input_queue
                .subscribe_activity(turn_state.as_deref())
                .await;
            (Some(activity_rx), pending_activity)
        } else {
            (None, None)
        };
        let lead_wait_cancelled = (is_team_lead && !session.is_team_lead().await)
            || if let Some(deadline) = lead_deadline {
                !session
                    .lead_oversight_deadline_is_current(deadline.instant)
                    .await
            } else {
                false
            };
        let lead_wait_requires_assessment = is_team_lead
            && active_direct_workers.is_some_and(|count| count > 0)
            && lead_deadline.is_none()
            && !lead_wait_cancelled;
        let statuses = if !initial_final_statuses.is_empty() {
            initial_final_statuses
        } else if is_team_lead && active_direct_workers == Some(0) {
            Vec::new()
        } else if lead_wait_cancelled {
            Vec::new()
        } else if lead_wait_requires_assessment {
            // The previous parked interval already fired. V1 has no dedicated
            // review-required result field, so return immediately and let the
            // Lead assess before issuing another wait call.
            Vec::new()
        } else {
            let mut futures = FuturesUnordered::new();
            for (id, rx) in status_rxs.into_iter() {
                let session = session.clone();
                futures.push(wait_for_final_status(session, id, rx));
            }
            let mut results = Vec::new();
            if is_team_lead {
                let activity_rx = activity_rx
                    .as_mut()
                    .expect("team Lead waits subscribe to activity");
                let deadline = lead_deadline.map(|deadline| deadline.instant);
                results =
                    wait_for_lead_statuses(&mut futures, activity_rx, pending_activity, deadline)
                        .await;
            } else {
                let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
                loop {
                    match timeout_at(deadline, futures.next()).await {
                        Ok(Some(Some(result))) => {
                            results.push(result);
                            break;
                        }
                        Ok(Some(None)) => continue,
                        Ok(None) | Err(_) => break,
                    }
                }
                if !results.is_empty() {
                    loop {
                        match futures.next().now_or_never() {
                            Some(Some(Some(result))) => results.push(result),
                            Some(Some(None)) => continue,
                            Some(None) | None => break,
                        }
                    }
                }
            }
            results
        };

        let timed_out = statuses.is_empty() && !(is_team_lead && active_direct_workers == Some(0));
        let statuses_by_id = statuses.clone().into_iter().collect::<HashMap<_, _>>();
        let result = WaitAgentResult {
            status: statuses
                .into_iter()
                .filter_map(|(thread_id, status)| {
                    target_by_thread_id
                        .get(&thread_id)
                        .cloned()
                        .map(|target| (target, status))
                })
                .collect(),
            timed_out,
        };

        for receiver_thread_id in &receiver_thread_ids {
            session
                .services
                .orchestrator_supervision
                .note_check(session.thread_id, *receiver_thread_id)
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to update orchestrator supervision wait timestamp: {err}"
                    ))
                })?;
        }

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: wait_tool_call_status(&statuses_by_id),
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: statuses_by_id.keys().copied().collect(),
                    receiver_agents: wait_receiver_agents(&statuses_by_id, &receiver_agents),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: statuses_by_id,
                }),
            )
            .await;

        Ok(boxed_tool_output(result))
    }
}

fn wait_tool_call_status(statuses: &HashMap<ThreadId, AgentStatus>) -> CollabAgentToolCallStatus {
    if statuses
        .values()
        .any(|status| matches!(status, AgentStatus::Errored(_) | AgentStatus::NotFound))
    {
        CollabAgentToolCallStatus::Failed
    } else {
        CollabAgentToolCallStatus::Completed
    }
}

fn wait_receiver_agents(
    statuses: &HashMap<ThreadId, AgentStatus>,
    receiver_agents: &[CollabAgentRef],
) -> Vec<CollabAgentRef> {
    if statuses.is_empty() {
        return Vec::new();
    }

    let mut agents = Vec::with_capacity(statuses.len());
    let mut seen = HashMap::with_capacity(receiver_agents.len());
    for receiver_agent in receiver_agents {
        seen.insert(receiver_agent.thread_id, ());
        if statuses.contains_key(&receiver_agent.thread_id) {
            agents.push(receiver_agent.clone());
        }
    }

    let mut extras = statuses
        .keys()
        .filter(|thread_id| !seen.contains_key(thread_id))
        .map(|thread_id| CollabAgentRef {
            thread_id: *thread_id,
            agent_nickname: None,
            agent_role: None,
        })
        .collect::<Vec<_>>();
    extras.sort_by_key(|agent| agent.thread_id.to_string());
    agents.extend(extras);
    agents
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
struct WaitArgs {
    #[serde(default)]
    targets: Vec<String>,
    timeout_ms: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) status: HashMap<String, AgentStatus>,
    pub(crate) timed_out: bool,
}

enum LeadWaitSignal<T> {
    Activity,
    Status(Option<T>),
}

/// Waits for one final Worker status, user/mailbox activity, or the shared Lead oversight
/// deadline. When the deadline has already been consumed by an earlier wait in the same turn,
/// `deadline` is `None`, so this remains parked until an actionable event rather than polling.
async fn wait_for_lead_statuses<F>(
    futures: &mut FuturesUnordered<F>,
    activity_rx: &mut Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    deadline: Option<Instant>,
) -> Vec<(ThreadId, AgentStatus)>
where
    F: Future<Output = Option<(ThreadId, AgentStatus)>> + Send,
{
    if pending_activity.is_some() {
        return Vec::new();
    }

    let mut results = Vec::new();
    loop {
        let signal = async {
            tokio::select! {
                activity = activity_rx.changed() => {
                    let _ = activity;
                    LeadWaitSignal::Activity
                }
                status = futures.next() => LeadWaitSignal::Status(status),
            }
        };
        let signal = if let Some(deadline) = deadline {
            match timeout_at(deadline, signal).await {
                Ok(signal) => signal,
                Err(_) => break,
            }
        } else {
            signal.await
        };
        match signal {
            LeadWaitSignal::Activity => break,
            LeadWaitSignal::Status(Some(Some(result))) => {
                results.push(result);
                break;
            }
            LeadWaitSignal::Status(Some(None)) => continue,
            LeadWaitSignal::Status(None) => break,
        }
    }
    if !results.is_empty() {
        loop {
            match futures.next().now_or_never() {
                Some(Some(Some(result))) => results.push(result),
                Some(Some(None)) => continue,
                Some(None) | None => break,
            }
        }
    }
    results
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

async fn wait_for_final_status(
    session: Arc<Session>,
    thread_id: ThreadId,
    mut status_rx: Receiver<AgentStatus>,
) -> Option<(ThreadId, AgentStatus)> {
    let mut status = status_rx.borrow().clone();
    if is_final(&status) {
        return Some((thread_id, status));
    }

    loop {
        if status_rx.changed().await.is_err() {
            let latest = session.services.agent_control.get_status(thread_id).await;
            return is_final(&latest).then_some((thread_id, latest));
        }
        status = status_rx.borrow().clone();
        if is_final(&status) {
            return Some((thread_id, status));
        }
    }
}
