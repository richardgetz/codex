use super::*;
use crate::agent::status::is_final;
use crate::agent::api::AgentInfo;
use crate::context::SubagentNotification;
use crate::session_prefix::format_inter_agent_completion_message;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_thread_store::PersistContext;
use futures::StreamExt;
use std::sync::Arc;

impl LocalAgentControl {
    /// Starts a detached watcher for sub-agents spawned from another thread.
    ///
    /// Legacy parents receive a context fragment. If a legacy-created child is running V2,
    /// its path-aware result uses the same message route as a V2 child created directly.
    pub(super) async fn maybe_start_completion_watcher(
        &self,
        child_thread_id: ThreadId,
        session_source: Option<SessionSource>,
        child_reference: String,
        child_agent_path: Option<codex_protocol::AgentPath>,
        terminal_delivery_guard: Option<TerminalResultDeliveryGuard>,
    ) {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return;
        };

        // Capture the parent's role before the detached task can lose its live session to
        // residency eviction. Durable fallback uses this hint if the parent is unloaded.
        let parent_team_lead_hint = self.parent_is_team_lead(parent_thread_id).await;
        let Some(watcher_registration) = self.begin_handoff_completion_watcher() else {
            if self.handoff_admission_sealed() {
                self.mark_handoff_delivery_failed();
            }
            return;
        };
        let control = self.clone();
        tokio::spawn(async move {
            let _watcher_registration = watcher_registration;
            let _terminal_delivery_guard = terminal_delivery_guard;
            let status = match control.subscribe_status(child_thread_id).await {
                Ok(mut updates) => {
                    let mut final_status = None;
                    while let Some(Ok(snapshot)) = updates.next().await {
                        if let Some(status) = snapshot.status()
                            && is_final(status)
                        {
                            final_status = Some(status.clone());
                            break;
                        }
                    }
                    match final_status {
                        Some(status) => status,
                        None => control.get_status(child_thread_id).await,
                    }
                }
                Err(_) => control.get_status(child_thread_id).await,
            };
            if !is_final(&status) {
                return;
            }

            if control.take_handoff_suspended(child_thread_id)
                && matches!(status, AgentStatus::Shutdown)
            {
                return;
            }

            let _handoff_delivery = control.begin_handoff_terminal_delivery();
            let Ok(state) = control.upgrade() else {
                if control.handoff_admission_sealed() {
                    control.mark_handoff_delivery_failed();
                }
                return;
            };
            let state_db = state.state_db().await;
            let child_thread = state.get_thread(child_thread_id).await.ok();
            let child_uses_multi_agent_v2 = child_thread
                .as_ref()
                .is_none_or(|thread| thread.multi_agent_version() == Some(MultiAgentVersion::V2));
            if child_agent_path.is_some() && child_uses_multi_agent_v2 {
                let Some(child_agent_path) = child_agent_path.clone() else {
                    return;
                };
                let Some(parent_agent_path) = child_agent_path
                    .as_str()
                    .rsplit_once('/')
                    .and_then(|(parent, _)| codex_protocol::AgentPath::try_from(parent).ok())
                else {
                    return;
                };
                let Some(message) = format_inter_agent_completion_message(
                    parent_agent_path.clone(),
                    child_agent_path.clone(),
                    &status,
                ) else {
                    return;
                };
                let trigger_turn = match state.get_thread(parent_thread_id).await {
                    Ok(parent_thread) => parent_thread.session.is_team_lead().await,
                    Err(_) => parent_team_lead_hint,
                };
                let communication = InterAgentCommunication::new(
                    child_agent_path,
                    parent_agent_path,
                    Vec::new(),
                    message,
                    trigger_turn,
                );
                let context = AgentCommunicationContext::new(
                    AgentCommunicationKind::Result,
                    child_thread_id,
                );
                let _ = if trigger_turn {
                    control
                        .send_team_lead_completion(
                            parent_thread_id,
                            communication,
                            context,
                            TurnStartOptions::default(),
                            &status,
                        )
                        .await
                } else {
                    control
                        .send_inter_agent_communication(
                            parent_thread_id,
                            communication,
                            context,
                            TurnStartOptions::default(),
                        )
                        .await
                };
                return;
            }

            let parent_thread = match state.get_thread(parent_thread_id).await {
                Ok(parent_thread) => parent_thread,
                Err(error) => {
                    if let Err(persist_error) = Self::persist_legacy_completion_to_state_db(
                        state_db.as_ref(),
                        parent_thread_id,
                        child_thread_id,
                        &child_reference,
                        child_agent_path.as_ref(),
                        &status,
                        parent_team_lead_hint,
                    )
                    .await
                    {
                        if control.handoff_admission_sealed() {
                            control.mark_handoff_delivery_failed();
                        }
                        tracing::warn!(
                            parent_thread_id = %parent_thread_id,
                            child_thread_id = %child_thread_id,
                            %error,
                            %persist_error,
                            "unable to retain legacy completion for an unloaded parent"
                        );
                    }
                    return;
                }
            };
            if parent_thread.session.is_team_lead().await {
                if matches!(status, AgentStatus::Completed(_))
                    && parent_thread
                        .session
                        .get_config()
                        .await
                        .effective_team_lead_work_policy()
                        == TeamLeadWorkPolicy::ManagerOnly
                    && let Some(child_agent_path) = child_agent_path.clone()
                    && let Some(parent_agent_path) = child_agent_path
                        .as_str()
                        .rsplit_once('/')
                        .and_then(|(parent, _)| codex_protocol::AgentPath::try_from(parent).ok())
                    && let Some(message) = format_inter_agent_completion_message(
                        parent_agent_path.clone(),
                        child_agent_path.clone(),
                        &status,
                    )
                {
                    let communication = InterAgentCommunication::new(
                        child_agent_path,
                        parent_agent_path,
                        Vec::new(),
                        message,
                        /*trigger_turn*/ true,
                    );
                    let context =
                        AgentCommunicationContext::new(AgentCommunicationKind::Result, child_thread_id);
                    let _ = control
                        .send_team_lead_completion(
                            parent_thread_id,
                            communication,
                            context,
                            TurnStartOptions::default(),
                            &status,
                        )
                        .await;
                    return;
                }

                let Ok(handoff_admission) = parent_thread
                    .session
                    .services
                    .agent_control
                    .begin_handoff_admission()
                else {
                    Self::retain_legacy_completion_after_handoff(
                        &parent_thread,
                        child_thread_id,
                        &child_reference,
                        child_agent_path.as_ref(),
                        &status,
                        /*trigger_turn*/ true,
                    )
                    .await;
                    return;
                };
                parent_thread
                    .inject_fragment_without_turn(
                        SubagentNotification::new(child_reference.as_str(), status.clone()),
                        &handoff_admission,
                    )
                    .await;
                parent_thread.session.cancel_lead_oversight().await;
                parent_thread
                    .session
                    .enqueue_lead_wakeup_with_admission(&format!(
                        "Worker {child_reference} completed with status {status:?}; review the result."
                    ))
                    .await;
                drop(handoff_admission);
                parent_thread
                    .session
                    .maybe_start_turn_for_pending_work()
                    .await;
                return;
            }

            let Ok(handoff_admission) = parent_thread
                .session
                .services
                .agent_control
                .begin_handoff_admission()
            else {
                Self::retain_legacy_completion_after_handoff(
                    &parent_thread,
                    child_thread_id,
                    &child_reference,
                    child_agent_path.as_ref(),
                    &status,
                    /*trigger_turn*/ false,
                )
                .await;
                return;
            };
            parent_thread
                .inject_fragment_without_turn(
                    SubagentNotification::new(child_reference.as_str(), status),
                    &handoff_admission,
                )
                .await;
        });
    }

    async fn persist_legacy_completion_to_state_db(
        state_db: Option<&codex_rollout::state_db::StateDbHandle>,
        target_thread_id: ThreadId,
        child_thread_id: ThreadId,
        child_reference: &str,
        child_agent_path: Option<&codex_protocol::AgentPath>,
        status: &AgentStatus,
        trigger_turn: bool,
    ) -> anyhow::Result<String> {
        let Some(state_db) = state_db else {
            anyhow::bail!("state database unavailable for legacy completion");
        };
        let author = child_agent_path
            .cloned()
            .or_else(|| codex_protocol::AgentPath::try_from(child_reference).ok())
            .unwrap_or_else(codex_protocol::AgentPath::root);
        let recipient = author
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| codex_protocol::AgentPath::try_from(parent).ok())
            .unwrap_or_else(codex_protocol::AgentPath::root);
        let communication = InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            format!(
                "Worker {child_reference} completed with status {status:?}; review the result."
            ),
            trigger_turn,
        );
        crate::session::persist_handoff_inter_agent_communication(
            state_db,
            target_thread_id,
            Some(child_thread_id),
            &communication,
            &TurnStartOptions::default(),
            trigger_turn,
        )
        .await
    }

    async fn retain_legacy_completion_after_handoff(
        parent_thread: &Arc<crate::CodexThread>,
        child_thread_id: ThreadId,
        child_reference: &str,
        child_agent_path: Option<&codex_protocol::AgentPath>,
        status: &AgentStatus,
        trigger_turn: bool,
    ) {
        let author = child_agent_path
            .cloned()
            .or_else(|| codex_protocol::AgentPath::try_from(child_reference).ok())
            .unwrap_or_else(codex_protocol::AgentPath::root);
        let recipient = author
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| codex_protocol::AgentPath::try_from(parent).ok())
            .unwrap_or_else(codex_protocol::AgentPath::root);
        let communication = InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            format!(
                "Worker {child_reference} completed with status {status:?}; review the result."
            ),
            trigger_turn,
        );
        let start_options = TurnStartOptions::default();
        if let Some(state_db) = parent_thread.session.state_db() {
            match crate::session::persist_handoff_inter_agent_communication(
                &state_db,
                parent_thread.session.thread_id(),
                Some(child_thread_id),
                &communication,
                &start_options,
                trigger_turn,
            )
            .await
            {
                Ok(message_id) => {
                    tracing::info!(
                        parent_thread_id = %parent_thread.session.thread_id(),
                        child_thread_id = %child_thread_id,
                        %message_id,
                        "persisted legacy completion after handoff admission was sealed"
                    );
                    return;
                }
                Err(error) => {
                    tracing::warn!(
                        parent_thread_id = %parent_thread.session.thread_id(),
                        child_thread_id = %child_thread_id,
                        %error,
                        "failed to persist legacy completion after handoff admission was sealed"
                    );
                }
            }
        } else {
            tracing::warn!(
                parent_thread_id = %parent_thread.session.thread_id(),
                child_thread_id = %child_thread_id,
                "state database unavailable for legacy completion handoff fallback"
            );
        }
        parent_thread
            .session
            .services
            .agent_control
            .mark_handoff_delivery_failed();
        if trigger_turn {
            parent_thread
                .session
                .input_queue
                .enqueue_team_lead_mailbox_communication(communication, start_options)
                .await;
        } else {
            parent_thread
                .session
                .input_queue
                .enqueue_mailbox_communication(communication, start_options)
                .await;
        }
    }

    /// Submit a shutdown request for a live agent without marking it explicitly closed in
    /// persisted spawn-edge state.
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let result = if let Ok(thread) = state.get_thread(agent_id).await {
            thread
                .session
                .ensure_rollout_materialized(PersistContext::Standard)
                .await;
            thread.session.flush_rollout().await?;
            let result = if matches!(thread.agent_status().await, AgentStatus::Shutdown) {
                Ok(String::new())
            } else {
                state
                    .send_op(
                        agent_id,
                        Op::Shutdown {},
                        /*parent_turn_id*/ None,
                        /*root_turn_id*/ None,
                    )
                    .await
            };
            thread.wait_until_terminated().await;
            result
        } else {
            state
                .send_op(
                    agent_id,
                    Op::Shutdown {},
                    /*parent_turn_id*/ None,
                    /*root_turn_id*/ None,
                )
                .await
        };
        let _ = state.remove_thread(&agent_id).await;
        self.forget_v2_residency(agent_id);
        self.runtime.registry.release_spawned_thread(agent_id);
        result
    }

    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down the
    /// agent and any live descendants reached from the in-memory tree.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<AgentInfo> {
        let eta_dispatch = self.lock_eta_reminders().await;
        let state = self.upgrade()?;
        let metadata = self.get_agent_metadata(agent_id);
        let known_agent = metadata.is_some();
        let snapshot = match state.get_thread(agent_id).await {
            Ok(thread) => {
                let agent = LiveAgent {
                    thread_id: agent_id,
                    metadata: metadata.unwrap_or_default(),
                    status: thread.agent_status().await,
                };
                let config = Box::new(thread.config_snapshot().await);
                if !config.ephemeral
                    && let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    warn!("failed to persist thread-spawn edge status for {agent_id}: {err}");
                }
                self.cancel_eta_reminders_for_owner_locked(agent_id, &eta_dispatch)
                    .await;
                AgentInfo::Loaded { agent, config }
            }
            Err(err)
                if known_agent && matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) =>
            {
                if let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    return Err(CodexErr::Fatal(format!(
                        "failed to persist stale thread-spawn edge status for {agent_id}: {err}"
                    )));
                }
                self.cancel_eta_reminders_for_owner_locked(agent_id, &eta_dispatch)
                    .await;
                AgentInfo::Unloaded(metadata.unwrap_or_default())
            }
            Err(err) => return Err(err),
        };
        drop(eta_dispatch);
        match Box::pin(self.shutdown_agent_tree(agent_id)).await {
            Err(err)
                if known_agent
                    && matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) =>
            {
                Ok(snapshot)
            }
            result => result.map(|_| snapshot),
        }
    }

    /// Shut down `agent_id` and any live descendants reachable from the in-memory spawn tree.
    pub(crate) async fn shutdown_agent_tree(&self, agent_id: ThreadId) -> CodexResult<String> {
        let descendant_ids = self.live_thread_spawn_descendants(agent_id).await?;
        let result = self.shutdown_live_agent(agent_id).await;
        for descendant_id in descendant_ids {
            match self.shutdown_live_agent(descendant_id).await {
                Ok(_) => {}
                Err(err)
                    if matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) => {}
                Err(err) => return Err(err),
            }
        }
        result
    }
}
