//! Coalesces successful Worker completion reports for manager-only Team Leads.

use super::session::Session;
use crate::agent::control::HandoffAdmissionGuard;
use codex_config::TeamLeadWorkPolicy;
use std::sync::Arc;
use std::time::Duration;

const MANAGER_COMPLETION_QUIET_WINDOW: Duration = Duration::from_millis(150);
const MANAGER_COMPLETION_WAKE: &str =
    "The direct Workers have finished. Review their bounded completion summaries.";

impl Session {
    /// Schedules one short quiet-window wake after the direct Worker completion batch is idle.
    /// Actionable input drains the shared progress buffer first and makes this callback a no-op.
    pub(crate) async fn schedule_manager_completion_batch_flush(
        self: &Arc<Self>,
        generation: u64,
    ) {
        let session = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(MANAGER_COMPLETION_QUIET_WINDOW).await;
            let Some(session) = session.upgrade() else {
                return;
            };
            session
                .flush_manager_completion_batch(generation, /*handoff_admission*/ None)
                .await;
        });
    }

    /// Releases a pending completion batch once its policy permits a Lead wake.
    /// Prompt-guided policy retains the legacy per-completion wake behavior, so a batch buffered
    /// under manager-only policy can be presented immediately after a live switch.
    pub(crate) async fn flush_manager_completion_batch(
        self: &Arc<Self>,
        generation: u64,
        handoff_admission: Option<&HandoffAdmissionGuard>,
    ) {
        // A settings dispatch already holds admission through its caller. Reacquiring here can
        // deadlock if a handoff seals after dispatch began: the flush waits for admission to open
        // while the handoff waits for this dispatch to finish.
        if let Some(handoff_admission) = handoff_admission {
            self.flush_manager_completion_batch_under_admission(generation, handoff_admission)
                .await;
        } else {
            let _handoff_admission = loop {
                match self.services.agent_control.begin_handoff_admission() {
                    Ok(admission) => break admission,
                    Err(_) => {
                        self.services
                            .agent_control
                            .wait_for_handoff_admission_open()
                            .await;
                    }
                }
            };
            self.flush_manager_completion_batch_under_admission(generation, &_handoff_admission)
                .await;
        }
    }

    async fn flush_manager_completion_batch_under_admission(
        self: &Arc<Self>,
        generation: u64,
        _handoff_admission: &HandoffAdmissionGuard,
    ) {
        let team_lead_turn_admission = self.team_lead_turn_admission.lock().await;
        let config = self.get_config().await;
        let manager_only =
            config.effective_team_lead_work_policy() == TeamLeadWorkPolicy::ManagerOnly;
        if !self.is_team_lead().await
            || self.shutdown_requested()
            || self.is_interrupted()
            || (manager_only
                && self
                    .services
                    .agent_control
                    .active_direct_worker_count(self.thread_id)
                    .await
                    != 0)
            || !self
                .input_queue
                .manager_completion_batch_is_pending(generation)
                .await
        {
            return;
        }

        // Retain the trigger through an activity pause; the ordinary scheduler holds it until
        // `/continue` releases the root tree.
        self.enqueue_lead_wakeup_under_team_lead_admission(MANAGER_COMPLETION_WAKE)
            .await;
        drop(team_lead_turn_admission);
        drop(_handoff_admission);
        self.maybe_start_turn_for_pending_work().await;
    }
}
