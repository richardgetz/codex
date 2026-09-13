use super::session::Session;
use codex_protocol::protocol::TeamMode;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

impl Session {
    /// Queues one bounded, actionable handoff when a Team Worker has no dependency left that this
    /// wait path can observe. The caller supplies target-specific dependency and activity checks;
    /// this method also waits for pending non-wait dispatches and checks direct children and the
    /// current mailbox to cover races between the wait snapshot and the handoff.
    pub(crate) async fn maybe_handoff_dependency_free_wait(
        &self,
        sub_id: &str,
        has_pending_activity: bool,
        cancellation_token: &CancellationToken,
    ) -> bool {
        if has_pending_activity {
            return false;
        }
        if self
            .wait_for_activity_quiescence(cancellation_token)
            .await
            .is_err()
        {
            return false;
        }
        if self.usage_resume_waiting.load(Ordering::Acquire)
            || self.input_queue.has_pending_mailbox_items().await
        {
            return false;
        }

        let Some(turn_state) = self
            .input_queue
            .turn_state_for_sub_id(&self.active_turn, sub_id)
            .await
        else {
            return false;
        };
        {
            let turn_state = turn_state.lock().await;
            if turn_state.has_pending_approval()
                || turn_state.has_pending_user_input()
                || turn_state.has_pending_input()
            {
                return false;
            }
        }

        let config = self.get_config().await;
        if config.team_mode != TeamMode::LeadWorker {
            return false;
        }
        let session_source = self.session_source().await;
        if crate::session::team::effective_role_for_session_source(&config, &session_source)
            != Some(codex_config::TeamRole::Worker)
        {
            return false;
        }
        if session_source.parent_thread_id().is_none() {
            return false;
        }
        if self
            .services
            .agent_control
            .active_direct_worker_count(self.thread_id)
            .await
            > 0
        {
            return false;
        }
        if self
            .wait_for_activity_quiescence(cancellation_token)
            .await
            .is_err()
        {
            return false;
        }
        if self.activity_in_flight.load(Ordering::Acquire) != 0
            || self.pending_handoff_dispatches() != 0
        {
            return false;
        }
        if self.usage_resume_waiting.load(Ordering::Acquire)
            || self
                .services
                .agent_control
                .active_direct_worker_count(self.thread_id)
                .await
                > 0
            || self.input_queue.has_pending_mailbox_items().await
        {
            return false;
        }
        {
            let turn_state = turn_state.lock().await;
            if turn_state.has_pending_approval()
                || turn_state.has_pending_user_input()
                || turn_state.has_pending_input()
            {
                return false;
            }
        }
        if cancellation_token.is_cancelled() {
            return false;
        }

        // Keep the durable claim and parent enqueue inside one handoff admission. A coordinator
        // waits for this short operation before closing the old runtime, so a sealed handoff cannot
        // strand the one-shot latch in a process-local task.
        let Ok(handoff_admission) = self.services.agent_control.begin_handoff_admission() else {
            return false;
        };
        if !self
            .input_queue
            .claim_dependency_free_wait_handoff(sub_id)
            .await
        {
            return false;
        }
        let agent_control = self.services.agent_control.clone();
        let child_thread_id = self.thread_id;
        tokio::spawn(async move {
            if let Err(err) = agent_control
                .notify_parent_of_dependency_free_wait(child_thread_id, &session_source, handoff_admission)
                .await
            {
                tracing::warn!(
                    thread_id = %child_thread_id,
                    error = %err,
                    "failed to queue dependency-free Worker wait handoff"
                );
            }
        });
        true
    }
}
