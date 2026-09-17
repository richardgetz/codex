//! Root-scoped, process-local pause and activity propagation for agent trees.

use super::AgentControl;
use crate::session::session::Session;
use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::ThreadActivity;
use codex_protocol::protocol::ThreadActivityUpdatedEvent;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;

impl AgentControl {
    /// Returns the current manual pause switch for this root agent tree.
    pub(crate) fn root_activity_paused(&self) -> bool {
        self.root_activity_paused.load(Ordering::Acquire)
    }

    /// Returns the process-local notification used by retained turns to await `/continue`.
    pub(crate) fn root_activity_resume_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.root_activity_resume_notify)
    }

    /// Serialize a durable Team activity transition with every loaded descendant of this root.
    ///
    /// Callers may hold this guard while updating the durable pause marker and awaiting the Core
    /// pause or continue acknowledgement. It is separate from the Core activity-update mutex so
    /// the acknowledgement handler can acquire its normal gate without deadlocking recovery.
    pub(crate) async fn lock_root_activity_transition(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.root_activity_transition)
            .lock_owned()
            .await
    }

    /// Serialize thread-manager registration with the root pause snapshot boundary.
    pub(crate) async fn lock_root_activity_pause_update(
        &self,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.root_activity_pause_update)
            .lock_owned()
            .await
    }

    /// Atomically admit one model/tool operation with the root pause toggle. A pause request that
    /// acquires the update lock first prevents this increment; an operation that acquires it
    /// first is considered already in flight and is allowed to finish at its next boundary.
    pub(crate) async fn admit_activity_operation(&self, in_flight: &AtomicU32) -> bool {
        let _update_guard = self.root_activity_pause_update.lock().await;
        if self.root_activity_paused() {
            return false;
        }
        in_flight.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// Pause the root thread and every loaded ThreadSpawn descendant.
    ///
    /// The state is process-local and is intentionally not written to rollout history. A pause
    /// request never aborts an in-flight operation; each session publishes `Pausing` until its
    /// current operation reaches a cooperative boundary.
    pub(crate) async fn pause_activity_for_subtree(&self) -> Vec<ThreadActivityUpdatedEvent> {
        self.set_root_activity(true).await
    }

    /// Capture loaded Team activity immediately before applying the root pause gate.
    ///
    /// The returned tuples contain the thread id, its recorded parent, and the pre-pause
    /// activity. Callers persist only the unfinished entries before admitting a later continue;
    /// capturing under the same update lock used by activity admission prevents a new operation
    /// from appearing between the snapshot and the pause boundary.
    pub(crate) async fn pause_activity_for_subtree_with_snapshot(
        &self,
    ) -> CodexResult<Vec<(ThreadId, Option<ThreadId>, ThreadActivity)>> {
        let _update_guard = self.root_activity_pause_update.lock().await;
        let snapshots = match self.collect_activity_snapshots_with_parents().await {
            Ok(snapshots) => snapshots,
            Err(error) => {
                // Leave the root gated even when the durable snapshot cannot be captured. The
                // caller keeps the marker in its applying state, and a later continue can safely
                // release only the root rather than admitting an untracked descendant.
                let _ = self.set_root_activity_locked(true).await;
                return Err(error);
            }
        };
        self.set_root_activity_locked(true).await;
        Ok(snapshots)
    }

    /// Release a manual pause for the root thread and every loaded ThreadSpawn descendant.
    ///
    /// Retained usage waits and mailbox work are released through their existing schedulers. No
    /// synthetic model turn is created by this method.
    pub(crate) async fn continue_activity_for_subtree(&self) -> Vec<ThreadActivityUpdatedEvent> {
        self.set_root_activity(false).await
    }

    /// Return current activity snapshots for the loaded root tree. This is used by reconnect and
    /// status paths because activity events themselves are ephemeral.
    pub(crate) async fn activity_snapshot_for_subtree(&self) -> Vec<ThreadActivityUpdatedEvent> {
        self.collect_activity_snapshots().await
    }

    /// Reconcile a child after startup. Root pause publication holds the update lock through
    /// descendant propagation, so a child that was inserted during a toggle observes the latest
    /// value here instead of retaining a stale startup snapshot.
    pub(crate) async fn reconcile_spawned_activity(&self, session: &Session) {
        let _update_guard = self.root_activity_pause_update.lock().await;
        if self.root_activity_paused() {
            session.set_activity_pause_requested(true).await;
        } else {
            session.set_activity_pause_requested(false).await;
        }
    }

    async fn set_root_activity(&self, paused: bool) -> Vec<ThreadActivityUpdatedEvent> {
        let _update_guard = self.root_activity_pause_update.lock().await;
        self.set_root_activity_locked(paused).await
    }

    async fn set_root_activity_locked(&self, paused: bool) -> Vec<ThreadActivityUpdatedEvent> {
        self.root_activity_paused.store(paused, Ordering::Release);

        let _propagation_guard = self.root_activity_pause_propagation.lock().await;
        let snapshots = self.apply_activity_to_loaded_tree(paused).await;
        // Wake retained turns for either edge. A paused usage wait consumes the change and then
        // waits for the next notification; a resumed wait proceeds to its normal check.
        self.root_activity_resume_notify.notify_waiters();
        snapshots
    }

    async fn apply_activity_to_loaded_tree(&self, paused: bool) -> Vec<ThreadActivityUpdatedEvent> {
        let thread_ids = self.loaded_root_tree_ids().await;
        let Ok(state) = self.upgrade() else {
            return Vec::new();
        };
        let mut snapshots = Vec::with_capacity(thread_ids.len());
        for thread_id in thread_ids {
            let Ok(thread) = state.get_thread(thread_id).await else {
                continue;
            };
            thread.session.set_activity_pause_requested(paused).await;
            snapshots.push(thread.session.activity_state().await);
            if !paused {
                // A retained mailbox turn may have been parked before the pause request. Let
                // the existing scheduler decide whether it can now admit that same work.
                thread.session.maybe_start_turn_for_pending_work().await;
                // Resume the existing Lead oversight interval only after the pause boundary has
                // been released. The helper checks the current role, active Workers, and turn
                // admission state before arming anything.
                let _ = thread
                    .session
                    .rearm_lead_oversight_after_team_enable()
                    .await;
            }
        }
        snapshots
    }

    async fn collect_activity_snapshots(&self) -> Vec<ThreadActivityUpdatedEvent> {
        let thread_ids = self.loaded_root_tree_ids().await;
        let Ok(state) = self.upgrade() else {
            return Vec::new();
        };
        let mut snapshots = Vec::with_capacity(thread_ids.len());
        for thread_id in thread_ids {
            let Ok(thread) = state.get_thread(thread_id).await else {
                continue;
            };
            snapshots.push(thread.session.activity_state().await);
        }
        snapshots
    }

    async fn collect_activity_snapshots_with_parents(
        &self,
    ) -> CodexResult<Vec<(ThreadId, Option<ThreadId>, ThreadActivity)>> {
        let thread_ids = self.loaded_root_tree_ids_strict().await?;
        let state = self.upgrade()?;
        let mut snapshots = Vec::with_capacity(thread_ids.len());
        for thread_id in thread_ids {
            let thread = state.get_thread(thread_id).await?;
            let activity = thread.session.activity_state().await;
            snapshots.push((
                activity.thread_id,
                thread.session_source().parent_thread_id(),
                activity.activity,
            ));
        }
        Ok(snapshots)
    }

    async fn loaded_root_tree_ids(&self) -> Vec<ThreadId> {
        let root_thread_id = ThreadId::from(self.session_id);
        let mut thread_ids = vec![root_thread_id];
        if let Ok(descendant_ids) = self.live_thread_spawn_descendants(root_thread_id).await {
            thread_ids.extend(descendant_ids);
        }
        thread_ids
    }

    async fn loaded_root_tree_ids_strict(&self) -> CodexResult<Vec<ThreadId>> {
        let root_thread_id = ThreadId::from(self.session_id);
        let descendant_ids = self.live_thread_spawn_descendants(root_thread_id).await?;
        let mut thread_ids = Vec::with_capacity(descendant_ids.len() + 1);
        thread_ids.push(root_thread_id);
        thread_ids.extend(descendant_ids);
        Ok(thread_ids)
    }
}
