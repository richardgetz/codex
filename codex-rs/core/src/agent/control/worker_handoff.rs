//! Actionable handoff routing for Workers that have reached a dependency-free wait.

use super::AgentControl;
use crate::TurnStartOptions;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::session::truncate_message;
use codex_config::TeamRole;
use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TeamMode;

impl AgentControl {
    /// Queues an actionable handoff with the immediate parent when a Team Worker has reached a
    /// dependency-free wait. A root Lead uses its existing bounded wake path; nested Workers use
    /// the ordinary trigger-mail path addressed to their exact parent agent path.
    pub(crate) async fn notify_parent_of_dependency_free_wait(
        &self,
        child_thread_id: ThreadId,
        child_source: &SessionSource,
    ) -> CodexResult<()> {
        let Some(parent_thread_id) = child_source.parent_thread_id() else {
            return Ok(());
        };
        let state = self.upgrade()?;
        let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
            return Ok(());
        };
        let parent_config = parent_thread.session.get_config().await;
        if parent_config.team_mode != TeamMode::LeadWorker {
            return Ok(());
        }
        let parent_source = parent_thread.session.session_source().await;
        let parent_role =
            crate::session::team::effective_role_for_session_source(&parent_config, &parent_source);
        let message = dependency_free_wait_handoff_message(child_source);

        if parent_role == Some(TeamRole::Lead) {
            parent_thread.session.enqueue_lead_wakeup(&message).await;
            parent_thread
                .session
                .maybe_start_turn_for_pending_work()
                .await;
            return Ok(());
        }
        if parent_role != Some(TeamRole::Worker) {
            return Ok(());
        }

        let (Some(child_agent_path), Some(parent_agent_path)) = (
            child_source.get_agent_path(),
            parent_source.get_agent_path(),
        ) else {
            return Ok(());
        };
        let communication = InterAgentCommunication::new(
            child_agent_path,
            parent_agent_path,
            Vec::new(),
            message,
            true,
        );
        self.send_inter_agent_communication(
            parent_thread_id,
            communication,
            AgentCommunicationContext::new(AgentCommunicationKind::Message, child_thread_id),
            TurnStartOptions::default(),
        )
        .await
        .map(|_| ())
    }
}

fn dependency_free_wait_handoff_message(source: &SessionSource) -> String {
    let worker = source
        .get_agent_path()
        .map_or_else(|| "Worker".to_string(), |path| path.to_string());
    truncate_message(&format!(
        "Worker {worker} is waiting with no active child dependency; review its report or provide follow-up."
    ))
}
