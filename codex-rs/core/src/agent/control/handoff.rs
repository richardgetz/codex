//! Process-local admission fence for durable daemon handoff.
//!
//! The fence belongs to the root `AgentControl`, which is shared by every
//! loaded descendant. It rejects new user turns and agent spawns while a
//! handoff owner snapshots and drains the tree. Existing operations are left
//! alone; the caller must still inspect and persist each node before stopping
//! it. Recovery submissions also pass this fence on the old runtime, while a
//! replacement process has a new control handle and can install an unfinished
//! turn behind its already-applied manual pause gate.

use super::AgentControl;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

/// RAII owner of the root-tree handoff admission fence.
///
/// Dropping the guard reopens admission. A coordinator must keep it alive
/// until all node receipts have been persisted and the replacement process has
/// either restored or marked every node.
#[derive(Debug)]
pub struct HandoffGuard {
    sealed: Arc<AtomicBool>,
    in_flight: Arc<AtomicU32>,
    notify: Arc<Notify>,
}

impl Drop for HandoffGuard {
    fn drop(&mut self) {
        self.sealed.store(false, Ordering::Release);
    }
}

impl HandoffGuard {
    /// Wait until admissions that crossed the fence before sealing have completed.
    ///
    /// New admissions fail immediately after the handoff seals the
    /// tree. Existing admissions are allowed to reach their normal submission boundary
    /// before the coordinator snapshots and suspends nodes.
    pub async fn wait_for_admissions(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// RAII permit for one operation admitted before a handoff seal.
///
/// Callers keep this guard alive across every process-local mutation or remote request that must
/// finish before the handoff coordinator snapshots and closes the thread. Dropping it releases
/// the in-flight admission count; it never cancels the operation.
#[derive(Debug)]
pub struct HandoffAdmissionGuard {
    in_flight: Arc<AtomicU32>,
    notify: Arc<Notify>,
}

impl Drop for HandoffAdmissionGuard {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }
}

impl AgentControl {
    /// Seal this root tree against new turn and spawn admission.
    pub(crate) fn begin_handoff(&self) -> CodexResult<HandoffGuard> {
        if self
            .handoff_admission_sealed
            .swap(true, Ordering::AcqRel)
        {
            return Err(CodexErr::InvalidRequest(
                "a handoff is already in progress for this agent tree".to_string(),
            ));
        }
        Ok(HandoffGuard {
            sealed: Arc::clone(&self.handoff_admission_sealed),
            in_flight: Arc::clone(&self.handoff_admission_in_flight),
            notify: Arc::clone(&self.handoff_admission_notify),
        })
    }

    /// Admit one operation that is already at its final turn/spawn boundary.
    ///
    /// The second fence check closes the race where a handoff seals after the first
    /// check but before this operation reaches its submission boundary. A caller holds
    /// the returned guard only across that boundary, then drops it before long-running
    /// model or tool work begins.
    pub(crate) fn begin_handoff_admission(&self) -> CodexResult<HandoffAdmissionGuard> {
        if self.handoff_admission_sealed.load(Ordering::Acquire) {
            return Err(CodexErr::InvalidRequest(
                "agent tree handoff is draining; new work is temporarily unavailable".to_string(),
            ));
        }
        self.handoff_admission_in_flight
            .fetch_add(1, Ordering::AcqRel);
        if self.handoff_admission_sealed.load(Ordering::Acquire) {
            self.handoff_admission_in_flight
                .fetch_sub(1, Ordering::AcqRel);
            self.handoff_admission_notify.notify_waiters();
            return Err(CodexErr::InvalidRequest(
                "agent tree handoff is draining; new work is temporarily unavailable".to_string(),
            ));
        }
        Ok(HandoffAdmissionGuard {
            in_flight: Arc::clone(&self.handoff_admission_in_flight),
            notify: Arc::clone(&self.handoff_admission_notify),
        })
    }

    pub(crate) fn handoff_admission_sealed(&self) -> bool {
        self.handoff_admission_sealed.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::AgentControl;
    use super::HandoffGuard;
    use codex_protocol::error::CodexErrorDetails;

    #[test]
    fn guard_seals_until_dropped() {
        let control = AgentControl::default();
        assert!(!control.handoff_admission_sealed());
        let guard = control.begin_handoff().expect("first handoff");
        assert!(control.handoff_admission_sealed());
        assert!(matches!(
            control.begin_handoff().unwrap_err().details(),
            CodexErrorDetails::InvalidRequest(_)
        ));
        drop(guard);
        assert!(!control.handoff_admission_sealed());
    }

    #[tokio::test]
    async fn wait_for_admissions_observes_preexisting_work() {
        let control = AgentControl::default();
        let admission = control.begin_handoff_admission().expect("admission");
        let guard = control.begin_handoff().expect("handoff");
        let waiter = tokio::spawn(async move {
            guard.wait_for_admissions().await;
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(admission);
        waiter.await.expect("waiter");
    }

    #[test]
    fn guard_is_not_cloneable() {
        fn assert_send<T: Send>() {}
        assert_send::<HandoffGuard>();
    }
}
