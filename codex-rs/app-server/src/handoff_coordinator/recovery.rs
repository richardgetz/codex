use super::HandoffCoordinator;
use super::core_error;
use super::ordered_indices;
use super::parse_thread_id;
use super::receipt_from_journal;
use crate::error_code::invalid_params;
use crate::outgoing_message::ConnectionId;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ThreadHandoffRecoverParams;
use codex_app_server_protocol::ThreadHandoffRecoverResponse;
use codex_app_server_protocol::ThreadHandoffRecoveryResolution;
use codex_core::CodexThread;
use codex_core::HandoffBlocker;
use codex_core::HandoffJournal;
use codex_core::HandoffJournalState;
use codex_core::HandoffNode;
use codex_core::HandoffNodeState;
use codex_core::RecoverTurnRequest;
use codex_core::StartIfIdleSubmission;
use codex_protocol::ThreadId;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadPauseState;
use codex_rollout::InitialHistory;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::Duration;
use tokio::time::timeout;

const RECOVERY_ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);

struct LoadedRecoveryNode {
    index: usize,
    thread: Arc<CodexThread>,
    root_thread_id: ThreadId,
}

fn format_handoff_blocker(blocker: &HandoffBlocker) -> String {
    format!("{blocker:?}")
}

fn format_handoff_blockers(blockers: &[HandoffBlocker]) -> String {
    blockers
        .iter()
        .map(format_handoff_blocker)
        .collect::<Vec<_>>()
        .join(", ")
}

impl HandoffCoordinator {
    pub(crate) async fn recover(
        &self,
        params: ThreadHandoffRecoverParams,
        connection_id: ConnectionId,
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
        if journal.quarantined && journal.state != HandoffJournalState::NeedsAttention {
            return Err(invalid_params(
                "quarantine marker is only valid on a NeedsAttention handoff",
            ));
        }
        if journal.quarantined {
            self.refresh_startup_recovery_state().await;
            return Ok(ThreadHandoffRecoverResponse {
                receipt: receipt_from_journal(&journal),
            });
        }
        if !journal.requires_recovery() {
            // A preparation-only failure is already terminal for startup fencing. Do not enter
            // the recovery-pending guard: there is no graph to restore, and dropping that guard
            // without completing it would re-wedge every future startup mutation.
            self.refresh_startup_recovery_state().await;
            return Ok(ThreadHandoffRecoverResponse {
                receipt: receipt_from_journal(&journal),
            });
        }
        if params.resolution == Some(ThreadHandoffRecoveryResolution::Quarantine) {
            let result = self.quarantine(&mut journal).await;
            if result.is_err() {
                // The failed explicit resolution leaves the manager's recovery guard fail-closed;
                // refresh the cached startup probe as well so a write arriving after the error
                // cannot observe a stale Ready value from before this journal was created.
                self.refresh_startup_recovery_state().await;
            }
            return result;
        }
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
        if timeout(
            RECOVERY_ADMISSION_TIMEOUT,
            recovery_pending.wait_for_admissions(),
        )
        .await
        .is_err()
        {
            journal.set_state(HandoffJournalState::NeedsAttention);
            self.persist_journal(&journal).await?;
            self.refresh_startup_recovery_state().await;
            return Ok(ThreadHandoffRecoverResponse {
                receipt: receipt_from_journal(&journal),
            });
        }
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

        // Start the normal app-server listener for every restored node before any exact turn is
        // admitted. This preserves completion/activity events through the same channel used by a
        // regular thread/resume, even though recovery has no client connection to subscribe.
        if !self
            .attach_recovery_listeners(&mut journal, &loaded_nodes, connection_id)
            .await?
        {
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

    async fn quarantine(
        &self,
        journal: &mut HandoffJournal,
    ) -> Result<ThreadHandoffRecoverResponse, JSONRPCErrorError> {
        if journal.state != HandoffJournalState::NeedsAttention {
            return Err(invalid_params(
                "only a NeedsAttention handoff can be explicitly quarantined",
            ));
        }

        if journal.nodes.is_empty() {
            return Err(invalid_params(
                "cannot quarantine a handoff without any recorded nodes",
            ));
        }

        let state_db = self.state_db.clone().ok_or_else(|| {
            invalid_params("cannot quarantine handoff: durable state database is unavailable")
        })?;

        // Validate the durable graph before changing any live or durable state. Quarantine is
        // allowed to retire a stale receipt whose rollout/parent has disappeared, but it must
        // still have a concrete root to pause and retain the original node diagnostics.
        let mut parsed_nodes = Vec::with_capacity(journal.nodes.len());
        let mut root_ids = Vec::new();
        let mut node_ids = HashSet::new();
        for (index, node) in journal.nodes.iter().enumerate() {
            let thread_id = parse_thread_id(&node.thread_id)?;
            let root_thread_id = parse_thread_id(&node.root_thread_id)?;
            let parent_thread_id = node
                .parent_thread_id
                .as_deref()
                .map(parse_thread_id)
                .transpose()?;
            if !node_ids.insert(thread_id) {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: duplicate node {}",
                    journal.handoff_id, node.thread_id
                )));
            }
            if parent_thread_id == Some(thread_id) {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: node {} is its own parent",
                    journal.handoff_id, node.thread_id
                )));
            }
            if !root_ids.contains(&root_thread_id) {
                root_ids.push(root_thread_id);
            }
            parsed_nodes.push((index, thread_id, root_thread_id, parent_thread_id));
        }
        let journal_nodes_by_id = journal
            .nodes
            .iter()
            .map(|node| {
                (
                    ThreadId::from_string(&node.thread_id).expect("node ids were parsed above"),
                    (
                        ThreadId::from_string(&node.root_thread_id)
                            .expect("root ids were parsed above"),
                        node.parent_thread_id.as_deref().map(|parent| {
                            ThreadId::from_string(parent).expect("parent ids were parsed above")
                        }),
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        for (_, thread_id, root_thread_id, parent_thread_id) in &parsed_nodes {
            if parent_thread_id.is_none() && thread_id != root_thread_id {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: root-less node {} does not identify itself as the root",
                    journal.handoff_id, thread_id
                )));
            }
            if thread_id == root_thread_id && parent_thread_id.is_some() {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: root {} has a parent",
                    journal.handoff_id, root_thread_id
                )));
            }
            if let Some(parent_thread_id) = parent_thread_id {
                if let Some((parent_root_thread_id, _)) = journal_nodes_by_id.get(parent_thread_id)
                    && parent_root_thread_id != root_thread_id
                {
                    return Err(invalid_params(format!(
                        "cannot quarantine handoff {}: node {} has inconsistent root lineage",
                        journal.handoff_id, thread_id
                    )));
                }
                // A loaded parent outside the receipt means the durable graph is incomplete. A
                // cold/missing parent is the stale case quarantine is designed to retire.
                if !journal_nodes_by_id.contains_key(parent_thread_id)
                    && self
                        .thread_manager
                        .get_thread(*parent_thread_id)
                        .await
                        .is_ok()
                {
                    return Err(invalid_params(format!(
                        "cannot quarantine handoff {}: node {} has an unrecorded live parent",
                        journal.handoff_id, thread_id
                    )));
                }
            }
        }
        for root_thread_id in &root_ids {
            let has_recorded_root = parsed_nodes.iter().any(|(_, thread_id, root, parent)| {
                thread_id == root_thread_id && root == root_thread_id && parent.is_none()
            });
            if !has_recorded_root
                && self
                    .thread_manager
                    .get_thread(*root_thread_id)
                    .await
                    .is_ok()
            {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: root {} is live but missing from the receipt",
                    journal.handoff_id, root_thread_id
                )));
            }
        }

        // Keep both the manager-wide handoff fence and the recovery-pending fence through every
        // live preflight, durable pause, and journal write. The manager fence closes new roots;
        // each loaded root below gets its own tree fence before it is inspected.
        let manager_guard = self.thread_manager.begin_handoff().map_err(core_error)?;
        if timeout(
            RECOVERY_ADMISSION_TIMEOUT,
            manager_guard.wait_for_admissions(),
        )
        .await
        .is_err()
        {
            manager_guard.abort();
            return Err(invalid_params(
                "thread-manager handoff admissions did not drain; quarantine was not applied",
            ));
        }
        let recovery_pending = self
            .thread_manager
            .begin_recovery_pending()
            .map_err(core_error)?;
        if timeout(
            RECOVERY_ADMISSION_TIMEOUT,
            recovery_pending.wait_for_admissions(),
        )
        .await
        .is_err()
        {
            return Err(invalid_params(
                "handoff admissions did not drain; quarantine was not applied",
            ));
        }

        // Only inspect sessions that are already live in this process. Loading a missing rollout
        // here would turn an explicit stale-state resolution into the same recovery loop it is
        // intended to escape. Cold roots are protected by the durable activity pause below.
        let mut loaded_nodes = Vec::new();
        for (index, thread_id, root_thread_id, parent_thread_id) in &parsed_nodes {
            let Ok(thread) = self.thread_manager.get_thread(*thread_id).await else {
                continue;
            };
            let config_snapshot = thread.config_snapshot().await;
            let parent_is_loaded = if let Some(parent_thread_id) = parent_thread_id {
                self.thread_manager
                    .get_thread(*parent_thread_id)
                    .await
                    .is_ok()
            } else {
                true
            };
            if config_snapshot.parent_thread_id != *parent_thread_id
                || (parent_thread_id.is_none() && thread_id != root_thread_id)
                || !parent_is_loaded
            {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: loaded node {} has inconsistent parent lineage",
                    journal.handoff_id, thread_id
                )));
            }
            loaded_nodes.push(LoadedRecoveryNode {
                index: *index,
                thread,
                root_thread_id: *root_thread_id,
            });
        }

        let mut tree_guards = Vec::new();
        let mut transition_guards = Vec::new();
        let mut guarded_thread_ids = HashSet::new();
        for loaded in &loaded_nodes {
            if !guarded_thread_ids.insert(loaded.thread.id()) {
                continue;
            }
            tree_guards.push(loaded.thread.begin_handoff().map_err(|error| {
                invalid_params(format!(
                    "cannot quarantine handoff {}: node {} handoff is already active: {error}",
                    journal.handoff_id,
                    loaded.thread.id()
                ))
            })?);
            transition_guards.push(loaded.thread.lock_activity_transition().await);
        }
        for tree_guard in &tree_guards {
            if timeout(RECOVERY_ADMISSION_TIMEOUT, tree_guard.wait_for_admissions())
                .await
                .is_err()
            {
                return Err(invalid_params(
                    "thread-tree handoff admissions did not drain; quarantine was not applied",
                ));
            }
        }

        let recorded_thread_ids = node_ids;
        for loaded in &loaded_nodes {
            let node = &journal.nodes[loaded.index];
            let mut preflight = loaded.thread.handoff_preflight().await;
            if preflight
                .blockers
                .iter()
                .any(|blocker| matches!(blocker, HandoffBlocker::LiveDescendants))
            {
                let live_subtree = self
                    .thread_manager
                    .list_agent_subtree_thread_ids(loaded.thread.id())
                    .await
                    .map_err(|error| {
                        invalid_params(format!(
                            "cannot quarantine handoff {}: could not inspect node {} descendants: {error}",
                            journal.handoff_id, node.thread_id
                        ))
                    })?;
                if live_subtree
                    .iter()
                    .all(|thread_id| recorded_thread_ids.contains(thread_id))
                {
                    preflight = loaded.thread.handoff_preflight_after_descendants().await;
                }
            }
            let blockers = preflight.blockers;
            if preflight.was_running || !blockers.is_empty() {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: node {} still has active work ({})",
                    journal.handoff_id,
                    node.thread_id,
                    format_handoff_blockers(&blockers)
                )));
            }
        }

        for root_thread_id in root_ids {
            let marker = state_db
                .pause_thread_activity(root_thread_id)
                .await
                .map_err(|error| {
                invalid_params(format!(
                    "cannot quarantine handoff {}: failed to persist pause for root {}: {error}",
                    journal.handoff_id, root_thread_id
                ))
                })?;
            if let Some(loaded) = loaded_nodes
                .iter()
                .find(|loaded| loaded.thread.id() == root_thread_id)
                && let Err(error) = loaded.thread.pause_activity_with_ack(None).await
            {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: failed to pause root {}: {error}",
                    journal.handoff_id, root_thread_id
                )));
            }
            let applied = state_db
                .complete_thread_activity_pause(root_thread_id, marker.generation)
                .await
                .map_err(|error| {
                    invalid_params(format!(
                        "cannot quarantine handoff {}: failed to persist paused root {}: {error}",
                        journal.handoff_id, root_thread_id
                    ))
                })?;
            if !applied {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: root {} pause changed concurrently",
                    journal.handoff_id, root_thread_id
                )));
            }
        }

        journal.mark_quarantined();
        self.persist_journal(journal).await?;
        drop(transition_guards);
        drop(tree_guards);
        manager_guard.abort();
        recovery_pending.complete();
        self.refresh_startup_recovery_state().await;
        Ok(ThreadHandoffRecoverResponse {
            receipt: receipt_from_journal(journal),
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
            let retry_failed_transfer = journal.transfer_started == Some(true)
                && node.state == HandoffNodeState::NeedsAttention
                && node.turn_id.is_some()
                && !node.blockers.is_empty()
                && node.blockers.iter().all(|blocker| {
                    matches!(
                        blocker,
                        HandoffBlocker::Persistence | HandoffBlocker::ParentUnavailable
                    )
                });
            if (node.state == HandoffNodeState::NeedsAttention || !node.blockers.is_empty())
                && !retry_failed_transfer
            {
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
                Ok(thread) => loaded_nodes.push(LoadedRecoveryNode {
                    index,
                    thread,
                    root_thread_id: parse_thread_id(&node.root_thread_id)?,
                }),
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

    async fn attach_recovery_listeners(
        &self,
        journal: &mut HandoffJournal,
        loaded_nodes: &[LoadedRecoveryNode],
        connection_id: ConnectionId,
    ) -> Result<bool, JSONRPCErrorError> {
        let mut all_listeners_attached = true;
        for loaded in loaded_nodes {
            let node = journal.nodes[loaded.index].clone();
            let thread_id = parse_thread_id(&node.thread_id)?;
            if let Err(error) = self
                .thread_processor
                .attach_recovery_listener(thread_id, connection_id)
                .await
            {
                all_listeners_attached = false;
                journal.update_node(
                    &node.thread_id,
                    HandoffNodeState::NeedsAttention,
                    vec![HandoffBlocker::Persistence],
                    None,
                );
                tracing::warn!(thread_id = %node.thread_id, error = %error.message, "failed to attach recovery listener");
            }
        }
        Ok(all_listeners_attached)
    }

    async fn restore_pause_state(
        &self,
        journal: &mut HandoffJournal,
        loaded_nodes: &[LoadedRecoveryNode],
    ) -> Result<bool, JSONRPCErrorError> {
        let mut all_pauses_restored = true;
        for loaded in loaded_nodes {
            let node = journal.nodes[loaded.index].clone();
            if node.was_paused {
                let pause_submitted = loaded.thread.submit(Op::PauseActivity).await.is_ok();
                let thread_id = loaded.thread.id();
                let pause_applied = pause_submitted
                    && timeout(RECOVERY_ADMISSION_TIMEOUT, async {
                        loop {
                            let is_paused =
                                loaded.thread.activity_snapshot().await.into_iter().any(
                                    |activity| {
                                        activity.thread_id == thread_id
                                            && activity.pause_state == ThreadPauseState::Paused
                                    },
                                );
                            if is_paused {
                                break true;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .is_ok();
                if !pause_applied {
                    all_pauses_restored = false;
                    journal.update_node(
                        &node.thread_id,
                        HandoffNodeState::NeedsAttention,
                        vec![HandoffBlocker::Persistence],
                        None,
                    );
                    continue;
                }
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
                && matches!(
                    node.state,
                    HandoffNodeState::Restored | HandoffNodeState::Paused
                )
            {
                journal.update_node(&node.thread_id, HandoffNodeState::Paused, Vec::new(), None);
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
            let retry_failed_transfer = journal.transfer_started == Some(true)
                && node.state == HandoffNodeState::NeedsAttention
                && node.turn_id.is_some()
                && !node.blockers.is_empty()
                && node.blockers.iter().all(|blocker| {
                    matches!(
                        blocker,
                        HandoffBlocker::Persistence | HandoffBlocker::ParentUnavailable
                    )
                });
            if !node.blockers.is_empty() || node.state == HandoffNodeState::NeedsAttention {
                if !retry_failed_transfer {
                    continue;
                }
                let preflight = loaded.thread.handoff_preflight().await;
                let mut blockers = preflight.blockers;
                blockers.retain(|blocker| !matches!(blocker, HandoffBlocker::LiveDescendants));
                if preflight.was_running {
                    if blockers.is_empty()
                        && preflight.turn_id.as_deref() == Some(expected_turn_id.as_str())
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
                if !blockers.is_empty() {
                    journal.update_node(
                        &node.thread_id,
                        HandoffNodeState::NeedsAttention,
                        blockers,
                        None,
                    );
                    journal.set_state(HandoffJournalState::NeedsAttention);
                    self.persist_journal(journal).await?;
                    continue;
                }
                journal.update_node(
                    &node.thread_id,
                    HandoffNodeState::Recovering,
                    Vec::new(),
                    None,
                );
                self.persist_journal(journal).await?;
            }

            if !retry_failed_transfer
                && matches!(
                    node.state,
                    HandoffNodeState::Recovering
                        | HandoffNodeState::Restored
                        | HandoffNodeState::Paused
                )
            {
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

            if !retry_failed_transfer && node.state != HandoffNodeState::Suspended {
                continue;
            }
            if !retry_failed_transfer {
                journal.update_node(
                    &node.thread_id,
                    HandoffNodeState::Recovering,
                    Vec::new(),
                    None,
                );
                self.persist_journal(journal).await?;
            }
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
        if let Some(thread) = loaded_thread.as_ref() {
            let expected_parent_thread_id = node
                .parent_thread_id
                .as_deref()
                .map(ThreadId::from_string)
                .transpose()
                .map_err(|_| HandoffBlocker::ParentUnavailable)?;
            let config_snapshot = thread.config_snapshot().await;
            if config_snapshot.parent_thread_id != expected_parent_thread_id
                || (expected_parent_thread_id.is_none() && node.root_thread_id != node.thread_id)
            {
                return Err(HandoffBlocker::ParentUnavailable);
            }
            if let Some(parent_thread_id) = expected_parent_thread_id
                && self
                    .thread_manager
                    .get_thread(parent_thread_id)
                    .await
                    .is_err()
            {
                return Err(HandoffBlocker::ParentUnavailable);
            }
            if !matches!(node.state, HandoffNodeState::Suspended) {
                return Ok(thread.clone());
            }
        }
        let multi_agent_version = if let Some(version) = loaded_thread
            .as_ref()
            .and_then(|thread| thread.multi_agent_version())
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
        } else if node.parent_thread_id.is_some()
            && multi_agent_version == Some(MultiAgentVersion::V1)
        {
            self.thread_manager
                .ensure_v1_agent_loaded(thread_id)
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
