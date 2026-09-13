//! Session-local execution activity and cooperative manual-pause gates.

use super::new_submission_id;
use super::session::Session;
use crate::state::ActiveTurn;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadActivity;
use codex_protocol::protocol::ThreadActivityUpdatedEvent;
use codex_protocol::protocol::ThreadActivityWaitReason;
use codex_protocol::protocol::ThreadPauseState;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

/// Whether an admitted activity operation is a model stream or a tool/MCP operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivityOperationKind {
    Model,
    Tool,
}

/// Guard covering one model/tool operation for activity reporting.
///
/// Dropping the guard never cancels the operation. It synchronously releases the in-flight slot
/// and wakes quiescence waiters, then asynchronously publishes the resulting state so a pause
/// request can transition from `Pausing` to `Paused` after the operation exits.
pub(crate) struct ActivityOperationGuard {
    session: Arc<Session>,
    kind: ActivityOperationKind,
}

impl Drop for ActivityOperationGuard {
    fn drop(&mut self) {
        if self.kind == ActivityOperationKind::Model {
            self.session
                .model_activity_in_flight
                .fetch_sub(1, Ordering::AcqRel);
        }
        // Publish the kind-specific counter first. A model and tool can finish concurrently;
        // decrementing the shared counter first would transiently make a still-running tool
        // look like a model-only operation to handoff preflight.
        self.session
            .activity_in_flight
            .fetch_sub(1, Ordering::AcqRel);
        self.session.activity_operation_notify.notify_waiters();
        let session = Arc::clone(&self.session);
        if let Ok(handle) = Handle::try_current() {
            handle.spawn(async move {
                session.publish_activity_state().await;
            });
        }
    }
}

/// Guard covering one non-wait tool dispatch while it is waiting for readiness or execution
/// admission. This handoff-only count keeps a Worker from waking its parent ahead of a sibling
/// tool that has been spawned but has not reached its activity operation guard yet.
pub(crate) struct HandoffDispatchGuard {
    session: Arc<Session>,
}

impl Drop for HandoffDispatchGuard {
    fn drop(&mut self) {
        self.session
            .handoff_dispatches_pending
            .fetch_sub(1, Ordering::AcqRel);
        self.session.activity_operation_notify.notify_waiters();
    }
}

impl Session {
    /// Returns true while the root activity control has paused this thread tree.
    pub(crate) fn is_activity_paused(&self) -> bool {
        self.services.agent_control.root_activity_paused()
    }

    /// Wait at a cooperative turn boundary until `/continue` releases the root tree.
    pub(crate) async fn wait_for_activity_resume(
        &self,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<()> {
        loop {
            // Register before checking the flag so a continue edge between the check and the
            // await cannot strand a retained turn. `Notify::notify_waiters` is intentionally
            // edge-triggered; the loop's flag check supplies the level-triggered state.
            let resume_notify = self.services.agent_control.root_activity_resume_notify();
            let notified = resume_notify.notified();
            if !self.is_activity_paused() {
                break;
            }
            tokio::select! {
                _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
                _ = notified => {},
            }
        }
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        Ok(())
    }

    /// Update this session's process-local pause state and publish its ephemeral snapshot.
    pub(crate) async fn set_activity_pause_requested(&self, paused: bool) {
        if paused {
            // Invalidate automatic Lead deadlines and their generated wake messages. Queue-only
            // communication remains retained for the resumed turn.
            self.cancel_lead_oversight().await;
        }
        self.publish_activity_state().await;
    }

    /// Begin tracking a model/tool operation. The caller retains the guard until the operation
    /// reaches its natural completion; manual pause never drops it early.
    pub(crate) async fn begin_activity_operation(
        self: &Arc<Self>,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<ActivityOperationGuard> {
        self.begin_activity_operation_with_kind(cancellation_token, ActivityOperationKind::Tool)
            .await
    }

    /// Admit one model response stream. Model cancellation can be resumed by exact turn ID after
    /// a handoff; tool and MCP operations remain blockers because shutdown would terminate them.
    pub(crate) async fn begin_model_sampling_operation(
        self: &Arc<Self>,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<ActivityOperationGuard> {
        self.begin_activity_operation_with_kind(cancellation_token, ActivityOperationKind::Model)
            .await
    }

    async fn begin_activity_operation_with_kind(
        self: &Arc<Self>,
        cancellation_token: &CancellationToken,
        kind: ActivityOperationKind,
    ) -> CodexResult<ActivityOperationGuard> {
        loop {
            let _admission = self
                .services
                .agent_control
                .begin_handoff_admission()?;
            if kind == ActivityOperationKind::Model {
                // Increment the model counter before the shared activity slot so a preflight
                // cannot observe an admitted stream as an unknown tool operation between those
                // two atomic updates. The handoff permit closes the admission race while both
                // counters are updated.
                self.model_activity_in_flight
                    .fetch_add(1, Ordering::AcqRel);
            }
            let admitted = self
                .services
                .agent_control
                .admit_activity_operation(&self.activity_in_flight)
                .await;
            drop(_admission);
            if admitted {
                self.publish_activity_state().await;
                return Ok(ActivityOperationGuard {
                    session: Arc::clone(self),
                    kind,
                });
            }
            if kind == ActivityOperationKind::Model {
                self.model_activity_in_flight
                    .fetch_sub(1, Ordering::AcqRel);
            }
            if self.services.agent_control.handoff_admission_sealed() {
                return Err(CodexErr::TurnAborted);
            }
            self.wait_for_activity_resume(cancellation_token).await?;
        }
    }

    pub(crate) fn non_model_activity_in_flight(&self) -> u32 {
        self.activity_in_flight
            .load(Ordering::Acquire)
            .saturating_sub(self.model_activity_in_flight.load(Ordering::Acquire))
    }

    /// Register a non-wait tool dispatch before its task is spawned so dependency-free handoffs
    /// observe siblings that are still waiting for readiness or the parallel execution gate.
    pub(crate) fn begin_handoff_dispatch(
        self: &Arc<Self>,
    ) -> CodexResult<HandoffDispatchGuard> {
        // Registration itself is a short admission boundary. The guard then remains alive until
        // the sibling reaches a real activity operation or exits, so handoff can classify it as
        // pending dispatch without waiting for the tool's full execution.
        let _admission = self.services.agent_control.begin_handoff_admission()?;
        self.handoff_dispatches_pending
            .fetch_add(1, Ordering::AcqRel);
        Ok(HandoffDispatchGuard {
            session: Arc::clone(self),
        })
    }

    pub(crate) fn pending_handoff_dispatches(&self) -> u32 {
        self.handoff_dispatches_pending.load(Ordering::Acquire)
    }

    /// Wait until admitted activity operations and pending non-wait dispatches have drained.
    ///
    /// The notification is registered before the counter check so an operation that finishes
    /// between those steps cannot strand the caller. Callers must recheck their own state after
    /// this boundary because a new operation may be admitted immediately afterward.
    pub(crate) async fn wait_for_activity_quiescence(
        &self,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<()> {
        loop {
            let notified = self.activity_operation_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.activity_in_flight.load(Ordering::Acquire) == 0
                && self.pending_handoff_dispatches() == 0
            {
                return Ok(());
            }
            tokio::select! {
                _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
                _ = &mut notified => {},
            }
        }
    }

    /// Return the current live activity state for reconnect/status paths.
    pub(crate) async fn activity_state(&self) -> ThreadActivityUpdatedEvent {
        let in_flight_operations = self.activity_in_flight.load(Ordering::Acquire);
        let active_turn = self.active_turn.lock().await;
        let (activity, wait_reason) = self
            .activity_for_active_turn(active_turn.as_ref(), in_flight_operations)
            .await;
        let pause_state = if self.is_activity_paused() {
            if activity == ThreadActivity::Working {
                ThreadPauseState::Pausing
            } else {
                ThreadPauseState::Paused
            }
        } else {
            ThreadPauseState::Running
        };
        ThreadActivityUpdatedEvent {
            thread_id: self.thread_id,
            root_thread_id: codex_protocol::ThreadId::from(
                self.services.agent_control.session_id(),
            ),
            activity,
            pause_state,
            wait_reason,
            in_flight_operations,
        }
    }

    async fn activity_for_active_turn(
        &self,
        active_turn: Option<&ActiveTurn>,
        in_flight_operations: u32,
    ) -> (ThreadActivity, Option<ThreadActivityWaitReason>) {
        let Some(active_turn) = active_turn else {
            return if in_flight_operations > 0 {
                (ThreadActivity::Working, None)
            } else {
                (ThreadActivity::Idle, None)
            };
        };
        let turn_state = active_turn.turn_state.lock().await;
        if self.usage_resume_waiting.load(Ordering::Acquire) {
            return (
                ThreadActivity::Waiting,
                Some(ThreadActivityWaitReason::UsageLimit),
            );
        }
        if turn_state.has_pending_approval() {
            return (
                ThreadActivity::Waiting,
                Some(ThreadActivityWaitReason::Approval),
            );
        }
        if turn_state.has_pending_user_input() {
            return (
                ThreadActivity::Waiting,
                Some(ThreadActivityWaitReason::UserInput),
            );
        }
        drop(turn_state);
        // An admitted operation is real execution even while direct Workers are active. Only
        // classify a Lead as waiting for Agents after all of its operations have quiesced.
        if in_flight_operations > 0 {
            return (ThreadActivity::Working, None);
        }
        let active_direct_worker_count = self
            .services
            .agent_control
            .active_direct_worker_count(self.thread_id)
            .await;
        classify_active_turn_activity(in_flight_operations, active_direct_worker_count)
    }

    pub(super) async fn publish_activity_state(&self) {
        self.send_event_raw_ephemeral(Event {
            id: new_submission_id(),
            msg: EventMsg::ThreadActivityUpdated(self.activity_state().await),
        })
        .await;
    }
}

fn classify_active_turn_activity(
    in_flight_operations: u32,
    active_direct_worker_count: usize,
) -> (ThreadActivity, Option<ThreadActivityWaitReason>) {
    if in_flight_operations > 0 {
        return (ThreadActivity::Working, None);
    }
    if active_direct_worker_count > 0 {
        return (
            ThreadActivity::Waiting,
            Some(ThreadActivityWaitReason::Agents),
        );
    }
    // An admitted turn with no operation in flight is waiting at a cooperative boundary. If it
    // is manually paused, this explicitly avoids presenting the retained turn as working.
    (ThreadActivity::Waiting, None)
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
