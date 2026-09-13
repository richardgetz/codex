//! Process-wide admission fence for daemon handoff.
//!
//! `ThreadManager` owns this fence so a coordinator can close every root and
//! descendant creation path before taking a stable graph snapshot. Existing
//! threads continue to use their per-tree [`crate::HandoffGuard`]; this gate
//! only prevents a new manager-owned thread from being created or loaded while
//! the handoff is draining.

use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

const SEALED_ERROR: &str =
    "thread manager handoff is draining; new roots and descendants are temporarily unavailable";

/// Shared process-wide state used by one [`crate::ThreadManager`].
///
/// The state is deliberately separate from a thread's root `AgentControl`:
/// a manager can own multiple independent roots, while each root still has its own tree gate for
/// active work.
#[derive(Debug)]
pub(crate) struct ThreadManagerHandoffState {
    sealed: AtomicBool,
    in_flight: AtomicU32,
    notify: Notify,
    /// Keeps replacement sessions from claiming durable inbound work until the recovery
    /// coordinator has loaded the complete graph and admitted its exact turns.
    recovery_pending: AtomicBool,
    recovery_in_flight: AtomicU32,
    recovery_notify: Notify,
}

impl Default for ThreadManagerHandoffState {
    fn default() -> Self {
        Self {
            sealed: AtomicBool::new(false),
            in_flight: AtomicU32::new(0),
            notify: Notify::new(),
            recovery_pending: AtomicBool::new(false),
            recovery_in_flight: AtomicU32::new(0),
            recovery_notify: Notify::new(),
        }
    }
}

impl ThreadManagerHandoffState {
    pub(crate) fn begin(self: &Arc<Self>) -> CodexResult<ThreadManagerHandoffGuard> {
        if self.sealed.swap(true, Ordering::AcqRel) {
            return Err(CodexErr::InvalidRequest(
                "a handoff is already in progress for this thread manager".to_string(),
            ));
        }
        Ok(ThreadManagerHandoffGuard {
            state: Arc::clone(self),
        })
    }

    pub(crate) fn begin_admission(
        self: &Arc<Self>,
    ) -> CodexResult<ThreadManagerHandoffAdmissionGuard> {
        if self.sealed.load(Ordering::Acquire) {
            return Err(CodexErr::InvalidRequest(SEALED_ERROR.to_string()));
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        if self.sealed.load(Ordering::Acquire) {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            self.notify.notify_waiters();
            return Err(CodexErr::InvalidRequest(SEALED_ERROR.to_string()));
        }
        Ok(ThreadManagerHandoffAdmissionGuard {
            state: Arc::clone(self),
        })
    }

    pub(crate) fn sealed(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
    }

    /// Mark this manager as recovering before loading any replacement sessions.
    ///
    /// The guard is intentionally fail-closed: dropping it without calling
    /// [`ThreadManagerRecoveryGuard::complete`] leaves the bit set so a failed or interrupted
    /// recovery cannot consume durable inbound work from a partially restored graph. A later
    /// explicit recovery attempt may acquire another guard and clear the bit only after it has
    /// completed successfully.
    pub(crate) fn begin_recovery_pending(self: &Arc<Self>) -> ThreadManagerRecoveryGuard {
        self.recovery_pending.store(true, Ordering::Release);
        ThreadManagerRecoveryGuard {
            state: Arc::clone(self),
            completed: false,
        }
    }

    /// Admit one short durable-inbound poller claim while recovery is not pending.
    ///
    /// The second bit check closes the race where recovery starts after the first check but before
    /// the poller claims a row. The coordinator waits for these permits before loading nodes, so a
    /// claim that crossed the boundary finishes its unclaim/enqueue work before recovery proceeds.
    pub(crate) fn begin_recovery_admission(
        self: &Arc<Self>,
    ) -> Option<ThreadManagerRecoveryAdmissionGuard> {
        if self.recovery_pending.load(Ordering::Acquire) {
            return None;
        }
        self.recovery_in_flight.fetch_add(1, Ordering::AcqRel);
        if self.recovery_pending.load(Ordering::Acquire) {
            self.recovery_in_flight.fetch_sub(1, Ordering::AcqRel);
            self.recovery_notify.notify_waiters();
            return None;
        }
        Some(ThreadManagerRecoveryAdmissionGuard {
            state: Arc::clone(self),
        })
    }

    pub(crate) fn recovery_pending(&self) -> bool {
        self.recovery_pending.load(Ordering::Acquire)
    }
}

/// RAII owner of the process-wide manager handoff fence.
///
/// Dropping this guard is the explicit abort/release operation: admission is
/// reopened and an unfinished journal remains the coordinator's durable record
/// of any node that needs attention. A successful coordinator keeps the guard
/// alive through old-runtime shutdown and until replacement recovery has been
/// durably acknowledged.
#[derive(Debug)]
pub struct ThreadManagerHandoffGuard {
    state: Arc<ThreadManagerHandoffState>,
}

impl ThreadManagerHandoffGuard {
    /// Wait until manager-owned thread creations admitted before sealing finish.
    pub async fn wait_for_admissions(&self) {
        loop {
            let notified = self.state.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Abort the handoff and reopen manager admission.
    ///
    /// This is equivalent to dropping the guard and is named for coordinators
    /// that need a visible fail-safe path after a blocked preflight.
    pub fn abort(self) {
        drop(self);
    }
}

impl Drop for ThreadManagerHandoffGuard {
    fn drop(&mut self) {
        self.state.sealed.store(false, Ordering::Release);
        self.state.notify.notify_waiters();
    }
}

/// RAII permit for one manager-owned thread creation or load operation.
///
/// The permit must cover the complete startup operation, including registration
/// in the manager map, so the graph snapshot cannot race a thread that has only
/// partially initialized.
#[derive(Debug)]
pub struct ThreadManagerHandoffAdmissionGuard {
    state: Arc<ThreadManagerHandoffState>,
}

impl Drop for ThreadManagerHandoffAdmissionGuard {
    fn drop(&mut self) {
        self.state.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.state.notify.notify_waiters();
    }
}

/// RAII owner of the manager-wide replacement recovery fence.
///
/// Dropping this guard before [`Self::complete`] keeps recovery pending, so a failed or
/// interrupted replacement cannot consume durable inbound work from a partially restored graph.
#[derive(Debug)]
pub struct ThreadManagerRecoveryGuard {
    state: Arc<ThreadManagerHandoffState>,
    completed: bool,
}

impl ThreadManagerRecoveryGuard {
    /// Wait for durable inbound claims that crossed the recovery boundary to finish.
    pub async fn wait_for_admissions(&self) {
        loop {
            let notified = self.state.recovery_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.recovery_in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Mark replacement recovery complete and reopen durable inbound polling.
    pub fn complete(mut self) {
        self.state.recovery_pending.store(false, Ordering::Release);
        self.state.recovery_notify.notify_waiters();
        self.completed = true;
    }
}

impl Drop for ThreadManagerRecoveryGuard {
    fn drop(&mut self) {
        // Keep failed or interrupted recovery fail-closed. A later explicit recovery operation
        // may acquire another guard and call complete after it has restored the graph safely.
        if !self.completed {
            self.state.recovery_notify.notify_waiters();
        }
    }
}

/// RAII permit for one durable inbound poller claim during normal runtime.
#[derive(Debug)]
pub(crate) struct ThreadManagerRecoveryAdmissionGuard {
    state: Arc<ThreadManagerHandoffState>,
}

impl Drop for ThreadManagerRecoveryAdmissionGuard {
    fn drop(&mut self) {
        self.state.recovery_in_flight.fetch_sub(1, Ordering::AcqRel);
        self.state.recovery_notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::ThreadManagerHandoffState;
    use codex_protocol::error::CodexErrorDetails;
    use std::sync::Arc;

    #[test]
    fn abort_reopens_global_admission() {
        let state = Arc::new(ThreadManagerHandoffState::default());
        let guard = state.begin().expect("first handoff");
        assert!(state.sealed());
        assert!(matches!(
            state.begin_admission().unwrap_err().details(),
            CodexErrorDetails::InvalidRequest(_)
        ));
        guard.abort();
        assert!(!state.sealed());
        let admission = state.begin_admission().expect("admission after abort");
        drop(admission);
    }

    #[tokio::test]
    async fn wait_for_admissions_observes_preexisting_creation() {
        let state = Arc::new(ThreadManagerHandoffState::default());
        let admission = state.begin_admission().expect("admission");
        let guard = state.begin().expect("handoff");
        let waiter = tokio::spawn(async move {
            guard.wait_for_admissions().await;
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(admission);
        waiter.await.expect("waiter");
    }

    #[test]
    fn failed_recovery_stays_pending_until_explicit_success() {
        let state = Arc::new(ThreadManagerHandoffState::default());
        let guard = state.begin_recovery_pending();
        assert!(state.recovery_pending());
        assert!(state.begin_recovery_admission().is_none());

        drop(guard);
        assert!(state.recovery_pending());

        let guard = state.begin_recovery_pending();
        guard.complete();
        assert!(!state.recovery_pending());
        assert!(state.begin_recovery_admission().is_some());
    }

    #[tokio::test]
    async fn recovery_wait_drains_claims_before_completion() {
        let state = Arc::new(ThreadManagerHandoffState::default());
        let admission = state
            .begin_recovery_admission()
            .expect("normal poller admission");
        let guard = state.begin_recovery_pending();
        let state_for_assertion = Arc::clone(&state);
        let waiter = tokio::spawn(async move {
            guard.wait_for_admissions().await;
        });

        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(admission);
        waiter.await.expect("recovery admission waiter");
        assert!(state_for_assertion.recovery_pending());
    }
}
