use super::{HandoffCoordinator, core_error, ordered_indices, receipt_from_journal};
use crate::error_code::invalid_params;
use codex_app_server_protocol::{
    JSONRPCErrorError, ThreadHandoffRecoverParams, ThreadHandoffRecoverResponse,
};
use codex_core::{
    CodexThread, HandoffBlocker, HandoffJournal, HandoffJournalState, HandoffNode,
    HandoffNodeState, RecoverTurnRequest, StartIfIdleSubmission,
};
use codex_protocol::ThreadId;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_rollout::InitialHistory;
use std::path::PathBuf;
use std::sync::Arc;

struct LoadedRecoveryNode {
    index: usize,
    thread: Arc<CodexThread>,
}

impl HandoffCoordinator {
    pub(crate) async fn recover(
        &self,
        params: ThreadHandoffRecoverParams,
    ) -> Result<ThreadHandoffRecoverResponse, JSONRPCErrorError> {
        let _operation = self.operation.lock().await;
        let journal = self.load_journal(&params.handoff_id).await?;
        if self.active.lock().await.contains_key(&journal.handoff_id) {
            return Err(invalid_params(
                "handoff recovery must run in the replacement app-server while the old runtime remains fenced",
            ));
        }
        if journal.state == HandoffJournalState::Completed {
            self.refresh_startup_recovery_state().await;
            return Ok(ThreadHandoffRecoverResponse {
                receipt: receipt_from_journal(&journal),
            });
        }

        let mut journal = journal;
        if matches!(
            journal.state,
            HandoffJournalState::Prepared | HandoffJournalState::Draining
        ) {
            for node in &mut journal.nodes {
                if !matches!(
                    node.state,
                    HandoffNodeState::Restored | HandoffNodeState::Paused
                ) {
                    node.state = HandoffNodeState::NeedsAttention;
                    if node.blockers.is_empty() {
                        node.blockers.push(HandoffBlocker::TaskExitedUnexpectedly);
                    }
                }
            }
            journal.set_state(HandoffJournalState::NeedsAttention);
            self.persist_journal(&journal).await?;
            self.refresh_startup_recovery_state().await;
            return Ok(ThreadHandoffRecoverResponse {
                receipt: receipt_from_journal(&journal),
            });
        }

        // Keep replacement sessions' durable inbound pollers fenced until every node has been
        // loaded, its process-local pause restored, and its exact saved turn admitted. The Core
        // guard is manager-wide so sessions created below share one recovery boundary; dropping
        // it on any failure leaves the replacement fail-closed for an explicit retry.
        let recovery_pending = self
            .thread_manager
            .begin_recovery_pending()
            .map_err(core_error)?;
        // Drain any poller claim that crossed the gate before creating replacement sessions; all
        // subsequent pollers observe the pending bit and remain idle until recovery completes.
        recovery_pending.wait_for_admissions().await;
        journal.set_state(HandoffJournalState::Restoring);
        self.persist_journal(&journal).await?;

        // Load every recoverable node first. Parent-first ordering makes the V2 child loader
        // available for every child before any exact turn is admitted.
        let (loaded_nodes, all_loaded) = self.load_all_recovery_nodes(&mut journal).await?;
        if !all_loaded {
            journal.set_state(HandoffJournalState::NeedsAttention);
            self.persist_journal(&journal).await?;
            self.refresh_startup_recovery_state().await;
            return Ok(ThreadHandoffRecoverResponse {
                receipt: receipt_from_journal(&journal),
            });
        }

        // Restore process-local pause state for the complete graph before starting a turn.
        if !self
            .restore_pause_state(&mut journal, &loaded_nodes)
            .await?
        {
            journal.set_state(HandoffJournalState::NeedsAttention);
            self.persist_journal(&journal).await?;
            self.refresh_startup_recovery_state().await;
            return Ok(ThreadHandoffRecoverResponse {
                receipt: receipt_from_journal(&journal),
            });
        }

        // Only after the graph is loaded and pauses are restored may exact turn IDs be admitted.
        self.recover_loaded_nodes(&mut journal, &loaded_nodes)
            .await?;

        let complete = journal.nodes.iter().all(|node| {
            matches!(
                node.state,
                HandoffNodeState::Restored | HandoffNodeState::Paused
            )
        });
        journal.set_state(if complete {
            HandoffJournalState::Completed
        } else {
            HandoffJournalState::NeedsAttention
        });
        self.persist_journal(&journal).await?;
        if complete {
            recovery_pending.complete();
        }
        self.refresh_startup_recovery_state().await;
        Ok(ThreadHandoffRecoverResponse {
            receipt: receipt_from_journal(&journal),
        })
    }

    async fn load_all_recovery_nodes(
        &self,
        journal: &mut HandoffJournal,
    ) -> Result<(Vec<LoadedRecoveryNode>, bool), JSONRPCErrorError> {
        let mut loaded_nodes = Vec::new();
        let mut all_loaded = true;
        for index in ordered_indices(&journal.nodes, false) {
            let node = journal.nodes[index].clone();
            if node.state == HandoffNodeState::NeedsAttention || !node.blockers.is_empty() {
                all_loaded = false;
                if node.state != HandoffNodeState::NeedsAttention {
                    journal.update_node(
                        &node.thread_id,
                        HandoffNodeState::NeedsAttention,
                        node.blockers,
                        None,
                    );
                }
                continue;
            }
            match self.load_recovery_node(&node).await {
                Ok(thread) => loaded_nodes.push(LoadedRecoveryNode { index, thread }),
                Err(blocker) => {
                    all_loaded = false;
                    journal.update_node(
                        &node.thread_id,
                        HandoffNodeState::NeedsAttention,
                        vec![blocker],
                        None,
                    );
                }
            }
        }
        Ok((loaded_nodes, all_loaded))
    }

    async fn restore_pause_state(
        &self,
        journal: &mut HandoffJournal,
        loaded_nodes: &[LoadedRecoveryNode],
    ) -> Result<bool, JSONRPCErrorError> {
        let mut all_pauses_restored = true;
        for loaded in loaded_nodes {
            let node = journal.nodes[loaded.index].clone();
            if node.was_paused && loaded.thread.submit(Op::PauseActivity).await.is_err() {
                all_pauses_restored = false;
                journal.update_node(
                    &node.thread_id,
                    HandoffNodeState::NeedsAttention,
                    vec![HandoffBlocker::Persistence],
                    None,
                );
                continue;
            }
            if node.turn_id.is_none() {
                let state = if node.was_paused {
                    HandoffNodeState::Paused
                } else {
                    HandoffNodeState::Restored
                };
                journal.update_node(&node.thread_id, state, Vec::new(), None);
                journal.clear_node_turn_id(&node.thread_id);
            } else if node.was_paused
                && matches!(node.state, HandoffNodeState::Restored | HandoffNodeState::Paused)
            {
                journal.update_node(
                    &node.thread_id,
                    HandoffNodeState::Paused,
                    Vec::new(),
                    None,
                );
            }
        }
        if !all_pauses_restored {
            journal.set_state(HandoffJournalState::NeedsAttention);
        }
        self.persist_journal(journal).await?;
        Ok(all_pauses_restored)
    }

    async fn recover_loaded_nodes(
        &self,
        journal: &mut HandoffJournal,
        loaded_nodes: &[LoadedRecoveryNode],
    ) -> Result<(), JSONRPCErrorError> {
        for loaded in loaded_nodes {
            let node = journal.nodes[loaded.index].clone();
            let Some(expected_turn_id) = node.turn_id.clone() else {
                continue;
            };
            if !node.blockers.is_empty() || node.state == HandoffNodeState::NeedsAttention {
                continue;
            }

            if matches!(
                node.state,
                HandoffNodeState::Recovering
                    | HandoffNodeState::Restored
                    | HandoffNodeState::Paused
            ) {
                // A non-Suspended receipt has already crossed the recovery boundary. If the
                // replacement restarted while that turn was idle, the rollout alone cannot prove
                // whether it completed before the crash, so never submit the exact turn again.
                let preflight = loaded.thread.handoff_preflight().await;
                let mut blockers = preflight.blockers;
                blockers.retain(|blocker| !matches!(blocker, HandoffBlocker::LiveDescendants));
                if blockers.is_empty() && preflight.was_running {
                    if preflight.turn_id.as_deref() == Some(expected_turn_id.as_str()) {
                        journal.update_node(
                            &node.thread_id,
                            if node.was_paused {
                                HandoffNodeState::Paused
                            } else {
                                HandoffNodeState::Restored
                            },
                            Vec::new(),
                            None,
                        );
                    } else {
                        journal.update_node(
                            &node.thread_id,
                            HandoffNodeState::NeedsAttention,
                            vec![HandoffBlocker::TaskExitedUnexpectedly],
                            None,
                        );
                        journal.set_state(HandoffJournalState::NeedsAttention);
                    }
                } else {
                    if blockers.is_empty() {
                        blockers.push(HandoffBlocker::TaskExitedUnexpectedly);
                    }
                    journal.update_node(
                        &node.thread_id,
                        HandoffNodeState::NeedsAttention,
                        blockers,
                        None,
                    );
                    journal.set_state(HandoffJournalState::NeedsAttention);
                }
                self.persist_journal(journal).await?;
                continue;
            }

            if node.state != HandoffNodeState::Suspended {
                continue;
            }
            journal.update_node(
                &node.thread_id,
                HandoffNodeState::Recovering,
                Vec::new(),
                None,
            );
            self.persist_journal(journal).await?;
            let submission = loaded
                .thread
                .recover_turn_if_idle(RecoverTurnRequest {
                    turn_id: expected_turn_id.clone(),
                    thread_settings: Default::default(),
                    trace: None,
                    cyber_access_program: None,
                })
                .await;
            match submission {
                Ok(StartIfIdleSubmission::Started { turn_id }) if turn_id == expected_turn_id => {
                    journal.update_node(
                        &node.thread_id,
                        if node.was_paused {
                            HandoffNodeState::Paused
                        } else {
                            HandoffNodeState::Restored
                        },
                        Vec::new(),
                        None,
                    );
                }
                Ok(StartIfIdleSubmission::Started { .. })
                | Ok(StartIfIdleSubmission::NotSubmitted { .. })
                | Err(_) => {
                    journal.update_node(
                        &node.thread_id,
                        HandoffNodeState::NeedsAttention,
                        vec![HandoffBlocker::TaskExitedUnexpectedly],
                        None,
                    );
                    journal.set_state(HandoffJournalState::NeedsAttention);
                }
            }
            self.persist_journal(journal).await?;
        }
        Ok(())
    }

    async fn load_recovery_node(
        &self,
        node: &HandoffNode,
    ) -> Result<Arc<CodexThread>, HandoffBlocker> {
        let thread_id =
            ThreadId::from_string(&node.thread_id).map_err(|_| HandoffBlocker::Persistence)?;
        let loaded_thread = self.thread_manager.get_thread(thread_id).await.ok();
        let thread_is_loaded = loaded_thread.is_some();
        if thread_is_loaded && !matches!(node.state, HandoffNodeState::Suspended) {
            return loaded_thread.ok_or(HandoffBlocker::Persistence);
        }
        let multi_agent_version = if let Some(version) =
            loaded_thread.as_ref().and_then(|thread| thread.multi_agent_version())
        {
            Some(version)
        } else {
            let rollout_path = node
                .rollout_path
                .as_deref()
                .filter(|path| !path.is_empty())
                .ok_or(HandoffBlocker::Persistence)?;
            let initial_history = codex_core::RolloutRecorder::get_rollout_history_with_options(
                &PathBuf::from(rollout_path),
                self.config.resume_load_options(),
            )
            .await
            .map_err(|_| HandoffBlocker::Persistence)?;
            initial_history.get_multi_agent_version().or_else(|| {
                matches!(
                    &initial_history,
                    InitialHistory::Resumed(_) | InitialHistory::Forked(_)
                )
                .then_some(MultiAgentVersion::V1)
            })
        };
        // A partial old-runtime attempt can leave a closed Suspended thread in the manager map.
        // Remove that stale handle so the normal rollout/parent loader creates a live owner.
        if thread_is_loaded {
            self.thread_manager.remove_thread(&thread_id).await;
        }
        if node.parent_thread_id.is_some() && multi_agent_version.is_none() {
            // A parent-linked node without persisted version metadata cannot be safely routed:
            // guessing V1 or V2 could detach it from its parent control state.
            return Err(HandoffBlocker::ParentUnavailable);
        }
        if node.parent_thread_id.is_some() && multi_agent_version == Some(MultiAgentVersion::V2) {
            self.thread_manager
                .ensure_multi_agent_v2_child_loaded(thread_id)
                .await
                .map_err(|_| HandoffBlocker::ParentUnavailable)?;
        } else {
            let rollout_path = node
                .rollout_path
                .as_deref()
                .filter(|path| !path.is_empty())
                .ok_or(HandoffBlocker::Persistence)?;
            self.thread_manager
                .resume_thread_from_rollout(
                    self.config.as_ref().clone(),
                    PathBuf::from(rollout_path),
                    self.thread_manager.auth_manager(),
                    None,
                    ClientMcpExtensions::default(),
                )
                .await
                .map_err(|_| HandoffBlocker::Persistence)?;
        }
        self.thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| HandoffBlocker::Persistence)
    }
}
