use super::HandoffCoordinator;
use super::RECOVERY_ADMISSION_TIMEOUT;
use super::core_error;
use super::parse_thread_id;
use super::receipt_from_journal;
use super::recovery::LoadedRecoveryNode;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ThreadHandoffRecoverResponse;
use codex_core::HandoffBlocker;
use codex_core::HandoffJournal;
use codex_core::HandoffJournalState;
use codex_core::PendingMailboxBlockerDetail;
use codex_protocol::ThreadId;
use std::collections::HashMap;
use std::collections::HashSet;
use tokio::time::timeout;

async fn persist_preflight_mailbox_diagnostics(
    codex_home: &std::path::Path,
    journal: &mut HandoffJournal,
    thread_id: &str,
    current: &[PendingMailboxBlockerDetail],
) -> Result<(), JSONRPCErrorError> {
    if journal.record_pending_mailbox_diagnostics(thread_id, current) {
        journal.persist(codex_home).await.map_err(|error| {
            internal_error(format!(
                "could not persist handoff preflight diagnostics: {error}"
            ))
        })?;
    }
    Ok(())
}

/// A parsed journal node used by graph validation and quarantine admission.
#[derive(Clone, Debug)]
pub(super) struct ParsedHandoffNode {
    pub index: usize,
    pub thread_id: ThreadId,
    pub root_thread_id: ThreadId,
    pub parent_thread_id: Option<ThreadId>,
}

/// Parse and validate durable parent/root lineage before recovery or quarantine admission.
///
/// Normal recovery requires every parent to be represented in the receipt. Explicit quarantine
/// may retire a receipt whose parent/root artifacts disappeared, but still rejects duplicate IDs,
/// conflicting known lineage, and cycles among the nodes that remain recorded.
pub(super) fn validate_graph(
    journal: &HandoffJournal,
    require_complete: bool,
) -> Result<Vec<ParsedHandoffNode>, JSONRPCErrorError> {
    if journal.nodes.is_empty() {
        if journal.transfer_started == Some(true) {
            return Ok(Vec::new());
        }
        return Err(invalid_params(format!(
            "handoff {} does not contain any recorded nodes",
            journal.handoff_id
        )));
    }

    let mut parsed_nodes = Vec::with_capacity(journal.nodes.len());
    let mut node_indices = HashMap::with_capacity(journal.nodes.len());
    for (index, node) in journal.nodes.iter().enumerate() {
        let parsed = ParsedHandoffNode {
            index,
            thread_id: parse_thread_id(&node.thread_id)?,
            root_thread_id: parse_thread_id(&node.root_thread_id)?,
            parent_thread_id: node
                .parent_thread_id
                .as_deref()
                .map(parse_thread_id)
                .transpose()?,
        };
        if node_indices
            .insert(parsed.thread_id, parsed_nodes.len())
            .is_some()
        {
            return Err(invalid_params(format!(
                "handoff {} contains duplicate node {}",
                journal.handoff_id, node.thread_id
            )));
        }
        parsed_nodes.push(parsed);
    }

    for node in &parsed_nodes {
        if node.parent_thread_id.is_none() && node.thread_id != node.root_thread_id {
            return Err(invalid_params(format!(
                "handoff {} node {} has no parent but does not identify itself as its root",
                journal.handoff_id, node.thread_id
            )));
        }
        if node.thread_id == node.root_thread_id && node.parent_thread_id.is_some() {
            return Err(invalid_params(format!(
                "handoff {} root {} has a parent",
                journal.handoff_id, node.root_thread_id
            )));
        }
        if let Some(parent_thread_id) = node.parent_thread_id {
            if let Some(parent_index) = node_indices.get(&parent_thread_id) {
                let parent = &parsed_nodes[*parent_index];
                if parent.root_thread_id != node.root_thread_id {
                    return Err(invalid_params(format!(
                        "handoff {} node {} has inconsistent root lineage",
                        journal.handoff_id, node.thread_id
                    )));
                }
            } else if require_complete {
                return Err(invalid_params(format!(
                    "handoff {} node {} has an unrecorded parent {}",
                    journal.handoff_id, node.thread_id, parent_thread_id
                )));
            }
        }
    }

    // Walk each recorded parent chain. This catches cycles longer than a direct self-parent and
    // ensures complete recovery reaches the exact recorded root rather than an unrelated parent.
    for node in &parsed_nodes {
        let mut current = node.index;
        let mut visited = HashSet::new();
        loop {
            let current_node = &parsed_nodes[current];
            if !visited.insert(current_node.thread_id) {
                return Err(invalid_params(format!(
                    "handoff {} contains a parent cycle involving {}",
                    journal.handoff_id, current_node.thread_id
                )));
            }
            let Some(parent_thread_id) = current_node.parent_thread_id else {
                if require_complete && current_node.thread_id != node.root_thread_id {
                    return Err(invalid_params(format!(
                        "handoff {} node {} does not reach recorded root {}",
                        journal.handoff_id, node.thread_id, node.root_thread_id
                    )));
                }
                break;
            };
            let Some(parent_index) = node_indices.get(&parent_thread_id) else {
                // The quarantine path intentionally permits a genuinely missing parent/root.
                // Complete recovery rejected this case in the validation above.
                break;
            };
            current = *parent_index;
        }
    }

    Ok(parsed_nodes)
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
    pub(super) async fn quarantine(
        &self,
        journal: &mut HandoffJournal,
    ) -> Result<ThreadHandoffRecoverResponse, JSONRPCErrorError> {
        if journal.state != HandoffJournalState::NeedsAttention {
            return Err(invalid_params(
                "only a NeedsAttention handoff can be explicitly quarantined",
            ));
        }

        let state_db = self.state_db.clone().ok_or_else(|| {
            invalid_params("cannot quarantine handoff: durable state database is unavailable")
        })?;
        let parsed_nodes = validate_graph(journal, false)?;
        let mut root_ids = parsed_nodes
            .iter()
            .map(|node| node.root_thread_id)
            .collect::<Vec<_>>();
        root_ids.sort_by_key(std::string::ToString::to_string);
        root_ids.dedup();
        let node_ids = parsed_nodes
            .iter()
            .map(|node| node.thread_id)
            .collect::<HashSet<_>>();

        for node in &parsed_nodes {
            if let Some(parent_thread_id) = node.parent_thread_id
                && !parsed_nodes
                    .iter()
                    .any(|candidate| candidate.thread_id == parent_thread_id)
                && self
                    .thread_manager
                    .get_thread(parent_thread_id)
                    .await
                    .is_ok()
            {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: node {} has an unrecorded live parent",
                    journal.handoff_id, node.thread_id
                )));
            }
        }
        for root_thread_id in &root_ids {
            let has_recorded_root = parsed_nodes.iter().any(|node| {
                node.thread_id == *root_thread_id
                    && node.root_thread_id == *root_thread_id
                    && node.parent_thread_id.is_none()
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
        for parsed in &parsed_nodes {
            let Ok(thread) = self.thread_manager.get_thread(parsed.thread_id).await else {
                continue;
            };
            let config_snapshot = thread.config_snapshot().await;
            let parent_is_loaded = if let Some(parent_thread_id) = parsed.parent_thread_id {
                self.thread_manager
                    .get_thread(parent_thread_id)
                    .await
                    .is_ok()
            } else {
                true
            };
            if config_snapshot.parent_thread_id != parsed.parent_thread_id
                || (parsed.parent_thread_id.is_none() && parsed.thread_id != parsed.root_thread_id)
                || !parent_is_loaded
            {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: loaded node {} has inconsistent parent lineage",
                    journal.handoff_id, parsed.thread_id
                )));
            }
            loaded_nodes.push(LoadedRecoveryNode {
                index: parsed.index,
                thread,
            });
        }

        // A loaded descendant shares its root's AgentControl. If the recorded root is cold or
        // missing, there is no process-local tree gate to seal around that descendant; the
        // durable root pause alone cannot stop work that is already loaded. Cold/missing roots
        // remain valid when every affected node is cold, which is the stale-rollout case this
        // explicit resolution is meant to retire.
        for loaded in &loaded_nodes {
            let parsed = &parsed_nodes[loaded.index];
            if parsed.thread_id != parsed.root_thread_id
                && loaded_nodes
                    .iter()
                    .all(|candidate| candidate.thread.id() != parsed.root_thread_id)
            {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: loaded node {} has no loaded root {} to fence",
                    journal.handoff_id, parsed.thread_id, parsed.root_thread_id
                )));
            }
        }

        let mut tree_guards = Vec::new();
        let mut transition_guards = Vec::new();
        let mut guarded_root_ids = HashSet::new();
        for root_thread_id in &root_ids {
            let Some(loaded) = loaded_nodes
                .iter()
                .find(|loaded| loaded.thread.id() == *root_thread_id)
            else {
                continue;
            };
            if !guarded_root_ids.insert(*root_thread_id) {
                continue;
            }
            tree_guards.push(loaded.thread.begin_handoff().map_err(|error| {
                invalid_params(format!(
                    "cannot quarantine handoff {}: node {} handoff is already active: {error}",
                    journal.handoff_id, root_thread_id
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

        for loaded in &loaded_nodes {
            let thread_id = journal.nodes[loaded.index].thread_id.clone();
            let mut preflight = loaded.thread.handoff_preflight().await;
            persist_preflight_mailbox_diagnostics(
                &self.codex_home,
                journal,
                &thread_id,
                &preflight.pending_mailbox_diagnostics,
            )
            .await?;
            if preflight
                .blockers
                .iter()
                .any(|blocker| matches!(blocker, HandoffBlocker::LiveDescendants))
            {
                let live_subtree = self
                    .thread_manager
                    .list_open_agent_subtree_thread_ids(loaded.thread.id())
                    .await
                    .map_err(|error| {
                        invalid_params(format!(
                            "cannot quarantine handoff {}: could not inspect node {} descendants: {error}",
                            journal.handoff_id, thread_id
                        ))
                    })?;
                if live_subtree
                    .iter()
                    .all(|thread_id| node_ids.contains(thread_id))
                {
                    preflight = loaded.thread.handoff_preflight_after_descendants().await;
                    persist_preflight_mailbox_diagnostics(
                        &self.codex_home,
                        journal,
                        &thread_id,
                        &preflight.pending_mailbox_diagnostics,
                    )
                    .await?;
                }
            }
            let blockers = preflight.blockers;
            if preflight.was_running || !blockers.is_empty() {
                return Err(invalid_params(format!(
                    "cannot quarantine handoff {}: node {} still has active work ({})",
                    journal.handoff_id,
                    thread_id,
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
}

#[cfg(test)]
#[path = "quarantine_tests.rs"]
mod tests;
