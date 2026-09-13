use super::handlers;
use super::session::Session;
use crate::state::TaskKind;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::turn_input::SuspendTurnOutcome;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SuspensionScope {
    Root,
    Handoff,
    HandoffAfterDescendants,
}

pub(super) async fn suspend_turn_and_shutdown(
    session: &Arc<Session>,
    submission_id: String,
) -> CodexResult<SuspendTurnOutcome> {
    suspend_turn_and_shutdown_with_scope(session, submission_id, SuspensionScope::Root).await
}

pub(super) async fn suspend_turn_and_shutdown_for_handoff(
    session: &Arc<Session>,
    submission_id: String,
) -> CodexResult<SuspendTurnOutcome> {
    suspend_turn_and_shutdown_for_handoff_with_descendants(
        session,
        submission_id,
        SuspensionScope::Handoff,
    )
    .await
}

pub(super) async fn suspend_turn_and_shutdown_for_handoff_after_descendants(
    session: &Arc<Session>,
    submission_id: String,
) -> CodexResult<SuspendTurnOutcome> {
    suspend_turn_and_shutdown_for_handoff_with_descendants(
        session,
        submission_id,
        SuspensionScope::HandoffAfterDescendants,
    )
    .await
}

async fn suspend_turn_and_shutdown_for_handoff_with_descendants(
    session: &Arc<Session>,
    submission_id: String,
    scope: SuspensionScope,
) -> CodexResult<SuspendTurnOutcome> {
    if !session
        .services
        .agent_control
        .handoff_admission_sealed()
    {
        return Err(CodexErr::InvalidRequest(
            "handoff suspension requires a sealed agent tree".to_string(),
        ));
    }
    let preflight = match scope {
        SuspensionScope::Handoff => session.handoff_preflight().await,
        SuspensionScope::HandoffAfterDescendants => session.handoff_preflight_after_descendants().await,
        SuspensionScope::Root => unreachable!("root suspension does not use handoff preflight"),
    };
    if !preflight.blockers.is_empty() {
        return Ok(SuspendTurnOutcome::Blocked {
            blockers: preflight.blockers,
        });
    }
    suspend_turn_and_shutdown_with_scope(session, submission_id, scope).await
}

async fn suspend_turn_and_shutdown_with_scope(
    session: &Arc<Session>,
    submission_id: String,
    scope: SuspensionScope,
) -> CodexResult<SuspendTurnOutcome> {
    {
        let active = session.active_turn.lock().await;
        let Some(task) = active.as_ref().and_then(|turn| turn.task.as_ref()) else {
            return Ok(SuspendTurnOutcome::NotActive);
        };
        if task.kind != TaskKind::Regular {
            return Ok(SuspendTurnOutcome::UnsupportedTask);
        }
    }

    if scope == SuspensionScope::Root && session.session_source().await.is_non_root_agent() {
        return Err(CodexErr::UnsupportedOperation(
            "turn suspension requires the owning root thread".to_string(),
        ));
    }

    // This is checked again after the caller's fence and preflight. A descendant admitted before
    // the fence still owns work and must be drained child-first before this thread can close.
    if matches!(scope, SuspensionScope::Root | SuspensionScope::Handoff)
        && session
            .services
            .agent_control
            .list_live_agent_subtree_thread_ids(session.thread_id)
            .await?
            .len()
            > 1
    {
        return Ok(SuspendTurnOutcome::HasLiveDescendants);
    }

    let live_thread = session
        .live_thread_for_persistence("suspend an unfinished turn")
        .map_err(|error| CodexErr::Fatal(error.to_string()))?;
    // Flush before canceling execution so a persistence failure leaves the original turn running.
    live_thread.flush().await.map_err(|error| {
        CodexErr::Fatal(format!("flush before turn suspension failed: {error}"))
    })?;

    // The flush can yield while the active turn completes or changes. Recheck its
    // kind under the same lock used to remove it.
    let mut turn = {
        let mut active = session.active_turn.lock().await;
        let Some(active_turn) = active.as_ref() else {
            return Ok(SuspendTurnOutcome::NotActive);
        };
        let Some(task) = active_turn.task.as_ref() else {
            return Ok(SuspendTurnOutcome::NotActive);
        };
        if task.kind != TaskKind::Regular {
            return Ok(SuspendTurnOutcome::UnsupportedTask);
        }
        active.take().ok_or_else(|| {
            CodexErr::Fatal("accepted turn suspension had no running turn".to_string())
        })?
    };

    let task = turn.task.take().ok_or_else(|| {
        CodexErr::Fatal("accepted turn suspension had no running task".to_string())
    })?;
    let turn_id = task.turn_context.sub_id.clone();
    // Normal shutdown records a terminal turn event, preventing another worker from
    // recovering this turn under its original ID. Cancel the task without that event.
    task.cancellation_token.cancel();
    task.turn_context
        .turn_metadata_state
        .cancel_git_enrichment_task();
    let mut task_handle = task.handle.detach();
    let mut blockers = Vec::new();
    match tokio::time::timeout(
        Duration::from_millis(crate::tasks::GRACEFULL_INTERRUPTION_TIMEOUT_MS),
        &mut task_handle,
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!(thread_id = %session.thread_id, %error, "suspended turn task exited abnormally");
            blockers.push(codex_protocol::turn_input::HandoffBlocker::TaskExitedUnexpectedly);
        }
        Err(_) => {
            warn!(
                thread_id = %session.thread_id,
                "suspended turn task did not stop gracefully; aborting it"
            );
            task_handle.abort();
            let _ = task_handle.await;
            blockers.push(codex_protocol::turn_input::HandoffBlocker::SuspensionTimeout);
        }
    }
    // Pending accepted input and interactive waiters live only in this process. Handoff
    // intentionally drops that state; persisting or replaying it needs a separate protocol.
    session.input_queue.clear_pending(&turn).await;

    // Stop all producers before flushing their final history and closing its writer. Once the
    // active task has been removed, any persistence failure makes this node non-transferable;
    // the handoff coordinator must retain a NeedsAttention receipt instead of replacing it.
    handlers::shutdown_session_runtime(session).await;
    if let Err(error) = live_thread.flush().await {
        warn!(thread_id = %session.thread_id, %error, "flush after turn suspension failed");
        if scope == SuspensionScope::Root {
            return Err(CodexErr::Fatal(format!(
                "flush after root turn suspension failed: {error}"
            )));
        }
        blockers.push(codex_protocol::turn_input::HandoffBlocker::Persistence);
    }
    if let Err(error) = live_thread.shutdown().await {
        warn!(thread_id = %session.thread_id, %error, "close suspended turn writer failed");
        if scope == SuspensionScope::Root {
            return Err(CodexErr::Fatal(format!(
                "close suspended root turn writer failed: {error}"
            )));
        }
        blockers.push(codex_protocol::turn_input::HandoffBlocker::Persistence);
    }
    if !blockers.is_empty() && scope != SuspensionScope::Root {
        return Ok(SuspendTurnOutcome::Blocked { blockers });
    }
    // Announce thread shutdown only after its writer closes so a replacement worker
    // cannot write the same thread concurrently.
    handlers::emit_thread_stop_lifecycle(session.as_ref()).await;
    session
        .deliver_event_raw(Event {
            id: submission_id,
            msg: EventMsg::ShutdownComplete,
        })
        .await;
    Ok(SuspendTurnOutcome::Suspended { turn_id })
}
