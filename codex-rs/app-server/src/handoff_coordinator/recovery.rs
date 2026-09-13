use super::{HandoffCoordinator, core_error, ordered_indices, receipt_from_journal};
use crate::error_code::{internal_error, invalid_params};
use codex_app_server_protocol::{
    JSONRPCErrorError, ThreadHandoffRecoverParams, ThreadHandoffRecoverResponse,
};
use codex_core::{
    HandoffBlocker, HandoffJournal, HandoffJournalState, HandoffNode, HandoffNodeState,
    RecoverTurnRequest, StartIfIdleSubmission,
};
use codex_protocol::ThreadId;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::Op;
use std::path::PathBuf;

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
            return Ok(ThreadHandoffRecoverResponse {
                receipt: receipt_from_journal(&journal),
            });
        }

        journal.set_state(HandoffJournalState::Restoring);
        if let Err(error) = journal.persist(&self.codex_home).await {
            return Err(internal_error(format!(
                "could not persist restoring handoff: {error}"
            )));
        }

        for index in ordered_indices(&journal.nodes, false) {
            let node = journal.nodes[index].clone();
            if matches!(
                node.state,
                HandoffNodeState::Restored
                    | HandoffNodeState::Paused
                    | HandoffNodeState::NeedsAttention
            ) || !node.blockers.is_empty()
            {
                continue;
            }
            let has_recoverable_descendant = journal.nodes.iter().any(|candidate| {
                candidate.parent_thread_id.as_deref() == Some(node.thread_id.as_str())
                    && candidate.turn_id.is_some()
                    && !matches!(
                        candidate.state,
                        HandoffNodeState::Restored | HandoffNodeState::Paused
                    )
            });
            let needs_pause_restore = node.parent_thread_id.is_none() && node.was_paused;
            if node.turn_id.is_none() && !has_recoverable_descendant && !needs_pause_restore {
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
                journal.clear_node_turn_id(&node.thread_id);
                if let Err(error) = journal.persist(&self.codex_home).await {
                    return Err(internal_error(format!(
                        "could not persist idle handoff node {}: {error}",
                        node.thread_id
                    )));
                }
                continue;
            }
            if let Err(blocker) = self.load_recovery_node(&node).await {
                journal.update_node(
                    &node.thread_id,
                    HandoffNodeState::NeedsAttention,
                    vec![blocker],
                    None,
                );
                journal.set_state(HandoffJournalState::NeedsAttention);
                let _ = journal.persist(&self.codex_home).await;
                continue;
            }
            if node.turn_id.is_none() {
                if needs_pause_restore {
                    let thread_id = ThreadId::from_string(&node.thread_id).map_err(|error| {
                        invalid_params(format!(
                            "handoff node has an invalid thread id {}: {error}",
                            node.thread_id
                        ))
                    })?;
                    let thread = self
                        .thread_manager
                        .get_thread(thread_id)
                        .await
                        .map_err(core_error)?;
                    if let Err(_error) = thread.submit(Op::PauseActivity).await {
                        journal.update_node(
                            &node.thread_id,
                            HandoffNodeState::NeedsAttention,
                            vec![HandoffBlocker::Persistence],
                            None,
                        );
                        journal.set_state(HandoffJournalState::NeedsAttention);
                        let _ = journal.persist(&self.codex_home).await;
                        continue;
                    }
                }
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
                journal.clear_node_turn_id(&node.thread_id);
                if let Err(error) = journal.persist(&self.codex_home).await {
                    return Err(internal_error(format!(
                        "could not persist loaded idle handoff node {}: {error}",
                        node.thread_id
                    )));
                }
                continue;
            }
            journal.update_node(
                &node.thread_id,
                HandoffNodeState::Recovering,
                Vec::new(),
                None,
            );
            if let Err(error) = journal.persist(&self.codex_home).await {
                return Err(internal_error(format!(
                    "could not persist recovery start for {}: {error}",
                    node.thread_id
                )));
            }

            let thread_id = ThreadId::from_string(&node.thread_id).map_err(|error| {
                invalid_params(format!(
                    "handoff node has an invalid thread id {}: {error}",
                    node.thread_id
                ))
            })?;
            let thread = self
                .thread_manager
                .get_thread(thread_id)
                .await
                .map_err(core_error)?;
            if node.parent_thread_id.is_none() && node.was_paused {
                if let Err(_error) = thread.submit(Op::PauseActivity).await {
                    journal.update_node(
                        &node.thread_id,
                        HandoffNodeState::NeedsAttention,
                        vec![HandoffBlocker::Persistence],
                        None,
                    );
                    journal.set_state(HandoffJournalState::NeedsAttention);
                    let _ = journal.persist(&self.codex_home).await;
                    continue;
                }
            }
            let submission = thread
                .recover_turn_if_idle(RecoverTurnRequest {
                    turn_id: node.turn_id.clone().expect("checked above"),
                    thread_settings: Default::default(),
                    trace: None,
                    cyber_access_program: None,
                })
                .await;
            match submission {
                Ok(StartIfIdleSubmission::Started { turn_id })
                    if turn_id == node.turn_id.clone().expect("checked above") =>
                {
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
            if let Err(error) = journal.persist(&self.codex_home).await {
                return Err(internal_error(format!(
                    "could not persist recovery result for {}: {error}",
                    node.thread_id
                )));
            }
        }

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
        Ok(ThreadHandoffRecoverResponse {
            receipt: receipt_from_journal(&journal),
        })
    }

    async fn load_recovery_node(&self, node: &HandoffNode) -> Result<(), HandoffBlocker> {
        let thread_id =
            ThreadId::from_string(&node.thread_id).map_err(|_| HandoffBlocker::Persistence)?;
        let thread_is_loaded = self.thread_manager.get_thread(thread_id).await.is_ok();
        if thread_is_loaded && !matches!(node.state, HandoffNodeState::Suspended) {
            return Ok(());
        }
        // A partial old-runtime attempt can leave a closed Suspended thread in the manager map.
        // Remove that stale handle so the normal rollout/parent loader creates a live owner.
        if thread_is_loaded {
            self.thread_manager.remove_thread(&thread_id).await;
        }
        if node.parent_thread_id.is_some() {
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
            .map(|_| ())
            .map_err(|_| HandoffBlocker::Persistence)
    }
}
