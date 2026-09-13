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
use codex_protocol::ThreadId;
use std::collections::HashSet;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

const HANDOFF_DELIVERY_REGISTRATION_CLOSED: u64 = 1 << 63;
const HANDOFF_DELIVERY_COUNT_MASK: u64 = u32::MAX as u64;
const HANDOFF_WATCHER_COUNT_SHIFT: u32 = 32;
const HANDOFF_WATCHER_COUNT_INCREMENT: u64 = 1 << HANDOFF_WATCHER_COUNT_SHIFT;
const HANDOFF_WATCHER_COUNT_MASK: u64 = ((1u64 << 31) - 1) << HANDOFF_WATCHER_COUNT_SHIFT;

/// RAII owner of the root-tree handoff admission fence.
///
/// Dropping the guard reopens admission. A coordinator must keep it alive
/// until all node receipts have been persisted and the replacement process has
/// either restored or marked every node.
#[derive(Debug)]
pub struct HandoffGuard {
    sealed: Arc<AtomicBool>,
    in_flight: Arc<AtomicU32>,
    delivery_state: Arc<AtomicU64>,
    delivery_failed: Arc<AtomicBool>,
    inbound_unsupported: Arc<AtomicBool>,
    notify: Arc<Notify>,
    suspended_threads: Arc<StdMutex<HashSet<ThreadId>>>,
}

impl Drop for HandoffGuard {
    fn drop(&mut self) {
        // An aborted handoff leaves any process-local fallback mailbox available to the old
        // runtime. A subsequent attempt starts with fresh failure state.
        self.delivery_failed.store(false, Ordering::Release);
        self.inbound_unsupported.store(false, Ordering::Release);
        let delivery_state = self.delivery_state.load(Ordering::Acquire);
        if delivery_count(delivery_state) == 0 && watcher_count(delivery_state) == 0 {
            self.suspended_threads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
        self.delivery_state.fetch_and(
            !HANDOFF_DELIVERY_REGISTRATION_CLOSED,
            Ordering::Release,
        );
        self.sealed.store(false, Ordering::Release);
        self.notify.notify_waiters();
    }
}

impl HandoffGuard {
    /// Wait until admissions that crossed the fence before sealing have completed, then close
    /// delivery registration and drain terminal callbacks that were already registered.
    ///
    /// The registration close is part of the same atomic state as the delivery counters. A
    /// completion watcher that races this boundary either holds a counted obligation or is
    /// rejected and must use its durable replacement path; it cannot become an untracked old
    /// mailbox write after this method returns.
    pub async fn wait_for_admissions(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.in_flight.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }

        self.close_delivery_registration();
        self.wait_for_delivery_count().await;
    }

    /// Wait for detached completion watchers after active sessions have been suspended.
    ///
    /// A coordinator must call this after child-first suspension and before publishing a
    /// transferable receipt. Watcher registrations are separate from short terminal delivery
    /// windows so this method does not wait for an ordinary worker's original model task before it
    /// is suspended.
    pub async fn wait_for_handoff_watchers(&self) {
        self.close_delivery_registration();
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let state = self.delivery_state.load(Ordering::Acquire);
            if delivery_count(state) == 0 && watcher_count(state) == 0 {
                self.suspended_threads
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clear();
                return;
            }
            notified.await;
        }
    }

    fn close_delivery_registration(&self) {
        self.delivery_state
            .fetch_or(HANDOFF_DELIVERY_REGISTRATION_CLOSED, Ordering::AcqRel);
    }

    async fn wait_for_delivery_count(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if delivery_count(self.delivery_state.load(Ordering::Acquire)) == 0 {
                return;
            }
            notified.await;
        }
    }
}

fn delivery_count(state: u64) -> u64 {
    state & HANDOFF_DELIVERY_COUNT_MASK
}

fn watcher_count(state: u64) -> u64 {
    (state & HANDOFF_WATCHER_COUNT_MASK) >> HANDOFF_WATCHER_COUNT_SHIFT
}

fn try_register(state: &AtomicU64, increment: u64, count_mask: u64) -> bool {
    let mut current = state.load(Ordering::Acquire);
    loop {
        if current & HANDOFF_DELIVERY_REGISTRATION_CLOSED != 0
            || current & count_mask == count_mask
        {
            return false;
        }
        let Some(next) = current.checked_add(increment) else {
            return false;
        };
        match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

/// RAII permit for one terminal completion delivery.
///
/// Completion watchers acquire this short-lived permit only after a child reaches a terminal
/// status. It keeps the coordinator's final handoff barrier ordered through the durable write (or
/// the explicitly failed fallback) without waiting for the worker's entire lifetime.
#[derive(Debug)]
pub(crate) struct HandoffDeliveryGuard {
    delivery_state: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl Drop for HandoffDeliveryGuard {
    fn drop(&mut self) {
        self.delivery_state.fetch_sub(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }
}

/// RAII permit for one detached completion watcher.
///
/// The permit is acquired while the spawn admission is still held and lives until the watcher has
/// either persisted or delivered its terminal result. Handoff drains these permits after active
/// nodes are suspended, which closes the race where a watcher starts after the short delivery
/// barrier but before the old runtime exits.
#[derive(Debug)]
pub(crate) struct HandoffCompletionWatcherGuard {
    delivery_state: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl Drop for HandoffCompletionWatcherGuard {
    fn drop(&mut self) {
        self.delivery_state
            .fetch_sub(HANDOFF_WATCHER_COUNT_INCREMENT, Ordering::AcqRel);
        self.notify.notify_waiters();
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
            delivery_state: Arc::clone(&self.handoff_delivery_state),
            delivery_failed: Arc::clone(&self.handoff_delivery_failed),
            inbound_unsupported: Arc::clone(&self.handoff_inbound_unsupported),
            suspended_threads: Arc::clone(&self.handoff_suspended_threads),
            notify: Arc::clone(&self.handoff_admission_notify),
        })
    }

    /// Register a terminal completion callback while it is being transferred or retained.
    ///
    /// Registration is rejected after the coordinator closes the delivery boundary. Callers that
    /// receive `None` must use the durable manager-owned state database path and mark the handoff
    /// unsafe if that write fails; they must not enqueue an old-runtime-only fallback as success.
    pub(crate) fn begin_handoff_delivery(&self) -> Option<HandoffDeliveryGuard> {
        try_register(
            &self.handoff_delivery_state,
            1,
            HANDOFF_DELIVERY_COUNT_MASK,
        )
        .then(|| HandoffDeliveryGuard {
            delivery_state: Arc::clone(&self.handoff_delivery_state),
            notify: Arc::clone(&self.handoff_admission_notify),
        })
    }

    /// Register a terminal abort/completion before a handoff can close delivery registration.
    ///
    /// A callback that reaches this method after the close boundary is already unsafe to
    /// transfer from the old runtime, so it marks the shared handoff failed closed instead of
    /// returning an untracked obligation.
    pub(crate) fn begin_handoff_terminal_delivery(&self) -> Option<HandoffDeliveryGuard> {
        let delivery = self.begin_handoff_delivery();
        if delivery.is_none()
            && self.handoff_admission_sealed()
            && delivery_count(self.handoff_delivery_state.load(Ordering::Acquire)) == 0
        {
            // An already-counted terminal callback may still be inside send_event after
            // registration closes. It remains covered by its outer guard, so do not turn this
            // nested observation into a false handoff failure. A zero count is genuinely late.
            self.mark_handoff_delivery_failed();
        }
        delivery
    }

    /// Register a detached V1 completion watcher before its task is spawned.
    ///
    /// Spawn callers hold a normal admission permit through this call, so the coordinator first
    /// drains all such callers and then atomically closes watcher registration. A `None` result is
    /// a fail-closed setup error; the caller must not start an untracked watcher during handoff.
    pub(crate) fn begin_handoff_completion_watcher(
        &self,
    ) -> Option<HandoffCompletionWatcherGuard> {
        try_register(
            &self.handoff_delivery_state,
            HANDOFF_WATCHER_COUNT_INCREMENT,
            HANDOFF_WATCHER_COUNT_MASK,
        )
        .then(|| HandoffCompletionWatcherGuard {
            delivery_state: Arc::clone(&self.handoff_delivery_state),
            notify: Arc::clone(&self.handoff_admission_notify),
        })
    }

    /// Mark a completion as unsafe for replacement because durable persistence failed.
    pub(crate) fn mark_handoff_delivery_failed(&self) {
        self.handoff_delivery_failed.store(true, Ordering::Release);
        self.handoff_admission_notify.notify_waiters();
    }

    pub(crate) fn handoff_delivery_failed(&self) -> bool {
        self.handoff_delivery_failed.load(Ordering::Acquire)
    }

    /// Mark a child whose active turn was intentionally stopped by handoff.
    ///
    /// The marker is consumed by the detached completion watcher when it observes the resulting
    /// `Shutdown` status. It is deliberately keyed by thread rather than globally: a sibling that
    /// completes naturally during the same handoff must still deliver its terminal result.
    pub(crate) fn mark_handoff_suspended(&self, thread_id: ThreadId) {
        self.handoff_suspended_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(thread_id);
    }

    /// Consume the marker for a child whose shutdown status reached its watcher.
    pub(crate) fn take_handoff_suspended(&self, thread_id: ThreadId) -> bool {
        self.handoff_suspended_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&thread_id)
    }

    /// Mark a durable inbound payload for a compatible replacement instead of consuming it.
    pub(crate) fn mark_handoff_inbound_unsupported(&self) {
        self.handoff_inbound_unsupported
            .store(true, Ordering::Release);
        self.handoff_admission_notify.notify_waiters();
    }

    pub(crate) fn handoff_inbound_unsupported(&self) -> bool {
        self.handoff_inbound_unsupported.load(Ordering::Acquire)
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

    /// Returns true while the replacement manager is loading and admitting a recovered graph.
    pub(crate) fn recovery_pending(&self) -> bool {
        self.manager
            .upgrade()
            .map(|state| state.recovery_pending())
            .unwrap_or(false)
    }

    /// Admit one durable inbound poller claim while replacement recovery is idle.
    pub(crate) fn begin_recovery_admission(
        &self,
    ) -> Option<crate::thread_manager_handoff::ThreadManagerRecoveryAdmissionGuard> {
        self.manager
            .upgrade()
            .and_then(|state| state.begin_recovery_admission())
    }
}

#[cfg(test)]
mod tests {
    use super::AgentControl;
    use super::HandoffGuard;
    use codex_protocol::ThreadId;
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

    #[tokio::test]
    async fn delivery_registration_closes_after_admission_drain() {
        let control = AgentControl::default();
        let admission = control.begin_handoff_admission().expect("admission");
        let guard = control.begin_handoff().expect("handoff");
        drop(admission);
        guard.wait_for_admissions().await;
        assert!(control.begin_handoff_delivery().is_none());
        assert!(control.begin_handoff_completion_watcher().is_none());
        drop(guard);
    }

    #[tokio::test]
    async fn watcher_wait_is_separate_from_admission_drain() {
        let control = AgentControl::default();
        let watcher = control
            .begin_handoff_completion_watcher()
            .expect("watcher registration");
        let guard = control.begin_handoff().expect("handoff");
        guard.wait_for_admissions().await;
        let wait = guard.wait_for_handoff_watchers();
        tokio::pin!(wait);
        tokio::select! {
            () = &mut wait => panic!("watcher should keep the final barrier open"),
            () = tokio::time::sleep(std::time::Duration::from_millis(1)) => {}
        }
        drop(watcher);
        wait.await;
        drop(guard);
    }

    #[tokio::test]
    async fn terminal_delivery_guard_keeps_handoff_barrier_open() {
        let control = AgentControl::default();
        let terminal = control
            .begin_handoff_terminal_delivery()
            .expect("terminal delivery registration");
        let guard = control.begin_handoff().expect("handoff");
        let wait = guard.wait_for_admissions();
        tokio::pin!(wait);
        tokio::select! {
            () = &mut wait => panic!("terminal delivery should keep the barrier open"),
            () = tokio::time::sleep(std::time::Duration::from_millis(1)) => {}
        }
        drop(terminal);
        wait.await;
        drop(guard);
    }

    #[tokio::test]
    async fn late_terminal_delivery_marks_handoff_failed() {
        let control = AgentControl::default();
        let guard = control.begin_handoff().expect("handoff");
        guard.wait_for_admissions().await;
        assert!(control.begin_handoff_terminal_delivery().is_none());
        assert!(control.handoff_delivery_failed());
        drop(guard);
    }

    #[test]
    fn dropping_guard_clears_suspension_markers() {
        let control = AgentControl::default();
        let thread_id = ThreadId::new();
        let guard = control.begin_handoff().expect("handoff");
        control.mark_handoff_suspended(thread_id);
        drop(guard);
        assert!(!control.take_handoff_suspended(thread_id));
    }

    #[test]
    fn guard_is_not_cloneable() {
        fn assert_send<T: Send>() {}
        assert_send::<HandoffGuard>();
    }
}
