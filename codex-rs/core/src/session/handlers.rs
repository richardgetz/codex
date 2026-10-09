use super::Submission;
use crate::WithTurnExtensionData;
use crate::realtime_conversation::handle_audio as handle_realtime_conversation_audio;
use crate::realtime_conversation::handle_close as handle_realtime_conversation_close;
use crate::realtime_conversation::handle_speech as handle_realtime_conversation_speech;
use crate::realtime_conversation::handle_start as handle_realtime_conversation_start;
use crate::realtime_conversation::handle_text as handle_realtime_conversation_text;
use crate::session::fork_ops;
use crate::session::lead_idle::lead_progress_communication;
use async_channel::Receiver;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_protocol::turn_input::SuspendTurnOutcome;
use tracing::Instrument;
use tracing::debug_span;
use tracing::info_span;

use crate::session::session::Session;
use crate::session::thread_settings;
use crate::session::turn_input;

use crate::config::Config;
use crate::context::ContextualUserFragment;
use crate::context::GuardianApprovedAction;
use crate::review_prompts::resolve_review_request;
use crate::session::spawn_review_thread;
use crate::tasks::CompactTask;
use crate::tasks::UserShellCommandMode;
use crate::tasks::UserShellCommandTask;
use crate::tasks::execute_user_shell_command;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GuardianAssessmentEvent;
use codex_protocol::protocol::GuardianAssessmentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RealtimeConversationListVoicesResponseEvent;
use codex_protocol::protocol::RealtimeVoicesList;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_thread_store::PersistContext;

use crate::context_manager::is_user_turn_boundary;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::mcp::RequestId as ProtocolRequestId;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use serde_json::Value;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use tracing::debug;
use tracing::info;
use tracing::warn;

pub async fn interrupt(sess: &Arc<Session>) {
    sess.interrupt_task().await;
}

pub(crate) async fn thread_settings_applied_event(sess: &Session) -> EventMsg {
    let snapshot = sess.thread_config_snapshot().await;
    EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
        thread_id: Some(sess.thread_id()),
        thread_settings: snapshot.into_thread_settings_snapshot(),
    })
}

pub async fn clean_background_terminals(sess: &Arc<Session>) {
    sess.close_unified_exec_processes().await;
}

pub async fn realtime_conversation_list_voices(sess: &Session, sub_id: String) {
    sess.send_event_raw(Event {
        id: sub_id,
        msg: EventMsg::RealtimeConversationListVoicesResponse(
            RealtimeConversationListVoicesResponseEvent {
                voices: RealtimeVoicesList::builtin(),
            },
        ),
    })
    .await;
}

/// Queues an inter-agent message, then lets the shared pending-work scheduler
/// decide whether an idle session should start a regular turn.
pub async fn inter_agent_communication(
    sess: &Arc<Session>,
    sub_id: String,
    communication: InterAgentCommunication,
    start_options: codex_protocol::turn_input::TurnStartOptions,
) {
    inter_agent_communication_inner(sess, sub_id, communication, start_options, false, None).await;
}

async fn inter_agent_communication_inner(
    sess: &Arc<Session>,
    sub_id: String,
    mut communication: InterAgentCommunication,
    start_options: codex_protocol::turn_input::TurnStartOptions,
    team_lead_trigger: bool,
    handoff_admission: Option<crate::agent::control::HandoffAdmissionGuard>,
) {
    let mut trigger_turn = communication.trigger_turn;
    let is_team_lead = sess.is_team_lead().await;
    if trigger_turn && team_lead_trigger && !is_team_lead {
        return;
    }
    if is_team_lead && !trigger_turn && !communication.author.is_root() {
        let _team_lead_turn_admission = sess.team_lead_turn_admission.lock().await;
        if !sess.is_team_lead().await {
            return;
        }
        if team_lead_trigger {
            if sess.get_config().await.effective_team_lead_work_policy()
                == codex_config::TeamLeadWorkPolicy::ManagerOnly
            {
                let generation = sess
                    .input_queue
                    .enqueue_team_lead_completion(communication)
                    .await;
                drop(_team_lead_turn_admission);
                sess.schedule_manager_completion_batch_flush(generation)
                    .await;
                crate::agent_communication::emit_agent_communication_receive(&sub_id);
                return;
            }
            communication.trigger_turn = true;
            trigger_turn = true;
        } else {
            sess.input_queue
                .enqueue_team_lead_progress(communication)
                .await;
            crate::agent_communication::emit_agent_communication_receive(&sub_id);
            return;
        }
        drop(_team_lead_turn_admission);
    }
    let team_lead_turn_admission = if trigger_turn {
        Some(sess.team_lead_turn_admission.lock().await)
    } else {
        None
    };
    if is_team_lead && trigger_turn {
        sess.cancel_lead_oversight().await;
        if let Some(summary) = sess.input_queue.take_team_progress_summary().await {
            sess.input_queue
                .enqueue_team_lead_progress_summary(
                    lead_progress_communication(summary),
                    start_options.clone(),
                )
                .await;
        }
        if !sess.is_team_lead().await {
            return;
        }
    }
    if is_team_lead && trigger_turn {
        sess.input_queue
            .enqueue_team_lead_mailbox_communication(communication, start_options)
            .await;
    } else {
        sess.input_queue
            .enqueue_mailbox_communication(communication, start_options)
            .await;
    }
    crate::agent_communication::emit_agent_communication_receive(&sub_id);
    if is_team_lead && trigger_turn && !sess.is_team_lead().await {
        sess.input_queue.clear_team_lead_trigger_mailbox().await;
        return;
    }
    if trigger_turn || sess.has_outstanding_durable_sleep() {
        drop(team_lead_turn_admission);
        if let Some(handoff_admission) = handoff_admission {
            sess.maybe_start_turn_for_pending_work_with_admission(sub_id, handoff_admission)
                .await;
        } else {
            sess.maybe_start_turn_for_pending_work_with_sub_id(sub_id)
                .await;
        }
    }
}

pub async fn run_user_shell_command(
    sess: &Arc<Session>,
    sub_id: String,
    command: String,
    timeout_ms: Option<u64>,
) {
    if let Some((turn_context, cancellation_token)) =
        sess.active_turn_context_and_cancellation_token().await
    {
        let session = Arc::clone(sess);
        tokio::spawn(async move {
            execute_user_shell_command(
                session,
                turn_context,
                command,
                timeout_ms,
                cancellation_token,
                UserShellCommandMode::ActiveTurnAuxiliary,
            )
            .await;
        });
        return;
    }

    let turn_context = sess
        .new_turn_with_default_settings(sub_id, Default::default())
        .await;
    sess.spawn_task(
        turn_context,
        Vec::new(),
        UserShellCommandTask::new(command, timeout_ms),
    )
    .await;
}

pub async fn resolve_elicitation(
    sess: &Arc<Session>,
    server_name: String,
    request_id: ProtocolRequestId,
    decision: codex_protocol::approvals::ElicitationAction,
    content: Option<Value>,
    meta: Option<Value>,
) {
    let action = match decision {
        codex_protocol::approvals::ElicitationAction::Accept => ElicitationAction::Accept,
        codex_protocol::approvals::ElicitationAction::Decline => ElicitationAction::Decline,
        codex_protocol::approvals::ElicitationAction::Cancel => ElicitationAction::Cancel,
    };
    let content = match action {
        // Preserve the legacy fallback for clients that only send an action.
        ElicitationAction::Accept => Some(content.unwrap_or_else(|| serde_json::json!({}))),
        ElicitationAction::Decline | ElicitationAction::Cancel => None,
        _ => None,
    };
    let response = ElicitationResponse {
        action,
        content,
        meta,
    };
    let request_id = match request_id {
        ProtocolRequestId::String(value) => {
            rmcp::model::NumberOrString::String(std::sync::Arc::from(value))
        }
        ProtocolRequestId::Integer(value) => rmcp::model::NumberOrString::Number(value),
    };
    if let Err(err) = sess
        .resolve_elicitation(server_name, request_id, response)
        .await
    {
        warn!(
            error = %err,
            "failed to resolve elicitation request in session"
        );
    }
}

/// Propagate a user's exec approval decision to the session.
/// Also optionally applies an execpolicy amendment.
pub async fn exec_approval(
    sess: &Arc<Session>,
    approval_id: String,
    turn_id: Option<String>,
    decision: ReviewDecision,
) {
    let event_turn_id = turn_id.unwrap_or_else(|| approval_id.clone());
    if let ReviewDecision::ApprovedExecpolicyAmendment {
        proposed_execpolicy_amendment,
    } = &decision
        && let Err(err) = sess
            .persist_execpolicy_amendment(proposed_execpolicy_amendment)
            .await
    {
        let message = format!("Failed to apply execpolicy amendment: {err}");
        tracing::warn!("{message}");
        let warning = EventMsg::Warning(WarningEvent { message });
        sess.send_event_raw(Event {
            id: event_turn_id.clone(),
            msg: warning,
        })
        .await;
    }
    match decision {
        ReviewDecision::Abort => {
            sess.interrupt_task().await;
        }
        other => sess.notify_approval(&approval_id, other).await,
    }
}

pub async fn patch_approval(sess: &Arc<Session>, id: String, decision: ReviewDecision) {
    match decision {
        ReviewDecision::Abort => {
            sess.interrupt_task().await;
        }
        other => sess.notify_approval(&id, other).await,
    }
}

pub async fn request_user_input_response(
    sess: &Arc<Session>,
    id: String,
    response: RequestUserInputResponse,
) {
    sess.notify_user_input_response(&id, response).await;
}

pub async fn request_permissions_response(
    sess: &Arc<Session>,
    id: String,
    response: RequestPermissionsResponse,
) {
    sess.notify_request_permissions_response(&id, response)
        .await;
}

pub async fn dynamic_tool_response(sess: &Arc<Session>, id: String, response: DynamicToolResponse) {
    sess.notify_dynamic_tool_response(&id, response).await;
}

pub fn refresh_mcp_servers(sess: &Session) {
    sess.services.mcp_runtime.reconnect_on_next_refresh();
    sess.request_mcp_runtime_refresh();
}

pub async fn reload_user_config(sess: &Arc<Session>) {
    Box::pin(sess.reload_user_config_layer()).await;
}

pub async fn compact(sess: &Arc<Session>, sub_id: String) {
    // Stop the old turn before the compact task picks up the next turn's environments.
    sess.abort_all_tasks(TurnAbortReason::Replaced).await;
    let turn_context = sess
        .new_turn_with_default_settings(sub_id, Default::default())
        .await;

    sess.spawn_task(turn_context, Vec::new(), CompactTask).await;
}

pub(super) async fn persist_thread_memory_mode_update(
    sess: &Session,
    mode: ThreadMemoryMode,
) -> anyhow::Result<()> {
    let live_thread = sess.live_thread_for_persistence("update thread memory mode")?;
    live_thread.persist(PersistContext::Standard).await?;
    live_thread.flush().await?;
    live_thread
        .update_memory_mode(mode, /*include_archived*/ false)
        .await?;
    live_thread.flush().await?;
    Ok(())
}

/// Persists thread-level memory mode metadata for the active session.
///
/// This does not involve the model and only affects whether the thread is
/// eligible for future memory generation.
pub async fn set_thread_memory_mode(sess: &Arc<Session>, sub_id: String, mode: ThreadMemoryMode) {
    if let Err(err) = persist_thread_memory_mode_update(sess, mode).await {
        warn!("Failed to persist thread memory mode update to rollout: {err}");
        let event = Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                misalignment: None,
                message: err.to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
    }
}

pub(super) async fn shutdown_session_runtime(sess: &Arc<Session>) {
    shutdown_session_runtime_inner(sess).await;
    emit_thread_stop_lifecycle(sess).await;
}

pub(super) async fn shutdown_session_runtime_for_handoff(sess: &Arc<Session>) {
    shutdown_session_runtime_inner(sess).await;
}

async fn shutdown_session_runtime_inner(sess: &Arc<Session>) {
    let startup_prewarm = {
        let mut state = sess.state.lock().await;
        // Stop admission and take the current warmup together so resume cannot replace it.
        state.shutting_down = true;
        state.take_session_startup_prewarm()
    };
    if let Some(startup_prewarm) = startup_prewarm {
        startup_prewarm.abort().await;
    }
    let _ = sess.conversation.shutdown().await;
    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
    let shell_snapshot_prewarm = sess.state.lock().await.shell_snapshot_prewarm.take();
    if let Some(shell_snapshot_prewarm) = shell_snapshot_prewarm {
        shell_snapshot_prewarm.abort();
        let _ = shell_snapshot_prewarm.await;
    }
    sess.hooks().shutdown().await;
    sess.async_hook_results.close();
    while sess.async_hook_results.try_recv().is_ok() {}
    sess.services
        .unified_exec_manager
        .terminate_all_processes()
        .await;
    if let Err(err) = sess.services.code_mode_service.shutdown().await {
        sess.services.local_agent_runtime.record_shutdown_failure();
        warn!("failed to shutdown code mode session: {err}");
    }
    sess.stop_mcp_prewarm_worker().await;
    {
        let _refresh = sess.mcp_refresh.acquire().await;
        sess.mcp_refresh.close();
        sess.services.mcp_runtime.shutdown().await;
    }

    sess.drain_code_mode_messages().await;

    crate::hook_runtime::run_session_end_hooks(sess).await;
}

pub(super) async fn emit_thread_stop_lifecycle(sess: &Session) {
    for contributor in sess.services.extensions.thread_lifecycle_contributors() {
        contributor
            .on_thread_stop(codex_extension_api::ThreadStopInput {
                session_store: &sess.services.session_extension_data,
                thread_store: &sess.services.thread_extension_data,
            })
            .await;
    }
}

pub async fn shutdown(sess: &Arc<Session>, sub_id: String) -> bool {
    shutdown_session_runtime(sess).await;
    info!("Shutting down Codex instance");
    let history = sess.clone_history().await;
    let turn_count = history
        .raw_items()
        .filter(|item| is_user_turn_boundary(item))
        .count();
    sess.services.session_telemetry.counter(
        "codex.conversation.turn.count",
        i64::try_from(turn_count).unwrap_or(0),
        &[],
    );

    // Gracefully flush and shutdown thread persistence on session end so tests
    // that inspect durable state do not race with the background writer.
    if let Some(live_thread) = sess.live_thread()
        && let Err(e) = live_thread.shutdown().await
    {
        sess.services.local_agent_runtime.record_shutdown_failure();
        warn!("failed to shutdown thread persistence: {e}");
        let event = Event {
            id: sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
                misalignment: None,
                message: "Failed to shutdown thread persistence".to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
    }

    let event = Event {
        id: sub_id,
        msg: EventMsg::ShutdownComplete,
    };
    sess.services
        .rollout_thread_trace
        .record_protocol_event(&event.msg);
    sess.deliver_event_raw(event).await;
    sess.services
        .rollout_thread_trace
        .record_ended(codex_rollout_trace::RolloutStatus::Completed);
    true
}

/// Lets an already-admitted realtime handoff finish routing while a conversation lifecycle
/// operation drains its fanout task. Ordinary submissions stay in arrival order in a bounded
/// deferred queue and are restored to the serialized loop after the lifecycle operation. Normal
/// senders hold the shared lifecycle gate through dequeue, so the active lifecycle's exclusive
/// guard prevents new ordinary items from appearing ahead of its tail handoff.
async fn await_realtime_lifecycle<T>(
    sess: &Arc<Session>,
    rx_sub: &Receiver<Submission>,
    deferred_submissions: &mut VecDeque<Submission>,
    lifecycle: impl Future<Output = T>,
) -> T {
    tokio::pin!(lifecycle);
    let mut receiver_open = true;
    loop {
        tokio::select! {
            biased;
            result = &mut lifecycle => return result,
            submission = rx_sub.recv(), if receiver_open => {
                match submission {
                    Ok(submission) => {
                        if submission.realtime_handoff_input.is_some()
                            && matches!(&submission.op, Op::TurnInput { .. })
                        {
                            dispatch_realtime_handoff(sess, submission).await;
                        } else {
                            debug_assert!(deferred_submissions.len() < super::SUBMISSION_CHANNEL_CAPACITY);
                            deferred_submissions.push_back(submission);
                        }
                    }
                    Err(_) => receiver_open = false,
                }
            }
        }
    }
}

async fn await_shutdown_after_cancellation<T>(
    sess: &Arc<Session>,
    rx_sub: &Receiver<Submission>,
    deferred_submissions: &mut VecDeque<Submission>,
    submission_lifecycle_gate: &Arc<tokio::sync::RwLock<()>>,
    lifecycle: impl Future<Output = T>,
) -> T {
    // Cancellation does not arrive as an Op::Shutdown, so it explicitly preempts queued ordinary
    // work: those submissions are dropped before any later realtime input is routed inline. The
    // exclusive gate then prevents new ordinary submissions from entering during shutdown.
    deferred_submissions.clear();
    tokio::pin!(lifecycle);
    let write_gate = Arc::clone(submission_lifecycle_gate).write_owned();
    tokio::pin!(write_gate);
    let mut receiver_open = true;
    let _lifecycle_gate = loop {
        tokio::select! {
            biased;
            gate = &mut write_gate => break gate,
            submission = rx_sub.recv(), if receiver_open => {
                match submission {
                    Ok(submission) => {
                        if submission.realtime_handoff_input.is_some()
                            && matches!(&submission.op, Op::TurnInput { .. })
                        {
                            dispatch_realtime_handoff(sess, submission).await;
                        } else {
                            drop(submission);
                        }
                    }
                    Err(_) => receiver_open = false,
                }
            }
        }
    };
    await_realtime_lifecycle(sess, rx_sub, deferred_submissions, lifecycle).await
}

async fn dispatch_realtime_handoff(sess: &Arc<Session>, submission: Submission) {
    let dispatch_span = submission_dispatch_span(&submission);
    async move {
        let Submission {
            id,
            op,
            turn_extension_init,
            handoff_admission,
            realtime_handoff_input,
            residency_guard,
            ..
        } = submission;
        let Op::TurnInput {
            request,
            mode,
            reply,
        } = op
        else {
            unreachable!("only realtime handoff TurnInput submissions are dispatched here");
        };
        let request = WithTurnExtensionData {
            request: *request,
            turn_extension_init,
        };
        let result = turn_input::handle(
            sess,
            request,
            mode,
            id,
            handoff_admission.as_ref(),
            realtime_handoff_input,
        )
        .await;
        let _ = reply.send(result);
        drop(residency_guard);
    }
    .instrument(dispatch_span)
    .await;
}

pub async fn review(
    sess: &Arc<Session>,
    config: &Arc<Config>,
    sub_id: String,
    review_request: ReviewRequest,
) {
    let turn_context = sess
        .new_turn_with_default_settings(sub_id.clone(), Default::default())
        .await;
    sess.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
        .await;
    #[allow(deprecated)]
    match resolve_review_request(review_request, &turn_context.cwd) {
        Ok(resolved) => {
            spawn_review_thread(
                Arc::clone(sess),
                Arc::clone(config),
                turn_context.clone(),
                sub_id,
                resolved,
            )
            .await;
        }
        Err(err) => {
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    misalignment: None,
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event(&turn_context, event.msg).await;
        }
    }
}

struct ManagerCompletionDeliveryAckCleanup(Arc<Session>);

impl Drop for ManagerCompletionDeliveryAckCleanup {
    fn drop(&mut self) {
        self.0.clear_manager_completion_delivery_ack_receivers();
    }
}

pub(super) async fn submission_loop(
    sess: Arc<Session>,
    config: Arc<Config>,
    rx_sub: Receiver<Submission>,
    submission_lifecycle_gate: Arc<tokio::sync::RwLock<()>>,
) {
    // Session shutdown and tree shutdown both use the existing teardown handler.
    let _manager_completion_delivery_ack_cleanup =
        ManagerCompletionDeliveryAckCleanup(Arc::clone(&sess));
    let mut shutdown_received = false;
    let mut deferred_submissions = VecDeque::new();
    loop {
        let mut sub = if let Some(submission) = deferred_submissions.pop_front() {
            if sess.services.local_agent_runtime.shutdown.is_cancelled() {
                drop(submission);
                shutdown_received = await_shutdown_after_cancellation(
                    &sess,
                    &rx_sub,
                    &mut deferred_submissions,
                    &submission_lifecycle_gate,
                    shutdown(&sess, super::new_submission_id()),
                )
                .await;
                break;
            }
            submission
        } else {
            tokio::select! {
                biased;
                _ = sess.services.local_agent_runtime.shutdown.cancelled() => {
                    shutdown_received = await_shutdown_after_cancellation(
                        &sess,
                        &rx_sub,
                        &mut deferred_submissions,
                        &submission_lifecycle_gate,
                        shutdown(&sess, super::new_submission_id()),
                    )
                    .await;
                    break;
                }
                sub = rx_sub.recv() => match sub {
                    Ok(sub) => sub,
                    Err(_) => break,
                },
            }
        };
        let mut ordinary_submission_permit = sub.ordinary_slot_permit.take();
        let _lifecycle_gate = ordinary_submission_permit
            .as_mut()
            .and_then(|permit| permit.gate_write.take());
        drop(ordinary_submission_permit);
        let manager_completion_delivery_ack = matches!(&sub.op, Op::TeamLeadCompletion { .. });
        let handoff_admission = sub.handoff_admission.take();
        let is_realtime_submission = sub.realtime_handoff_input.is_some()
            || matches!(
                &sub.op,
                Op::RealtimeConversationStart(_)
                    | Op::RealtimeConversationClose
                    | Op::RealtimeConversationAudio(_)
                    | Op::RealtimeConversationText(_)
                    | Op::RealtimeConversationSpeech(_)
                    | Op::RealtimeConversationListVoices
            );
        let realtime_handoff_input = sub.realtime_handoff_input.take();
        if is_realtime_submission {
            // Realtime operations can carry transcript, audio, or session setup payloads.
            debug!(
                submission_id = %sub.id,
                operation = sub.op.kind(),
                "Realtime submission"
            );
        } else if matches!(sub.op, Op::ResolveElicitation { .. }) {
            debug!(submission_id = %sub.id, operation = sub.op.kind(), "Submission");
        } else {
            debug!(?sub, "Submission");
        }
        let dispatch_span = submission_dispatch_span(&sub);
        let should_exit = async {
            match sub.op {
                Op::Interrupt => {
                    interrupt(&sess).await;
                    false
                }
                Op::ContinueUsage => {
                    fork_ops::continue_usage(&sess, sub.id.clone()).await;
                    false
                }
                Op::PauseActivity => {
                    fork_ops::pause_activity(&sess, sub.id.clone()).await;
                    false
                }
                Op::PauseActivityWithAck { reply } => {
                    fork_ops::pause_activity(&sess, sub.id.clone()).await;
                    let _ = reply.send(());
                    false
                }
                Op::PauseActivityWithSnapshotAck { reply } => {
                    let snapshot = sess
                        .services
                        .local_agent_control()
                        .pause_activity_for_subtree_with_snapshot()
                        .await;
                    let _ = reply.send(snapshot);
                    false
                }
                Op::ContinueActivity => {
                    fork_ops::continue_activity(&sess, sub.id.clone()).await;
                    false
                }
                Op::ContinueActivityWithAck { reply } => {
                    fork_ops::continue_activity(&sess, sub.id.clone()).await;
                    let _ = reply.send(());
                    false
                }
                Op::InterruptIfNoPendingInput { turn_id, reply } => {
                    sess.interrupt_turn_if_no_pending_input(&turn_id, reply)
                        .await;
                    false
                }
                Op::CleanBackgroundTerminals => {
                    clean_background_terminals(&sess).await;
                    false
                }
                Op::RealtimeConversationStart(params) => {
                    let result = await_realtime_lifecycle(
                        &sess,
                        &rx_sub,
                        &mut deferred_submissions,
                        handle_realtime_conversation_start(&sess, sub.id.clone(), params),
                    )
                    .await;
                    if let Err(err) = result {
                        sess.send_event_raw(Event {
                            id: sub.id.clone(),
                            msg: EventMsg::Error(ErrorEvent {
                                misalignment: None,
                                message: err.to_string(),
                                codex_error_info: Some(CodexErrorInfo::Other),
                            }),
                        })
                        .await;
                    }
                    false
                }
                Op::RealtimeConversationAudio(params) => {
                    handle_realtime_conversation_audio(&sess, sub.id.clone(), params).await;
                    false
                }
                Op::RealtimeConversationText(params) => {
                    handle_realtime_conversation_text(&sess, sub.id.clone(), params).await;
                    false
                }
                Op::RealtimeConversationSpeech(params) => {
                    handle_realtime_conversation_speech(&sess, sub.id.clone(), params).await;
                    false
                }
                Op::RealtimeConversationClose => {
                    await_realtime_lifecycle(
                        &sess,
                        &rx_sub,
                        &mut deferred_submissions,
                        handle_realtime_conversation_close(&sess, sub.id.clone()),
                    )
                    .await;
                    false
                }
                Op::RealtimeConversationListVoices => {
                    realtime_conversation_list_voices(&sess, sub.id.clone()).await;
                    false
                }
                Op::TurnInput {
                    request,
                    mode,
                    reply,
                } => {
                    let request = WithTurnExtensionData {
                        request: *request,
                        turn_extension_init: sub.turn_extension_init,
                    };
                    let result = turn_input::handle(
                        &sess,
                        request,
                        mode,
                        sub.id.clone(),
                        handoff_admission.as_ref(),
                        realtime_handoff_input,
                    )
                    .await;
                    let _ = reply.send(result);
                    false
                }
                Op::UserInput {
                    items,
                    final_output_json_schema,
                    responsesapi_client_metadata,
                    additional_context,
                    thread_settings,
                } => {
                    let config = sess.get_config().await;
                    let session_source = sess.session_source().await;
                    let _team_worker_lease = match sess
                        .services
                        .local_agent_control()
                        .reserve_team_worker_turn(&config, &session_source, sess.thread_id())
                    {
                        Ok(lease) => lease,
                        Err(error) => {
                            sess.send_event_raw(Event {
                                id: sub.id.clone(),
                                msg: EventMsg::Error(error.to_error_event(None)),
                            })
                            .await;
                            return false;
                        }
                    };
                    let request = codex_protocol::turn_input::TurnInputRequest::new(
                        codex_protocol::turn_input::TurnInput::UserInput {
                            content: items,
                            client_id: sub.client_user_message_id,
                        },
                    )
                    .with_thread_settings(thread_settings)
                    .on_start(codex_protocol::turn_input::TurnStartOptions {
                        final_output_json_schema,
                        parent_turn_id: sub.parent_turn_id,
                        root_turn_id: sub.root_turn_id,
                        ..Default::default()
                    })
                    .with_additional_context(additional_context)
                    .with_responses_metadata(responsesapi_client_metadata)
                    .with_trace(sub.trace);
                    let result = turn_input::handle(
                        &sess,
                        WithTurnExtensionData {
                            request,
                            turn_extension_init: sub.turn_extension_init,
                        },
                        codex_protocol::turn_input::TurnInputMode::StartOrSteer,
                        sub.id.clone(),
                        handoff_admission.as_ref(),
                        None,
                    )
                    .await;
                    let error = match result {
                        Ok(codex_protocol::turn_input::TurnInputSubmission::NotSubmitted {
                            reason,
                        }) => Some(CodexErr::InvalidRequest(format!(
                            "user input was not submitted: {reason:?}"
                        ))),
                        Ok(_) => None,
                        Err(error) => Some(error),
                    };
                    if let Some(error) = error {
                        sess.send_event_raw(Event {
                            id: sub.id.clone(),
                            msg: EventMsg::Error(error.to_error_event(None)),
                        })
                        .await;
                    }
                    false
                }
                Op::RecoverTurn {
                    thread_settings,
                    start_options,
                    reply,
                } => {
                    let result = turn_input::handle_recovery(
                        &sess,
                        WithTurnExtensionData {
                            request: thread_settings,
                            turn_extension_init: sub.turn_extension_init,
                        },
                        start_options,
                        sub.id.clone(),
                    )
                    .await;
                    let _ = reply.send(result);
                    false
                }
                Op::SuspendTurnAndShutdown { reply } => {
                    let result = await_realtime_lifecycle(
                        &sess,
                        &rx_sub,
                        &mut deferred_submissions,
                        super::turn_suspension::suspend_turn_and_shutdown(&sess, sub.id.clone()),
                    )
                    .await;
                    // Exit only after history is durable and its writer has closed; an error
                    // must leave responsibility for the thread with the current worker.
                    let should_exit = matches!(
                        &result,
                        Ok(SuspendTurnOutcome::Suspended { .. })
                    );
                    let _ = reply.send(result);
                    should_exit
                }
                Op::SuspendTurnAndShutdownForHandoff { reply } => {
                    let result = await_realtime_lifecycle(
                        &sess,
                        &rx_sub,
                        &mut deferred_submissions,
                        super::turn_suspension::suspend_turn_and_shutdown_for_handoff(
                            &sess,
                            sub.id.clone(),
                        ),
                    )
                    .await;
                    let should_exit = matches!(
                        &result,
                        Ok(
                            SuspendTurnOutcome::Suspended { .. }
                                | SuspendTurnOutcome::BlockedAndShutdown { .. }
                        )
                    );
                    let _ = reply.send(result);
                    should_exit
                }
                Op::SuspendTurnAndShutdownForHandoffAfterDescendants { reply } => {
                    let result = await_realtime_lifecycle(
                        &sess,
                        &rx_sub,
                        &mut deferred_submissions,
                        super::turn_suspension::suspend_turn_and_shutdown_for_handoff_after_descendants(
                            &sess,
                            sub.id.clone(),
                        ),
                    )
                    .await;
                    let should_exit = matches!(
                        &result,
                        Ok(
                            SuspendTurnOutcome::Suspended { .. }
                                | SuspendTurnOutcome::BlockedAndShutdown { .. }
                        )
                    );
                    let _ = reply.send(result);
                    should_exit
                }
                Op::ThreadSettings {
                    thread_settings,
                    usage_policy_update,
                    reply,
                } => {
                    let thread_settings = WithTurnExtensionData {
                        request: thread_settings,
                        turn_extension_init: sub.turn_extension_init,
                    };
                    thread_settings::update(
                        &sess,
                        sub.id.clone(),
                        thread_settings,
                        usage_policy_update,
                        handoff_admission.as_ref(),
                        reply,
                    )
                    .await;
                    false
                }
                Op::TurnSettings {
                    turn_id,
                    update,
                    reply,
                } => {
                    let outcome = sess.apply_turn_settings(&turn_id, update).await;
                    let _ = reply.send(outcome);
                    false
                }
                Op::InterAgentCommunication {
                    communication,
                    start_options,
                } => {
                    inter_agent_communication_inner(
                        &sess,
                        sub.id.clone(),
                        communication,
                        start_options,
                        false,
                        handoff_admission,
                    )
                    .await;
                    false
                }
                Op::TeamLeadCompletion {
                    communication,
                    start_options,
                } => {
                    inter_agent_communication_inner(
                        &sess,
                        sub.id.clone(),
                        communication,
                        start_options,
                        true,
                        handoff_admission,
                    )
                    .await;
                    false
                }
                Op::ExecApproval {
                    id: approval_id,
                    turn_id,
                    decision,
                } => {
                    exec_approval(&sess, approval_id, turn_id, decision).await;
                    false
                }
                Op::PatchApproval { id, decision } => {
                    patch_approval(&sess, id, decision).await;
                    false
                }
                Op::UserInputAnswer { id, response } => {
                    request_user_input_response(&sess, id, response).await;
                    false
                }
                Op::RequestPermissionsResponse { id, response } => {
                    request_permissions_response(&sess, id, response).await;
                    false
                }
                Op::DynamicToolResponse { id, response } => {
                    dynamic_tool_response(&sess, id, response).await;
                    false
                }
                Op::RefreshMcpServers => {
                    refresh_mcp_servers(&sess);
                    false
                }
                Op::ReloadUserConfig => {
                    reload_user_config(&sess).await;
                    false
                }
                Op::Compact => {
                    compact(&sess, sub.id.clone()).await;
                    false
                }
                Op::ConsolidateOrchestratorMemory => {
                    fork_ops::consolidate_orchestrator_memory(&sess, &config, sub.id.clone());
                    false
                }
                Op::OrchestratorMemoryForget { needle } => {
                    fork_ops::forget_orchestrator_memory(
                        &sess,
                        &config,
                        sub.id.clone(),
                        needle,
                    );
                    false
                }
                Op::UserPreferencesMemoryMigrate => {
                    fork_ops::migrate_user_preferences_memory(&sess, &config, sub.id.clone());
                    false
                }
                Op::PruneIdleAgents => {
                    fork_ops::prune_idle_agents(&sess, sub.id.clone()).await;
                    false
                }
                Op::SetThreadName { name } => {
                    fork_ops::set_thread_name(&sess, sub.id.clone(), name).await;
                    false
                }
                Op::SetScratchpadContinuousPolicy { enabled } => {
                    fork_ops::set_scratchpad_continuous_policy(&sess, sub.id.clone(), enabled)
                        .await;
                    false
                }
                Op::SetThreadMemoryMode { mode } => {
                    set_thread_memory_mode(&sess, sub.id.clone(), mode).await;
                    false
                }
                Op::SetMemoryAccessPolicy { policy } => {
                    fork_ops::set_memory_access_policy(&sess, sub.id.clone(), policy).await;
                    false
                }
                Op::SetUserPreferencesMemoryPolicy { policy } => {
                    fork_ops::set_user_preferences_memory_policy(
                        &sess,
                        sub.id.clone(),
                        policy,
                    )
                    .await;
                    false
                }
                Op::ThreadRollback { num_turns } => {
                    fork_ops::thread_rollback(&sess, sub.id.clone(), num_turns).await;
                    false
                }
                Op::RunUserShellCommand {
                    command,
                    timeout_ms,
                } => {
                    run_user_shell_command(&sess, sub.id.clone(), command, timeout_ms).await;
                    false
                }
                Op::ResolveElicitation {
                    server_name,
                    request_id,
                    decision,
                    content,
                    meta,
                } => {
                    resolve_elicitation(&sess, server_name, request_id, decision, content, meta)
                        .await;
                    false
                }
                Op::Shutdown => {
                    await_realtime_lifecycle(
                        &sess,
                        &rx_sub,
                        &mut deferred_submissions,
                        shutdown(&sess, sub.id.clone()),
                    )
                    .await
                }
                Op::Review { review_request } => {
                    review(&sess, &config, sub.id.clone(), review_request).await;
                    false
                }
                Op::ApproveGuardianDeniedAction { event } => {
                    approve_guardian_denied_action(&sess, event).await;
                    false
                }
                _ => false, // Ignore unknown ops; enum is non_exhaustive to allow extensions.
            }
        }
        .instrument(dispatch_span)
        .await;
        if manager_completion_delivery_ack {
            sess.acknowledge_manager_completion_delivery(&sub.id).await;
        }
        drop(sub.residency_guard);
        if should_exit {
            shutdown_received = true;
            break;
        }
    }
    // A completed shutdown drops queued ordinary submissions. Their reply senders, bounded-slot
    // permits, and residency guards are released together; no deferred item is processed twice.
    deferred_submissions.clear();
    while let Ok(submission) = rx_sub.try_recv() {
        drop(submission);
    }
    sess.clear_manager_completion_delivery_acks().await;
    // Receiver closure is a no-hang teardown path. All strong submission senders are gone, so the
    // realtime manager's weak sender cannot upgrade and this path cannot route a configured tail.
    if !shutdown_received {
        await_realtime_lifecycle(
            &sess,
            &rx_sub,
            &mut deferred_submissions,
            shutdown_session_runtime(&sess),
        )
        .await;
        if let Some(live_thread) = sess.live_thread()
            && let Err(err) = live_thread.shutdown().await
        {
            sess.services.local_agent_runtime.record_shutdown_failure();
            warn!("failed to shutdown thread persistence after submission channel closed: {err}");
        }
    }
    debug!("Agent loop exited");
}

async fn approve_guardian_denied_action(sess: &Arc<Session>, event: GuardianAssessmentEvent) {
    if event.status != GuardianAssessmentStatus::Denied {
        warn!(
            review_id = event.id.as_str(),
            "ignoring approval for non-denied Guardian assessment"
        );
        return;
    }

    let approved_action = serde_json::json!({
        "action": &event.action,
        "outcome": "allowed",
    });
    let approved_action_json = match serde_json::to_string_pretty(&approved_action) {
        Ok(approved_action_json) => approved_action_json,
        Err(error) => {
            warn!(%error, review_id = event.id.as_str(), "failed to serialize approved Guardian action");
            return;
        }
    };
    let items = vec![ContextualUserFragment::into(GuardianApprovedAction::new(
        approved_action_json,
    ))];

    sess.inject_no_new_turn(items, /*current_turn_context*/ None)
        .await;
}

pub(super) fn submission_dispatch_span(sub: &Submission) -> tracing::Span {
    let op_name = sub.op.kind();
    let span_name = format!("op.dispatch.{op_name}");
    let dispatch_span = match &sub.op {
        Op::RealtimeConversationAudio(_) => {
            debug_span!(
                "submission_dispatch",
                otel.name = span_name.as_str(),
                submission.id = sub.id.as_str(),
                codex.op = op_name
            )
        }
        _ => info_span!(
            "submission_dispatch",
            otel.name = span_name.as_str(),
            submission.id = sub.id.as_str(),
            codex.op = op_name
        ),
    };
    if let Some(trace) = sub.trace.as_ref()
        && !set_parent_from_w3c_trace_context(&dispatch_span, trace)
    {
        warn!(
            submission.id = sub.id.as_str(),
            "ignoring invalid submission trace carrier"
        );
    }
    dispatch_span
}
