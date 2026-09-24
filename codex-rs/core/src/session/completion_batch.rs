//! Coalesces successful Worker completion reports for manager-only Team Leads.

use super::session::Session;
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
            let _handoff_admission = loop {
                match session
                    .services
                    .agent_control
                    .begin_handoff_admission()
                {
                    Ok(admission) => break admission,
                    Err(_) => {
                        session
                            .services
                            .agent_control
                            .wait_for_handoff_admission_open()
                            .await;
                    }
                }
            };
            let team_lead_turn_admission = session.team_lead_turn_admission.lock().await;
            let config = session.get_config().await;
            if !session.is_team_lead().await
                || config.effective_team_lead_work_policy() != TeamLeadWorkPolicy::ManagerOnly
                || session.shutdown_requested()
                || session.is_interrupted()
                || session
                    .services
                    .agent_control
                    .active_direct_worker_count(session.thread_id)
                    .await
                    != 0
                || !session
                    .input_queue
                    .manager_completion_batch_is_pending(generation)
                    .await
            {
                return;
            }

            // Retain the trigger through an activity pause; the ordinary scheduler holds it until
            // `/continue` releases the root tree.
            session
                .enqueue_lead_wakeup_under_team_lead_admission(MANAGER_COMPLETION_WAKE)
                .await;
            drop(team_lead_turn_admission);
            drop(_handoff_admission);
            session.maybe_start_turn_for_pending_work().await;
        });
    }
}
