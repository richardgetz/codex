use crate::realtime_conversation::handle_audio as handle_realtime_conversation_audio;
use crate::realtime_conversation::handle_close as handle_realtime_conversation_close;
use crate::realtime_conversation::handle_speech as handle_realtime_conversation_speech;
use crate::realtime_conversation::handle_start as handle_realtime_conversation_start;
use crate::realtime_conversation::handle_text as handle_realtime_conversation_text;
use async_channel::Receiver;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_protocol::protocol::Submission;
use tracing::Instrument;
use tracing::debug_span;
use tracing::info_span;

use crate::session::SteerInputError;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::session::SessionSettingsUpdate;
use crate::tools::handlers::builtin_scratchpad::ScratchpadCheckpointRestore;
use crate::tools::handlers::builtin_scratchpad::restore_thread_scratchpad_checkpoint;
use crate::tools::handlers::builtin_scratchpad::scratchpad_absent_update_event;
use crate::tools::handlers::builtin_scratchpad::scratchpad_update_event_from_result;
use crate::tools::handlers::builtin_scratchpad::set_thread_continuous_policy;

use crate::config::Config;
use crate::review_prompts::resolve_review_request;
use crate::session::spawn_review_thread;
use crate::tasks::CompactTask;
use crate::tasks::UserShellCommandMode;
use crate::tasks::UserShellCommandTask;
use crate::tasks::execute_user_shell_command;
use codex_mcp::McpConnectionManager;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GuardianAssessmentEvent;
use codex_protocol::protocol::GuardianAssessmentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::McpServerRefreshConfig;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RealtimeConversationListVoicesResponseEvent;
use codex_protocol::protocol::RealtimeVoicesList;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::ThreadNameUpdatedEvent;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputResponse;

use crate::context_manager::is_user_turn_boundary;
use codex_protocol::config_types::MemoryAccessPolicy;
use codex_protocol::config_types::UserPreferencesMemoryBucket;
use codex_protocol::config_types::UserPreferencesMemoryBucketPolicy;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::mcp::RequestId as ProtocolRequestId;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use serde_json::Value;
use std::sync::Arc;
use tracing::debug;
use tracing::info;
use tracing::warn;

pub async fn interrupt(sess: &Arc<Session>) {
    sess.interrupt_task().await;
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

pub async fn user_input_or_turn(
    sess: &Arc<Session>,
    sub_id: String,
    op: Op,
    client_user_message_id: Option<String>,
) {
    user_input_or_turn_inner(sess, sub_id, op, client_user_message_id).await;
}

pub async fn update_thread_settings(
    sess: &Arc<Session>,
    sub_id: String,
    thread_settings: ThreadSettingsOverrides,
) {
    let updates = thread_settings_update(sess, thread_settings).await;
    let msg = match sess.update_settings(updates).await {
        Ok(()) => thread_settings_applied_event(sess).await,
        Err(err) => EventMsg::Error(ErrorEvent {
            message: format!("invalid thread settings override: {err}"),
            codex_error_info: Some(CodexErrorInfo::BadRequest),
        }),
    };
    sess.send_event_raw(Event { id: sub_id, msg }).await;
}

async fn thread_settings_update(
    sess: &Session,
    thread_settings: ThreadSettingsOverrides,
) -> SessionSettingsUpdate {
    let ThreadSettingsOverrides {
        environments,
        workspace_roots,
        profile_workspace_roots,
        approval_policy,
        approvals_reviewer,
        sandbox_policy,
        permission_profile,
        active_permission_profile,
        windows_sandbox_level,
        model,
        effort,
        summary,
        service_tier,
        collaboration_mode,
        multi_agent_mode,
        personality,
    } = thread_settings;
    let collaboration_mode = match collaboration_mode {
        Some(collaboration_mode) => collaboration_mode,
        None => {
            let state = sess.state.lock().await;
            // Model and reasoning effort live in CollaborationMode settings today, so
            // partial thread-settings updates refresh those fields on the active mode.
            state
                .session_configuration
                .collaboration_mode
                .with_updates(model, effort, /*developer_instructions*/ None)
        }
    };
    SessionSettingsUpdate {
        environments,
        workspace_roots,
        profile_workspace_roots,
        approval_policy,
        approvals_reviewer,
        sandbox_policy,
        permission_profile,
        active_permission_profile,
        windows_sandbox_level,
        collaboration_mode: Some(collaboration_mode),
        multi_agent_mode,
        reasoning_summary: summary,
        service_tier,
        personality,
        ..Default::default()
    }
}

async fn thread_settings_applied_event(sess: &Session) -> EventMsg {
    let (snapshot, reasoning_summary, collaboration_mode) = {
        let state = sess.state.lock().await;
        let session_configuration = &state.session_configuration;
        (
            session_configuration.thread_config_snapshot(),
            session_configuration.model_reasoning_summary,
            session_configuration.collaboration_mode.clone(),
        )
    };
    let cwd = snapshot.cwd().clone();
    EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
        thread_settings: ThreadSettingsSnapshot {
            model: snapshot.model,
            model_provider_id: snapshot.model_provider_id,
            service_tier: snapshot.service_tier,
            approval_policy: snapshot.approval_policy,
            approvals_reviewer: snapshot.approvals_reviewer,
            permission_profile: snapshot.permission_profile,
            active_permission_profile: snapshot.active_permission_profile,
            cwd,
            reasoning_effort: snapshot.reasoning_effort,
            reasoning_summary,
            personality: snapshot.personality,
            collaboration_mode,
            multi_agent_mode: snapshot.multi_agent_mode,
        },
    })
}

pub(super) async fn user_input_or_turn_inner(
    sess: &Arc<Session>,
    sub_id: String,
    op: Op,
    client_user_message_id: Option<String>,
) {
    let Op::UserInput {
        items,
        final_output_json_schema,
        responsesapi_client_metadata,
        additional_context,
        thread_settings,
    } = op
    else {
        unreachable!();
    };
    let emit_thread_settings_applied = thread_settings != ThreadSettingsOverrides::default();
    let mut updates = if emit_thread_settings_applied {
        thread_settings_update(sess, thread_settings).await
    } else {
        SessionSettingsUpdate::default()
    };
    updates.final_output_json_schema = Some(final_output_json_schema);

    let Ok(current_context) = sess.new_turn_with_sub_id(sub_id.clone(), updates).await else {
        // new_turn_with_sub_id already emits the error event.
        return;
    };
    if emit_thread_settings_applied {
        sess.send_event_raw(Event {
            id: sub_id.clone(),
            msg: thread_settings_applied_event(sess).await,
        })
        .await;
    }
    sess.record_scratchpad_checkpoint_before_turn(current_context.as_ref())
        .await;
    sess.maybe_emit_unknown_model_warning_for_turn(current_context.as_ref())
        .await;
    match sess
        .steer_input(
            items.clone(),
            additional_context.clone(),
            /*expected_turn_id*/ None,
            client_user_message_id.clone(),
            responsesapi_client_metadata.clone(),
        )
        .await
    {
        Ok(_) => {
            current_context.session_telemetry.user_prompt(&items);
        }
        Err(SteerInputError::NoActiveTurn(items)) => {
            if let Some(responsesapi_client_metadata) = responsesapi_client_metadata {
                current_context
                    .turn_metadata_state
                    .set_responsesapi_client_metadata(responsesapi_client_metadata);
            }
            current_context.session_telemetry.user_prompt(&items);
            sess.refresh_mcp_servers_if_requested(
                &current_context,
                Some(sess.mcp_elicitation_reviewer()),
            )
            .await;
            let additional_context_input = {
                let mut state = sess.state.lock().await;
                state.additional_context.merge(additional_context)
            };
            let mut task_input = additional_context_input
                .into_iter()
                .map(ResponseItem::from)
                .map(TurnInput::ResponseItem)
                .collect::<Vec<_>>();
            if !items.is_empty() {
                task_input.push(TurnInput::UserInput {
                    content: items,
                    client_id: client_user_message_id,
                });
            }
            if task_input.is_empty() {
                sess.send_event(
                    current_context.as_ref(),
                    EventMsg::TurnStarted(TurnStartedEvent {
                        turn_id: current_context.sub_id.clone(),
                        trace_id: current_context.trace_id.clone(),
                        started_at: current_context
                            .turn_timing_state
                            .started_at_unix_secs()
                            .await,
                        model_context_window: current_context.model_context_window(),
                        collaboration_mode_kind: current_context.collaboration_mode.mode,
                    }),
                )
                .await;
                let (completed_at, duration_ms) = current_context
                    .turn_timing_state
                    .completed_at_and_duration_ms()
                    .await;
                sess.send_event(
                    current_context.as_ref(),
                    EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: current_context.sub_id.clone(),
                        last_agent_message: None,
                        completed_at,
                        duration_ms,
                        time_to_first_token_ms: None,
                    }),
                )
                .await;
                return;
            }
            sess.spawn_task(
                Arc::clone(&current_context),
                task_input,
                crate::tasks::RegularTask::new(),
            )
            .await;
        }
        Err(err) => {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(err.to_error_event()),
            })
            .await;
        }
    }
}

/// Queues an inter-agent message, then lets the shared pending-work scheduler
/// decide whether an idle session should start a regular turn.
pub async fn inter_agent_communication(
    sess: &Arc<Session>,
    sub_id: String,
    communication: InterAgentCommunication,
) {
    let trigger_turn = communication.trigger_turn;
    sess.enqueue_mailbox_communication(communication).await;
    if trigger_turn {
        sess.maybe_start_turn_for_pending_work_with_sub_id(sub_id)
            .await;
    }
}

pub async fn run_user_shell_command(sess: &Arc<Session>, sub_id: String, command: String) {
    if let Some((turn_context, cancellation_token)) =
        sess.active_turn_context_and_cancellation_token().await
    {
        let session = Arc::clone(sess);
        tokio::spawn(async move {
            execute_user_shell_command(
                session,
                turn_context,
                command,
                cancellation_token,
                UserShellCommandMode::ActiveTurnAuxiliary,
            )
            .await;
        });
        return;
    }

    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
    sess.spawn_task(
        Arc::clone(&turn_context),
        Vec::new(),
        UserShellCommandTask::new(command),
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
    {
        match sess
            .persist_execpolicy_amendment(proposed_execpolicy_amendment)
            .await
        {
            Ok(()) => {
                sess.record_execpolicy_amendment_message(
                    &event_turn_id,
                    proposed_execpolicy_amendment,
                )
                .await;
            }
            Err(err) => {
                let message = format!("Failed to apply execpolicy amendment: {err}");
                tracing::warn!("{message}");
                let warning = EventMsg::Warning(WarningEvent { message });
                sess.send_event_raw(Event {
                    id: event_turn_id.clone(),
                    msg: warning,
                })
                .await;
            }
        }
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

pub async fn refresh_mcp_servers(sess: &Arc<Session>, refresh_config: McpServerRefreshConfig) {
    let mut guard = sess.pending_mcp_server_refresh_config.lock().await;
    *guard = Some(refresh_config);
}

pub async fn reload_user_config(sess: &Arc<Session>) {
    sess.reload_user_config_layer().await;
}

pub async fn compact(sess: &Arc<Session>, sub_id: String) {
    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;

    sess.spawn_task(Arc::clone(&turn_context), Vec::new(), CompactTask)
        .await;
}

pub async fn drop_memories(sess: &Arc<Session>, config: &Arc<Config>, sub_id: String) {
    let mut errors = Vec::new();

    if let Some(state_db) = sess.services.state_db.as_deref() {
        if let Err(err) = state_db.clear_memory_data().await {
            errors.push(format!("failed clearing memory rows from state db: {err}"));
        }
    } else {
        errors.push("state db unavailable; memory rows were not cleared".to_string());
    }

    for memory_root in [
        config.codex_home.join("memories"),
        config.codex_home.join("memories_extensions"),
        config.codex_home.join("user_preferences_memory"),
        config.codex_home.join("orchestrator_memory"),
    ] {
        if let Err(err) = clear_memory_root_contents(&memory_root).await {
            errors.push(format!(
                "failed clearing memory directory {}: {err}",
                memory_root.display()
            ));
        }
    }

    if errors.is_empty() {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Warning(WarningEvent {
                message: format!(
                    "Dropped memories under {} and cleared memory rows from state db.",
                    config.codex_home.display()
                ),
            }),
        })
        .await;
        return;
    }

    sess.send_event_raw(Event {
        id: sub_id,
        msg: EventMsg::Error(ErrorEvent {
            message: format!("Memory drop completed with errors: {}", errors.join("; ")),
            codex_error_info: Some(CodexErrorInfo::Other),
        }),
    })
    .await;
}

pub async fn update_memories(sess: &Arc<Session>, _config: &Arc<Config>, sub_id: String) {
    sess.send_event_raw(Event {
        id: sub_id,
        msg: EventMsg::Error(ErrorEvent {
            message: "Manual memory update is unavailable in this core session; memory generation is handled by the app-server memory pipeline after user turns.".to_string(),
            codex_error_info: Some(CodexErrorInfo::Other),
        }),
    })
    .await;
}

pub fn consolidate_orchestrator_memory(sess: &Arc<Session>, config: &Arc<Config>, sub_id: String) {
    let sess = Arc::clone(sess);
    let config = Arc::clone(config);
    tokio::spawn(async move {
        match crate::orchestrator_memory::run_cleanup_now_for_session(&sess, &config).await {
            Ok(result) => {
                sess.send_event_raw(Event {
                    id: sub_id,
                    msg: EventMsg::Warning(WarningEvent {
                        message: format!(
                            "Orchestrator memory consolidation completed. Raw events: {} -> {} (removed {}).",
                            result.raw_events_before,
                            result.raw_events_after,
                            result.removed_raw_events
                        ),
                    }),
                })
                .await;
            }
            Err(err) => {
                sess.send_event_raw(Event {
                    id: sub_id,
                    msg: EventMsg::Error(ErrorEvent {
                        message: format!("Failed to consolidate orchestrator memory: {err}"),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                })
                .await;
            }
        }
    });
}

pub fn forget_orchestrator_memory(
    sess: &Arc<Session>,
    config: &Arc<Config>,
    sub_id: String,
    needle: String,
) {
    let sess = Arc::clone(sess);
    let config = Arc::clone(config);
    tokio::spawn(async move {
        let Some(_permit) = sess.memory_write_permit().await else {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: "Memory writes are disabled for this session; enable memory generation before editing user preferences memory.".to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            })
            .await;
            return;
        };

        let bucket_policy = sess.user_preferences_memory_policy().await;
        if !UserPreferencesMemoryBucket::all()
            .iter()
            .copied()
            .all(|bucket| bucket_policy.can_write(bucket))
        {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: "Orchestrator memory forget requires write access to all user-preferences memory buckets; widen this session's userPreferencesMemoryPolicy.writeBuckets before running this global maintenance command.".to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            })
            .await;
            return;
        }

        match crate::orchestrator_memory::prune_entries_matching_needle(
            &config.codex_home,
            &config.orchestrator_memory,
            &needle,
        )
        .await
        {
            Ok(result) => {
                sess.send_event_raw(Event {
                    id: sub_id,
                    msg: EventMsg::Warning(WarningEvent {
                        message: format!(
                            "Orchestrator memory forget completed for `{needle}`. Removed preference events: {}; summary lines: {}; profile lines: {}.",
                            result.removed_preference_events,
                            result.removed_summary_lines,
                            result.removed_profile_lines
                        ),
                    }),
                })
                .await;
            }
            Err(err) => {
                sess.send_event_raw(Event {
                    id: sub_id,
                    msg: EventMsg::Error(ErrorEvent {
                        message: format!("Orchestrator memory forget failed: {err}"),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                })
                .await;
            }
        }
    });
}

pub fn migrate_user_preferences_memory(sess: &Arc<Session>, config: &Arc<Config>, sub_id: String) {
    let sess = Arc::clone(sess);
    let config = Arc::clone(config);
    tokio::spawn(async move {
        let Some(_permit) = sess.memory_write_permit().await else {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: "Memory writes are disabled for this session; enable memory generation before migrating user preferences memory.".to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            })
            .await;
            return;
        };

        match crate::orchestrator_memory::migrate_orchestrator_memory_to_user_preferences(
            &config.codex_home,
        ) {
            Ok(true) => {
                sess.send_event_raw(Event {
                    id: sub_id,
                    msg: EventMsg::Warning(WarningEvent {
                        message: "User preferences memory migration completed.".to_string(),
                    }),
                })
                .await;
            }
            Ok(false) => {
                sess.send_event_raw(Event {
                    id: sub_id,
                    msg: EventMsg::Warning(WarningEvent {
                        message: "No legacy orchestrator memory files were found to migrate."
                            .to_string(),
                    }),
                })
                .await;
            }
            Err(err) => {
                sess.send_event_raw(Event {
                    id: sub_id,
                    msg: EventMsg::Error(ErrorEvent {
                        message: format!("User preferences memory migration failed: {err}"),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                })
                .await;
            }
        }
    });
}

pub async fn thread_rollback(sess: &Arc<Session>, sub_id: String, num_turns: u32) {
    if num_turns == 0 {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "num_turns must be >= 1".to_string(),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }

    let has_active_turn = {
        sess.active_turn
            .lock()
            .await
            .as_ref()
            .is_some_and(|active_turn| active_turn.task.is_some())
    };
    if has_active_turn {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "Cannot rollback while a turn is in progress.".to_string(),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }

    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
    let live_thread = match sess.live_thread_for_persistence("rollback thread") {
        Ok(live_thread) => live_thread,
        Err(_) => {
            sess.send_event_raw(Event {
                id: turn_context.sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "thread rollback requires persisted thread history".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                }),
            })
            .await;
            return;
        }
    };
    if let Err(err) = live_thread.flush().await {
        sess.send_event_raw(Event {
            id: turn_context.sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
                message: format!("failed to flush thread persistence for rollback replay: {err}"),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }

    let stored_history = match live_thread.load_history(/*include_archived*/ false).await {
        Ok(history) => history,
        Err(err) => {
            sess.send_event_raw(Event {
                id: turn_context.sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: format!("failed to load thread history for rollback replay: {err}"),
                    codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                }),
            })
            .await;
            return;
        }
    };

    let rollback_event = ThreadRolledBackEvent { num_turns };
    let rollback_msg = EventMsg::ThreadRolledBack(rollback_event.clone());
    let replay_items = stored_history
        .items
        .into_iter()
        .chain(std::iter::once(RolloutItem::EventMsg(rollback_msg.clone())))
        .collect::<Vec<_>>();
    sess.apply_rollout_reconstruction(turn_context.as_ref(), replay_items.as_slice())
        .await;
    sess.services
        .agent_control
        .rollout_budget()
        .rearm_reminder(sess.thread_id());
    sess.recompute_token_usage(turn_context.as_ref()).await;

    sess.persist_rollout_items(&[RolloutItem::EventMsg(rollback_msg.clone())])
        .await;
    if let Err(err) = sess.flush_rollout().await {
        sess.send_event(
            turn_context.as_ref(),
            EventMsg::Warning(WarningEvent {
                message: format!(
                    "Rolled the thread back, but failed to save the rollback marker. Codex will continue retrying. Error: {err}"
                ),
            }),
        )
        .await;
    }

    sess.deliver_event_raw(Event {
        id: turn_context.sub_id.clone(),
        msg: rollback_msg,
    })
    .await;
    restore_scratchpad_after_thread_rollback(&turn_context, sess).await;
}

pub(super) async fn persist_thread_memory_mode_update(
    sess: &Session,
    mode: ThreadMemoryMode,
) -> anyhow::Result<()> {
    let live_thread = sess.live_thread_for_persistence("update thread memory mode")?;
    live_thread.persist().await?;
    live_thread.flush().await?;
    live_thread
        .update_memory_mode(mode, /*include_archived*/ false)
        .await?;
    live_thread.flush().await?;
    Ok(())
}

async fn persist_thread_name_update(
    sess: &Session,
    event: ThreadNameUpdatedEvent,
) -> anyhow::Result<EventMsg> {
    let msg = EventMsg::ThreadNameUpdated(event);
    let item = RolloutItem::EventMsg(msg.clone());
    let live_thread = sess.live_thread_for_persistence("rename thread")?;
    live_thread.persist().await?;
    live_thread
        .append_items(std::slice::from_ref(&item))
        .await?;
    live_thread.flush().await?;
    Ok(msg)
}

/// Persists the thread name in the rollout and state database, updates in-memory state, and
/// emits a `ThreadNameUpdated` event on success.
pub async fn set_thread_name(sess: &Arc<Session>, sub_id: String, name: String) {
    let Some(name) = crate::util::normalize_thread_name(&name) else {
        let event = Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "Thread name cannot be empty.".to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        };
        sess.send_event_raw(event).await;
        return;
    };

    let updated = ThreadNameUpdatedEvent {
        thread_id: sess.thread_id,
        thread_name: Some(name.clone()),
    };

    let msg = match persist_thread_name_update(sess, updated).await {
        Ok(msg) => msg,
        Err(err) => {
            warn!("Failed to persist thread name update to rollout: {err}");
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event_raw(event).await;
            return;
        }
    };

    if let Some(state_db) = sess.services.state_db.as_deref()
        && let Err(err) = state_db.update_thread_title(sess.thread_id, &name).await
    {
        warn!("Failed to update thread title in state db: {err}");
    }

    {
        let mut state = sess.state.lock().await;
        state.session_configuration.thread_name = Some(name.clone());
    }

    let codex_home = sess.get_config().await.codex_home.clone();
    if let Err(err) = crate::rollout::append_thread_name(&codex_home, sess.thread_id, &name).await {
        warn!("Failed to update legacy thread name index: {err}");
    }

    sess.deliver_event_raw(Event { id: sub_id, msg }).await;
}

async fn restore_scratchpad_after_thread_rollback(
    turn_context: &Arc<crate::session::turn_context::TurnContext>,
    sess: &Arc<Session>,
) {
    let max_checkpoints = turn_context
        .config
        .scratchpad
        .rollback
        .max_user_turn_checkpoints;
    if max_checkpoints == 0 {
        return;
    }

    let scratchpad_id = sess.thread_id.to_string();
    let target_turn_index = sess.user_turn_count().await;
    match restore_thread_scratchpad_checkpoint(
        &turn_context.config.codex_home,
        &scratchpad_id,
        target_turn_index,
        max_checkpoints,
    ) {
        Ok(ScratchpadCheckpointRestore::Restored(scratchpad)) => {
            if let Some(event) = scratchpad_update_event_from_result(&serde_json::json!({
                "scratchpad": scratchpad,
            })) {
                sess.send_event(turn_context.as_ref(), EventMsg::ScratchpadUpdate(event))
                    .await;
            }
        }
        Ok(ScratchpadCheckpointRestore::RestoredAbsent { deleted }) => {
            if deleted {
                sess.send_event(
                    turn_context.as_ref(),
                    EventMsg::ScratchpadUpdate(scratchpad_absent_update_event(scratchpad_id)),
                )
                .await;
            }
        }
        Ok(ScratchpadCheckpointRestore::MissingCheckpoint) => {
            sess.send_event(
                turn_context.as_ref(),
                EventMsg::Warning(WarningEvent {
                    message: "Rolled the thread back, but no scratchpad checkpoint was retained for this boundary; leaving the current scratchpad unchanged.".to_string(),
                }),
            )
            .await;
        }
        Err(err) => {
            sess.send_event(
                turn_context.as_ref(),
                EventMsg::Warning(WarningEvent {
                    message: format!(
                        "Rolled the thread back, but could not restore scratchpad state. Error: {err}"
                    ),
                }),
            )
            .await;
        }
    }
}

pub async fn set_scratchpad_continuous_policy(sess: &Arc<Session>, sub_id: String, enabled: bool) {
    let codex_home = sess.get_config().await.codex_home.clone();
    let result = set_thread_continuous_policy(&codex_home, &sess.thread_id.to_string(), enabled);
    let result = match result {
        Ok(result) => result,
        Err(err) => {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            })
            .await;
            return;
        }
    };
    if let Some(event) = scratchpad_update_event_from_result(&result) {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::ScratchpadUpdate(event),
        })
        .await;
    }
}

pub async fn prune_idle_agents(sess: &Arc<Session>, sub_id: String) {
    match sess
        .services
        .agent_control
        .prune_idle_agents(sess.thread_id)
        .await
    {
        Ok(report) => {
            let closed_count = report.closed.len();
            if !report.failed.is_empty() {
                let failed = report
                    .failed
                    .into_iter()
                    .map(|(thread_id, err)| format!("{thread_id}: {err}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                sess.send_event_raw(Event {
                    id: sub_id.clone(),
                    msg: EventMsg::Error(ErrorEvent {
                        message: format!("Failed to prune some idle agents: {failed}"),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                })
                .await;
            }
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Warning(WarningEvent {
                    message: if closed_count == 0 {
                        "No idle agents were eligible to prune.".to_string()
                    } else {
                        format!("Pruned {closed_count} idle agent session(s).")
                    },
                }),
            })
            .await;
        }
        Err(err) => {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: format!("Failed to prune idle agents: {err}"),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            })
            .await;
        }
    }
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
                message: err.to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
    }
}

/// Applies the session-local outer memories read/write policy.
///
/// This affects subsequent turns in the current live session only; persistent
/// defaults still come from `[memories]` in config.
pub async fn set_memory_access_policy(
    sess: &Arc<Session>,
    sub_id: String,
    policy: MemoryAccessPolicy,
) {
    if let Err(err) = sess
        .update_settings(SessionSettingsUpdate {
            memory_policy: Some(policy),
            ..Default::default()
        })
        .await
    {
        warn!("Failed to update memory access policy: {err}");
        let event = Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: err.to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
    }
}

/// Applies the session-local user preferences memory bucket policy.
///
/// This affects subsequent turns in the current live session only; persistent
/// defaults still come from `[user_preferences_memory]` in config.
pub async fn set_user_preferences_memory_policy(
    sess: &Arc<Session>,
    sub_id: String,
    policy: UserPreferencesMemoryBucketPolicy,
) {
    if let Err(err) = sess
        .update_settings(SessionSettingsUpdate {
            user_preferences_memory_policy: Some(policy),
            ..Default::default()
        })
        .await
    {
        warn!("Failed to update user preferences memory policy: {err}");
        let event = Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: err.to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
    }
}

async fn clear_memory_root_contents(memory_root: &std::path::Path) -> std::io::Result<()> {
    match tokio::fs::symlink_metadata(memory_root).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to clear symlinked memory root {}",
                    memory_root.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    tokio::fs::create_dir_all(memory_root).await?;
    let mut entries = tokio::fs::read_dir(memory_root).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;
        if file_type.is_dir() {
            tokio::fs::remove_dir_all(path).await?;
        } else {
            tokio::fs::remove_file(path).await?;
        }
    }
    Ok(())
}

async fn shutdown_session_runtime(sess: &Arc<Session>) {
    if let Some(startup_prewarm) = sess.take_session_startup_prewarm().await {
        startup_prewarm.abort().await;
    }
    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
    let _ = sess.conversation.shutdown().await;
    sess.services
        .unified_exec_manager
        .terminate_all_processes()
        .await;
    if let Err(err) = sess.services.code_mode_service.shutdown().await {
        warn!("failed to shutdown code mode session: {err}");
    }
    let config = sess.get_config().await;
    let old_mcp_manager = sess.services.mcp_connection_manager.swap(Arc::new(
        McpConnectionManager::new_uninitialized_with_permission_profile(
            &config.permissions.approval_policy,
            config.permissions.permission_profile(),
            config.prefix_mcp_tool_names(),
        ),
    ));
    match Arc::try_unwrap(old_mcp_manager) {
        Ok(manager) => manager.shutdown().await,
        Err(_) => warn!("skipping MCP shutdown because the manager still has active references"),
    }
    sess.guardian_review_session.shutdown().await;
}

async fn emit_thread_stop_lifecycle(sess: &Session) {
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
        .iter()
        .filter(|item| is_user_turn_boundary(item))
        .count();
    sess.services.session_telemetry.counter(
        "codex.conversation.turn.count",
        i64::try_from(turn_count).unwrap_or(0),
        &[],
    );

    emit_thread_stop_lifecycle(sess.as_ref()).await;

    // Gracefully flush and shutdown thread persistence on session end so tests
    // that inspect durable state do not race with the background writer.
    if let Some(live_thread) = sess.live_thread()
        && let Err(e) = live_thread.shutdown().await
    {
        warn!("failed to shutdown thread persistence: {e}");
        let event = Event {
            id: sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
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

pub async fn review(
    sess: &Arc<Session>,
    config: &Arc<Config>,
    sub_id: String,
    review_request: ReviewRequest,
) {
    let turn_context = sess.new_default_turn_with_sub_id(sub_id.clone()).await;
    sess.maybe_emit_unknown_model_warning_for_turn(turn_context.as_ref())
        .await;
    sess.refresh_mcp_servers_if_requested(&turn_context, Some(sess.mcp_elicitation_reviewer()))
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
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event(&turn_context, event.msg).await;
        }
    }
}

pub(super) async fn submission_loop(
    sess: Arc<Session>,
    config: Arc<Config>,
    rx_sub: Receiver<Submission>,
) {
    // To break out of this loop, send Op::Shutdown.
    let mut shutdown_received = false;
    while let Ok(sub) = rx_sub.recv().await {
        debug!(?sub, "Submission");
        let dispatch_span = submission_dispatch_span(&sub);
        let should_exit = async {
            match sub.op.clone() {
                Op::Interrupt => {
                    interrupt(&sess).await;
                    false
                }
                Op::CleanBackgroundTerminals => {
                    clean_background_terminals(&sess).await;
                    false
                }
                Op::RealtimeConversationStart(params) => {
                    if let Err(err) =
                        handle_realtime_conversation_start(&sess, sub.id.clone(), params).await
                    {
                        sess.send_event_raw(Event {
                            id: sub.id.clone(),
                            msg: EventMsg::Error(ErrorEvent {
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
                    handle_realtime_conversation_close(&sess, sub.id.clone()).await;
                    false
                }
                Op::RealtimeConversationListVoices => {
                    realtime_conversation_list_voices(&sess, sub.id.clone()).await;
                    false
                }
                Op::ThreadSettings { thread_settings } => {
                    update_thread_settings(&sess, sub.id.clone(), thread_settings).await;
                    false
                }
                Op::UserInput { .. } => {
                    user_input_or_turn(&sess, sub.id.clone(), sub.op, sub.client_user_message_id)
                        .await;
                    false
                }
                Op::InterAgentCommunication { communication } => {
                    inter_agent_communication(&sess, sub.id.clone(), communication).await;
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
                Op::RefreshMcpServers { config } => {
                    refresh_mcp_servers(&sess, config).await;
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
                Op::DropMemories => {
                    drop_memories(&sess, &config, sub.id.clone()).await;
                    false
                }
                Op::UpdateMemories => {
                    update_memories(&sess, &config, sub.id.clone()).await;
                    false
                }
                Op::ConsolidateOrchestratorMemory => {
                    consolidate_orchestrator_memory(&sess, &config, sub.id.clone());
                    false
                }
                Op::OrchestratorMemoryForget { needle } => {
                    forget_orchestrator_memory(&sess, &config, sub.id.clone(), needle);
                    false
                }
                Op::UserPreferencesMemoryMigrate => {
                    migrate_user_preferences_memory(&sess, &config, sub.id.clone());
                    false
                }
                Op::ThreadRollback { num_turns } => {
                    thread_rollback(&sess, sub.id.clone(), num_turns).await;
                    false
                }
                Op::SetThreadName { name } => {
                    set_thread_name(&sess, sub.id.clone(), name).await;
                    false
                }
                Op::SetScratchpadContinuousPolicy { enabled } => {
                    set_scratchpad_continuous_policy(&sess, sub.id.clone(), enabled).await;
                    false
                }
                Op::PruneIdleAgents => {
                    prune_idle_agents(&sess, sub.id.clone()).await;
                    false
                }
                Op::SetThreadMemoryMode { mode } => {
                    set_thread_memory_mode(&sess, sub.id.clone(), mode).await;
                    false
                }
                Op::SetMemoryAccessPolicy { policy } => {
                    set_memory_access_policy(&sess, sub.id.clone(), policy).await;
                    false
                }
                Op::SetUserPreferencesMemoryPolicy { policy } => {
                    set_user_preferences_memory_policy(&sess, sub.id.clone(), policy).await;
                    false
                }
                Op::RunUserShellCommand { command } => {
                    run_user_shell_command(&sess, sub.id.clone(), command).await;
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
                Op::Shutdown => shutdown(&sess, sub.id.clone()).await,
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
        if should_exit {
            shutdown_received = true;
            break;
        }
    }
    // If the submission loop exits because the channel closed without an
    // explicit shutdown op, still run session teardown.
    if !shutdown_received {
        shutdown_session_runtime(&sess).await;
        emit_thread_stop_lifecycle(sess.as_ref()).await;
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
    let approval_prefix = crate::guardian::AUTO_REVIEW_DENIED_ACTION_APPROVAL_DEVELOPER_PREFIX;
    let text = format!(
        r#"{approval_prefix}

Treat this as approval to perform that exact action in the same context in which it was originally requested.
Do not assume this also authorizes similar operations with different payloads.

Approved action:
{approved_action_json}"#,
    );
    let items = vec![ResponseItem::from(ResponseInputItem::Message {
        role: "developer".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
    })];

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn clear_memory_root_contents_preserves_root_directory() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("memories");
        let nested_dir = root.join("rollout_summaries");
        tokio::fs::create_dir_all(&nested_dir)
            .await
            .expect("create rollout summaries dir");
        tokio::fs::write(root.join("MEMORY.md"), "stale memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(nested_dir.join("rollout.md"), "stale rollout\n")
            .await
            .expect("write rollout summary");

        clear_memory_root_contents(&root)
            .await
            .expect("clear memory root contents");

        assert!(
            tokio::fs::try_exists(&root)
                .await
                .expect("check memory root existence"),
            "memory root should still exist after clearing contents"
        );
        let mut entries = tokio::fs::read_dir(&root)
            .await
            .expect("read memory root after clear");
        assert!(
            entries
                .next_entry()
                .await
                .expect("read next entry")
                .is_none(),
            "memory root should be empty after clearing contents"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clear_memory_root_contents_rejects_symlinked_root() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("outside");
        tokio::fs::create_dir_all(&target)
            .await
            .expect("create symlink target dir");
        let target_file = target.join("keep.txt");
        tokio::fs::write(&target_file, "keep\n")
            .await
            .expect("write target file");

        let root = dir.path().join("memories");
        std::os::unix::fs::symlink(&target, &root).expect("create memory root symlink");

        let err = clear_memory_root_contents(&root)
            .await
            .expect_err("symlinked memory root should be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            tokio::fs::try_exists(&target_file)
                .await
                .expect("check target file existence"),
            "rejecting a symlinked memory root should not delete the symlink target"
        );
    }
}
