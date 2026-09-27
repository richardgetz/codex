use super::LocalAgentControl;
use codex_protocol::ThreadId;
use codex_rollout::StateDbHandle;
use codex_state::TaskEstimate;
use std::time::Duration;
use tokio::sync::OwnedMutexGuard;

impl LocalAgentControl {
    /// Arm one-shot ETA reminders for changed tasks in the shared agent tree.
    pub(crate) async fn schedule_eta_reminders(
        &self,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        tasks: &[TaskEstimate],
        freshness_minimum: Duration,
    ) {
        if tasks.is_empty() {
            return;
        }
        self.eta_reminders
            .schedule(
                self.clone(),
                state_db,
                root_thread_id,
                tasks,
                freshness_minimum,
            )
            .await;
    }

    pub(crate) async fn lock_eta_reminders(&self) -> OwnedMutexGuard<()> {
        self.eta_reminders.lock_dispatch().await
    }

    pub(crate) async fn schedule_eta_reminders_locked(
        &self,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        tasks: &[TaskEstimate],
        freshness_minimum: Duration,
    ) {
        if tasks.is_empty() {
            return;
        }
        self.eta_reminders
            .schedule_locked(
                self.clone(),
                state_db,
                root_thread_id,
                tasks,
                freshness_minimum,
            )
            .await;
    }

    #[cfg(test)]
    pub(crate) async fn eta_reminder_state_for_tests(
        &self,
        task_id: &str,
    ) -> Option<(bool, bool, bool, bool)> {
        self.eta_reminders.state_for_tests(task_id).await
    }

    pub(crate) async fn reconfigure_eta_reminders_locked(
        &self,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        freshness_minimum: Duration,
        _eta_dispatch: &OwnedMutexGuard<()>,
    ) {
        self.eta_reminders
            .reconfigure_locked(self.clone(), state_db, root_thread_id, freshness_minimum)
            .await;
    }

    pub(crate) async fn cancel_eta_reminders(&self) {
        self.eta_reminders.cancel_all().await;
    }

    pub(crate) async fn suspend_eta_reminders(&self) {
        self.eta_reminders.suspend_all().await;
    }

    pub(crate) async fn cancel_eta_reminders_locked(&self, _eta_dispatch: &OwnedMutexGuard<()>) {
        self.eta_reminders.cancel_all_locked().await;
    }

    pub(crate) async fn cancel_eta_reminders_for_owner(&self, owner_thread_id: ThreadId) {
        self.eta_reminders.cancel_owner(owner_thread_id).await;
    }

    pub(crate) async fn suspend_eta_reminders_for_owner(&self, owner_thread_id: ThreadId) {
        self.eta_reminders.suspend_owner(owner_thread_id).await;
    }

    pub(crate) async fn cancel_eta_reminders_for_owner_locked(
        &self,
        owner_thread_id: ThreadId,
        _eta_dispatch: &OwnedMutexGuard<()>,
    ) {
        self.eta_reminders
            .cancel_owner_locked(owner_thread_id)
            .await;
    }
}
