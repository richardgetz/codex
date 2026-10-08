use super::super::session::Session;
use crate::context::NodeReplReviewEvidence;
use crate::state::ReasoningEffortPin;
use crate::tools::handlers::builtin_scratchpad;
use codex_history::RolloutItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadNameUpdatedEvent;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::WarningEvent;
use codex_thread_store::PersistContext;
use std::sync::Arc;
use tracing::warn;

pub(in crate::session) async fn thread_rollback(
    session: &Arc<Session>,
    submission_id: String,
    num_turns: u32,
) {
    if num_turns == 0 {
        send_rollback_error(session, submission_id, "num_turns must be >= 1").await;
        return;
    }

    let has_active_turn = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .is_some_and(|active_turn| active_turn.task.is_some());
    if has_active_turn {
        send_rollback_error(session, submission_id, "Cannot rollback while a turn is in progress.").await;
        return;
    }

    let turn_context = session
        .new_turn_with_default_settings(submission_id, Default::default())
        .await;
    let live_thread = match session.live_thread_for_persistence("rollback thread") {
        Ok(live_thread) => live_thread,
        Err(_) => {
            send_rollback_error(
                session,
                turn_context.sub_id.clone(),
                "thread rollback requires persisted thread history",
            )
            .await;
            return;
        }
    };
    if let Err(error) = live_thread.flush().await {
        send_rollback_error(
            session,
            turn_context.sub_id.clone(),
            &format!("failed to flush thread persistence for rollback replay: {error}"),
        )
        .await;
        return;
    }

    let stored_history = match live_thread.load_history(/*include_archived*/ false).await {
        Ok(history) => history,
        Err(error) => {
            send_rollback_error(
                session,
                turn_context.sub_id.clone(),
                &format!("failed to load thread history for rollback replay: {error}"),
            )
            .await;
            return;
        }
    };

    let rollback_event = ThreadRolledBackEvent { num_turns };
    let rollback_msg = EventMsg::ThreadRolledBack(rollback_event);
    let replay_items = stored_history
        .items
        .into_iter()
        .chain(std::iter::once(RolloutItem::EventMsg(rollback_msg.clone())))
        .collect::<Vec<_>>();
    session
        .apply_rollout_reconstruction(&turn_context, replay_items.as_slice())
        .await;
    {
        let mut state = session.state.lock().await;
        if state.startup_prewarm.is_none() {
            state.reasoning_effort_pin = ReasoningEffortPin::Unset;
        }
    }
    session
        .services
        .thread_extension_data
        .remove::<NodeReplReviewEvidence>();
    session
        .services
        .local_agent_control()
        .rearm_budget_reminder(session.thread_id());
    session.recompute_token_usage(turn_context.as_ref()).await;

    session
        .persist_rollout_items(&[RolloutItem::EventMsg(rollback_msg.clone())])
        .await;
    if let Err(error) = session.flush_rollout().await {
        session
            .send_event(
                turn_context.as_ref(),
                EventMsg::Warning(WarningEvent {
                    message: format!(
                        "Rolled the thread back, but failed to save the rollback marker. Codex will continue retrying. Error: {error}"
                    ),
                }),
            )
            .await;
    }

    session
        .deliver_event_raw(Event {
            id: turn_context.sub_id.clone(),
            msg: rollback_msg,
        })
        .await;
    restore_scratchpad_after_thread_rollback(turn_context.as_ref(), session).await;
}

async fn send_rollback_error(session: &Arc<Session>, submission_id: String, message: &str) {
    session
        .send_event_raw(Event {
            id: submission_id,
            msg: EventMsg::Error(ErrorEvent {
                misalignment: None,
                message: message.to_string(),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
}

async fn restore_scratchpad_after_thread_rollback(
    turn_context: &crate::session::turn_context::TurnContext,
    session: &Arc<Session>,
) {
    let max_checkpoints = turn_context
        .config
        .scratchpad
        .rollback
        .max_user_turn_checkpoints;
    if max_checkpoints == 0 {
        return;
    }

    let scratchpad_id = session.thread_id.to_string();
    let target_turn_index = session.user_turn_count().await;
    match builtin_scratchpad::restore_thread_scratchpad_checkpoint(
        &turn_context.config.codex_home,
        &scratchpad_id,
        target_turn_index,
        max_checkpoints,
    ) {
        Ok(builtin_scratchpad::ScratchpadCheckpointRestore::Restored(scratchpad)) => {
            if let Some(event) = builtin_scratchpad::scratchpad_update_event_from_result(
                &serde_json::json!({"scratchpad": scratchpad}),
            ) {
                session
                    .send_event(turn_context, EventMsg::ScratchpadUpdate(event))
                    .await;
            }
        }
        Ok(builtin_scratchpad::ScratchpadCheckpointRestore::RestoredAbsent { deleted }) => {
            if deleted {
                session
                    .send_event(
                        turn_context,
                        EventMsg::ScratchpadUpdate(
                            builtin_scratchpad::scratchpad_absent_update_event(scratchpad_id),
                        ),
                    )
                    .await;
            }
        }
        Ok(builtin_scratchpad::ScratchpadCheckpointRestore::MissingCheckpoint) => {
            session
                .send_event(
                    turn_context,
                    EventMsg::Warning(WarningEvent {
                        message: "Rolled the thread back, but no scratchpad checkpoint was retained for this boundary; leaving the current scratchpad unchanged.".to_string(),
                    }),
                )
                .await;
        }
        Err(error) => {
            session
                .send_event(
                    turn_context,
                    EventMsg::Warning(WarningEvent {
                        message: format!(
                            "Rolled the thread back, but could not restore scratchpad state. Error: {error}"
                        ),
                    }),
                )
                .await;
        }
    }
}

pub(in crate::session) async fn set_thread_name(
    session: &Arc<Session>,
    submission_id: String,
    name: String,
) {
    let Some(name) = crate::util::normalize_thread_name(&name) else {
        session
            .send_event_raw(Event {
                id: submission_id,
                msg: EventMsg::Error(ErrorEvent {
                    misalignment: None,
                    message: "Thread name cannot be empty.".to_string(),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                }),
            })
            .await;
        return;
    };

    let updated = ThreadNameUpdatedEvent {
        thread_id: session.thread_id(),
        thread_name: Some(name.clone()),
    };
    let msg = match persist_thread_name_update(session, updated).await {
        Ok(msg) => msg,
        Err(error) => {
            warn!("Failed to persist thread name update to rollout: {error}");
            session
                .send_event_raw(Event {
                    id: submission_id,
                    msg: EventMsg::Error(ErrorEvent {
                        misalignment: None,
                        message: error.to_string(),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                })
                .await;
            return;
        }
    };

    if let Some(state_db) = session.services.state_db.as_deref()
        && let Err(error) = state_db.update_thread_title(session.thread_id(), &name).await
    {
        warn!("Failed to update thread title in state db: {error}");
    }
    session.state.lock().await.session_configuration.thread_name = Some(name.clone());

    let codex_home = session.get_config().await.codex_home.clone();
    if let Err(error) = crate::rollout::append_thread_name(&codex_home, session.thread_id(), &name).await {
        warn!("Failed to update legacy thread name index: {error}");
    }
    session.deliver_event_raw(Event { id: submission_id, msg }).await;
}

async fn persist_thread_name_update(
    session: &Session,
    event: ThreadNameUpdatedEvent,
) -> anyhow::Result<EventMsg> {
    let msg = EventMsg::ThreadNameUpdated(event);
    let item = RolloutItem::EventMsg(msg.clone());
    let live_thread = session.live_thread_for_persistence("rename thread")?;
    live_thread.persist(PersistContext::Standard).await?;
    live_thread
        .append_items(std::slice::from_ref(&item))
        .await?;
    live_thread.flush().await?;
    Ok(msg)
}

pub(in crate::session) async fn set_scratchpad_continuous_policy(
    session: &Arc<Session>,
    submission_id: String,
    enabled: bool,
) {
    let codex_home = session.get_config().await.codex_home.clone();
    let result = match builtin_scratchpad::set_thread_continuous_policy(
        &codex_home,
        &session.thread_id().to_string(),
        enabled,
    ) {
        Ok(result) => result,
        Err(error) => {
            session
                .send_event_raw(Event {
                    id: submission_id,
                    msg: EventMsg::Error(ErrorEvent {
                        misalignment: None,
                        message: error.to_string(),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                })
                .await;
            return;
        }
    };
    if let Some(event) = builtin_scratchpad::scratchpad_update_event_from_result(&result) {
        session
            .send_event_raw(Event {
                id: submission_id,
                msg: EventMsg::ScratchpadUpdate(event),
            })
            .await;
    }
}

pub(in crate::session) async fn prune_idle_agents(session: &Arc<Session>, submission_id: String) {
    match session
        .services
        .local_agent_control()
        .prune_idle_agents(session.thread_id())
        .await
    {
        Ok(report) => {
            let closed_count = report.closed.len();
            if !report.failed.is_empty() {
                let failed = report
                    .failed
                    .into_iter()
                    .map(|(thread_id, error)| format!("{thread_id}: {error}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                session
                    .send_event_raw(Event {
                        id: submission_id.clone(),
                        msg: EventMsg::Error(ErrorEvent {
                            misalignment: None,
                            message: format!("Failed to prune some idle agents: {failed}"),
                            codex_error_info: Some(CodexErrorInfo::Other),
                        }),
                    })
                    .await;
            }
            session
                .send_event_raw(Event {
                    id: submission_id,
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
        Err(error) => {
            session
                .send_event_raw(Event {
                    id: submission_id,
                    msg: EventMsg::Error(ErrorEvent {
                        misalignment: None,
                        message: format!("Failed to prune idle agents: {error}"),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                })
                .await;
        }
    }
}
