use super::{ActiveHandoff, HandoffCoordinator, core_error, ordered_indices, parse_thread_id};
use crate::error_code::{internal_error, invalid_params};
use codex_app_server_protocol::{
    JSONRPCErrorError, ThreadHandoffPrepareParams, ThreadHandoffPrepareResponse,
};
use codex_core::{
    CodexThread, HandoffBlocker, HandoffJournal, HandoffJournalState, HandoffNode,
    HandoffNodeState, SuspendTurnOutcome,
};
use codex_protocol::ThreadId;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{Duration, timeout};

const HANDOFF_BARRIER_TIMEOUT: Duration = Duration::from_secs(30);

impl HandoffCoordinator {
    pub(crate) async fn prepare(
        &self,
        params: ThreadHandoffPrepareParams,
    ) -> Result<ThreadHandoffPrepareResponse, JSONRPCErrorError> {
        let _operation = self.operation.lock().await;
        let requested_root = params
            .root_thread_id
            .as_deref()
            .map(parse_thread_id)
            .transpose()?;

        let manager_guard = self.thread_manager.begin_handoff().map_err(core_error)?;
        if timeout(HANDOFF_BARRIER_TIMEOUT, manager_guard.wait_for_admissions())
            .await
            .is_err()
        {
            manager_guard.abort();
            return Err(internal_error(
                "timed out waiting for thread-manager handoff admissions",
            ));
        }

        let (roots, loaded_thread_ids) = match self.loaded_roots(requested_root).await {
            Ok(value) => value,
            Err(error) => {
                manager_guard.abort();
                return Err(error);
            }
        };

        let mut tree_guards = Vec::with_capacity(roots.len());
        for root in &roots {
            match root.begin_handoff() {
                Ok(guard) => tree_guards.push(guard),
                Err(error) => {
                    drop(tree_guards);
                    manager_guard.abort();
                    return Err(core_error(error));
                }
            }
        }
        for root_index in 0..roots.len() {
            let timed_out = timeout(
                HANDOFF_BARRIER_TIMEOUT,
                tree_guards[root_index].wait_for_admissions(),
            )
            .await
            .is_err();
            if timed_out {
                let root_id = roots[root_index].id();
                drop(tree_guards);
                manager_guard.abort();
                return Err(internal_error(format!(
                    "timed out waiting for handoff admissions for root {root_id}",
                )));
            }
        }

        let nodes = match self
            .snapshot_nodes(&roots, &loaded_thread_ids, requested_root)
            .await
        {
            Ok(nodes) => nodes,
            Err(error) => {
                drop(tree_guards);
                manager_guard.abort();
                return Err(error);
            }
        };
        let mut journal = match HandoffJournal::begin(
            &self.codex_home,
            self.runtime_version.clone(),
            nodes,
        )
        .await
        {
            Ok(journal) => journal,
            Err(error) => {
                drop(tree_guards);
                manager_guard.abort();
                return Err(internal_error(format!(
                    "could not persist handoff preparation: {error}"
                )));
            }
        };

        let blocked_nodes = journal
            .nodes
            .iter()
            .filter(|node| !node.blockers.is_empty())
            .map(|node| (node.thread_id.clone(), node.blockers.clone()))
            .collect::<Vec<_>>();
        if !blocked_nodes.is_empty() {
            journal.set_state(HandoffJournalState::NeedsAttention);
            for (thread_id, blockers) in blocked_nodes {
                journal.update_node(&thread_id, HandoffNodeState::NeedsAttention, blockers, None);
            }
            if let Err(error) = self.persist_journal(&journal).await {
                drop(tree_guards);
                manager_guard.abort();
                return Err(error);
            }
            let response = self.receipt_response(&journal);
            drop(tree_guards);
            manager_guard.abort();
            return Ok(response);
        }

        journal.set_state(HandoffJournalState::Draining);
        if let Err(error) = journal.persist(&self.codex_home).await {
            drop(tree_guards);
            manager_guard.abort();
            return Err(internal_error(format!(
                "could not persist handoff drain state: {error}"
            )));
        }

        for index in ordered_indices(&journal.nodes, true) {
            let thread_id = journal.nodes[index].thread_id.clone();
            journal.update_node(&thread_id, HandoffNodeState::Suspending, Vec::new(), None);
            if let Err(error) = journal.persist(&self.codex_home).await {
                journal.set_state(HandoffJournalState::NeedsAttention);
                let _ = journal.persist(&self.codex_home).await;
                drop(tree_guards);
                manager_guard.abort();
                return Err(internal_error(format!(
                    "could not persist suspension for {thread_id}: {error}"
                )));
            }

            let thread_id_value = match ThreadId::from_string(&thread_id) {
                Ok(value) => value,
                Err(error) => {
                    journal.update_node(
                        &thread_id,
                        HandoffNodeState::NeedsAttention,
                        vec![HandoffBlocker::Persistence],
                        None,
                    );
                    journal.set_state(HandoffJournalState::NeedsAttention);
                    let _ = journal.persist(&self.codex_home).await;
                    drop(tree_guards);
                    manager_guard.abort();
                    return Err(invalid_params(format!(
                        "handoff node has an invalid thread id {thread_id}: {error}"
                    )));
                }
            };
            let thread = match self.thread_manager.get_thread(thread_id_value).await {
                Ok(thread) => thread,
                Err(error) => {
                    journal.update_node(
                        &thread_id,
                        HandoffNodeState::NeedsAttention,
                        vec![HandoffBlocker::Persistence],
                        None,
                    );
                    journal.set_state(HandoffJournalState::NeedsAttention);
                    let _ = journal.persist(&self.codex_home).await;
                    drop(tree_guards);
                    manager_guard.abort();
                    return Ok(self.receipt_response(&journal));
                }
            };
            let has_children = journal
                .nodes
                .iter()
                .any(|node| node.parent_thread_id.as_deref() == Some(thread_id.as_str()));
            let outcome = if has_children {
                thread
                    .suspend_turn_and_shutdown_for_handoff_after_descendants()
                    .await
            } else {
                thread.suspend_turn_and_shutdown_for_handoff().await
            };
            match outcome {
                Ok(SuspendTurnOutcome::Suspended { turn_id }) => {
                    journal.update_node(
                        &thread_id,
                        HandoffNodeState::Suspended,
                        Vec::new(),
                        Some(turn_id),
                    );
                }
                Ok(SuspendTurnOutcome::NotActive) => {
                    if let Err(error) = thread.shutdown_and_wait().await {
                        journal.update_node(
                            &thread_id,
                            HandoffNodeState::NeedsAttention,
                            vec![HandoffBlocker::Persistence],
                            None,
                        );
                        journal.set_state(HandoffJournalState::NeedsAttention);
                        let _ = journal.persist(&self.codex_home).await;
                        drop(tree_guards);
                        manager_guard.abort();
                        return Ok(self.receipt_response(&journal));
                    }
                    journal.clear_node_turn_id(&thread_id);
                    journal.update_node(&thread_id, HandoffNodeState::Suspended, Vec::new(), None);
                }
                Ok(SuspendTurnOutcome::HasLiveDescendants) => {
                    journal.update_node(
                        &thread_id,
                        HandoffNodeState::NeedsAttention,
                        vec![HandoffBlocker::LiveDescendants],
                        None,
                    );
                    journal.set_state(HandoffJournalState::NeedsAttention);
                    if let Err(error) = self.persist_journal(&journal).await {
                        drop(tree_guards);
                        manager_guard.abort();
                        return Err(error);
                    }
                    let response = self.receipt_response(&journal);
                    drop(tree_guards);
                    manager_guard.abort();
                    return Ok(response);
                }
                Ok(SuspendTurnOutcome::Blocked { blockers }) => {
                    journal.update_node(
                        &thread_id,
                        HandoffNodeState::NeedsAttention,
                        blockers,
                        None,
                    );
                    journal.set_state(HandoffJournalState::NeedsAttention);
                    if let Err(error) = self.persist_journal(&journal).await {
                        drop(tree_guards);
                        manager_guard.abort();
                        return Err(error);
                    }
                    let response = self.receipt_response(&journal);
                    drop(tree_guards);
                    manager_guard.abort();
                    return Ok(response);
                }
                Ok(SuspendTurnOutcome::UnsupportedTask) => {
                    journal.update_node(
                        &thread_id,
                        HandoffNodeState::NeedsAttention,
                        vec![HandoffBlocker::UnsupportedTask],
                        None,
                    );
                    journal.set_state(HandoffJournalState::NeedsAttention);
                    if let Err(error) = self.persist_journal(&journal).await {
                        drop(tree_guards);
                        manager_guard.abort();
                        return Err(error);
                    }
                    let response = self.receipt_response(&journal);
                    drop(tree_guards);
                    manager_guard.abort();
                    return Ok(response);
                }
                Err(error) => {
                    journal.update_node(
                        &thread_id,
                        HandoffNodeState::NeedsAttention,
                        vec![HandoffBlocker::Persistence],
                        None,
                    );
                    journal.set_state(HandoffJournalState::NeedsAttention);
                    let _ = journal.persist(&self.codex_home).await;
                    drop(tree_guards);
                    manager_guard.abort();
                    return Err(core_error(error));
                }
            }
            if let Err(error) = journal.persist(&self.codex_home).await {
                journal.set_state(HandoffJournalState::NeedsAttention);
                journal.update_node(
                    &thread_id,
                    HandoffNodeState::NeedsAttention,
                    vec![HandoffBlocker::Persistence],
                    None,
                );
                let _ = journal.persist(&self.codex_home).await;
                drop(tree_guards);
                manager_guard.abort();
                return Err(internal_error(format!(
                    "could not persist handoff node {thread_id}: {error}"
                )));
            }
        }

        for root_index in 0..roots.len() {
            let timed_out = timeout(
                HANDOFF_BARRIER_TIMEOUT,
                tree_guards[root_index].wait_for_handoff_watchers(),
            )
            .await
            .is_err();
            if timed_out {
                let pending_root_ids = roots
                    .iter()
                    .skip(root_index)
                    .map(|root| root.id().to_string())
                    .collect::<HashSet<_>>();
                let affected = journal
                    .nodes
                    .iter()
                    .filter(|node| pending_root_ids.contains(&node.root_thread_id))
                    .map(|node| node.thread_id.clone())
                    .collect::<Vec<_>>();
                for thread_id in affected {
                    journal.update_node(
                        &thread_id,
                        HandoffNodeState::NeedsAttention,
                        vec![HandoffBlocker::SuspensionTimeout],
                        None,
                    );
                }
                journal.set_state(HandoffJournalState::NeedsAttention);
                if let Err(error) = self.persist_journal(&journal).await {
                    drop(tree_guards);
                    manager_guard.abort();
                    return Err(error);
                }
                let response = self.receipt_response(&journal);
                drop(tree_guards);
                manager_guard.abort();
                return Ok(response);
            }

            let mut late_blockers = roots[root_index].handoff_preflight().await.blockers;
            late_blockers.retain(|blocker| !matches!(blocker, HandoffBlocker::LiveDescendants));
            if !late_blockers.is_empty() {
                let root_id = roots[root_index].id().to_string();
                let affected = journal
                    .nodes
                    .iter()
                    .filter(|node| node.root_thread_id == root_id)
                    .map(|node| node.thread_id.clone())
                    .collect::<Vec<_>>();
                for thread_id in affected {
                    journal.update_node(
                        &thread_id,
                        HandoffNodeState::NeedsAttention,
                        late_blockers.clone(),
                        None,
                    );
                }
                journal.set_state(HandoffJournalState::NeedsAttention);
                if let Err(error) = self.persist_journal(&journal).await {
                    drop(tree_guards);
                    manager_guard.abort();
                    return Err(error);
                }
                let response = self.receipt_response(&journal);
                drop(tree_guards);
                manager_guard.abort();
                return Ok(response);
            }
        }

        journal.set_state(HandoffJournalState::Suspended);
        if let Err(error) = journal.persist(&self.codex_home).await {
            journal.set_state(HandoffJournalState::NeedsAttention);
            let _ = journal.persist(&self.codex_home).await;
            drop(tree_guards);
            manager_guard.abort();
            return Err(internal_error(format!(
                "could not persist suspended handoff: {error}"
            )));
        }

        let response = self.receipt_response(&journal);
        let handoff_id = journal.handoff_id.clone();
        self.active.lock().await.insert(
            handoff_id,
            ActiveHandoff {
                manager_guard,
                tree_guards,
            },
        );
        Ok(response)
    }

    async fn loaded_roots(
        &self,
        requested_root: Option<ThreadId>,
    ) -> Result<(Vec<Arc<CodexThread>>, HashSet<ThreadId>), JSONRPCErrorError> {
        let loaded_ids = self
            .thread_manager
            .list_thread_ids()
            .await
            .into_iter()
            .collect::<HashSet<_>>();
        let mut roots = Vec::new();
        for thread_id in &loaded_ids {
            let Ok(thread) = self.thread_manager.get_thread(*thread_id).await else {
                continue;
            };
            if thread.session_source().parent_thread_id().is_none()
                && requested_root.is_none_or(|root| root == *thread_id)
            {
                roots.push(thread);
            }
        }
        roots.sort_by_key(|thread| thread.id().to_string());
        if let Some(requested_root) = requested_root
            && roots.iter().all(|root| root.id() != requested_root)
        {
            return Err(invalid_params(format!(
                "loaded root thread {requested_root} was not found"
            )));
        }
        Ok((roots, loaded_ids))
    }

    async fn snapshot_nodes(
        &self,
        roots: &[Arc<CodexThread>],
        loaded_ids: &HashSet<ThreadId>,
        requested_root: Option<ThreadId>,
    ) -> Result<Vec<HandoffNode>, JSONRPCErrorError> {
        let mut nodes = Vec::new();
        let mut seen = HashSet::new();
        for root in roots {
            let root_id = root.id();
            let subtree_ids = self
                .thread_manager
                .list_agent_subtree_thread_ids(root_id)
                .await
                .map_err(core_error)?;
            for thread_id in subtree_ids {
                if !loaded_ids.contains(&thread_id) || !seen.insert(thread_id) {
                    continue;
                }
                let thread = self
                    .thread_manager
                    .get_thread(thread_id)
                    .await
                    .map_err(core_error)?;
                let source = thread.session_source();
                let rollout_path = self.materialize_rollout_path(&thread).await?;
                let preflight = thread.handoff_preflight().await;
                let mut blockers = preflight.blockers;
                // A parent-linked node with no persisted version cannot be routed safely during
                // replacement. Detect it while the old owner is still fenced, before suspension.
                if source.parent_thread_id().is_some()
                    && thread.multi_agent_version().is_none()
                    && !blockers
                        .iter()
                        .any(|blocker| matches!(blocker, HandoffBlocker::ParentUnavailable))
                {
                    blockers.push(HandoffBlocker::ParentUnavailable);
                }
                // The manager gate makes this loaded subtree snapshot stable; loaded descendants
                // are represented as their own nodes and drained child-first below; cold descendants remain in the
                // graph store for a later parent-aware load.
                if blockers
                    .iter()
                    .any(|blocker| matches!(blocker, HandoffBlocker::LiveDescendants))
                {
                    blockers.retain(|blocker| !matches!(blocker, HandoffBlocker::LiveDescendants));
                }
                nodes.push(HandoffNode {
                    thread_id: thread_id.to_string(),
                    root_thread_id: root_id.to_string(),
                    parent_thread_id: source.parent_thread_id().map(|id| id.to_string()),
                    agent_path: source.get_agent_path().map(|path| path.to_string()),
                    turn_id: preflight.turn_id,
                    rollout_path: Some(rollout_path.to_string_lossy().into_owned()),
                    was_running: preflight.was_running,
                    was_paused: preflight.was_paused,
                    state: HandoffNodeState::Planned,
                    blockers,
                });
            }
        }

        // Every loaded child is eligible for an all-roots handoff. If the graph query omitted a
        // loaded node, its parent chain cannot be proven covered before side effects begin. Keep a
        // node-level blocker in the durable receipt so prepare fails closed instead of publishing
        // a partial transferable graph. A requested root may exclude a loaded, unrelated root when
        // its parent chain is known; an unknown chain remains blocked conservatively.
        for thread_id in loaded_ids {
            if seen.contains(thread_id) {
                continue;
            }
            let thread = self
                .thread_manager
                .get_thread(*thread_id)
                .await
                .map_err(core_error)?;
            let source = thread.session_source();
            let chain_root = self.loaded_chain_root(*thread_id).await;
            if let (Some(requested_root), Some(chain_root)) = (requested_root, chain_root)
                && requested_root != chain_root
            {
                continue;
            }
            let rollout_path = self.materialize_rollout_path(&thread).await?;
            let preflight = thread.handoff_preflight().await;
            let mut blockers = preflight.blockers;
            blockers.retain(|blocker| !matches!(blocker, HandoffBlocker::LiveDescendants));
            if !blockers
                .iter()
                .any(|blocker| matches!(blocker, HandoffBlocker::ParentUnavailable))
            {
                blockers.push(HandoffBlocker::ParentUnavailable);
            }
            let fallback_root = source
                .parent_thread_id()
                .unwrap_or(*thread_id)
                .to_string();
            nodes.push(HandoffNode {
                thread_id: thread_id.to_string(),
                root_thread_id: chain_root
                    .map(|root| root.to_string())
                    .unwrap_or(fallback_root),
                parent_thread_id: source.parent_thread_id().map(|id| id.to_string()),
                agent_path: source.get_agent_path().map(|path| path.to_string()),
                turn_id: preflight.turn_id,
                rollout_path: Some(rollout_path.to_string_lossy().into_owned()),
                was_running: preflight.was_running,
                was_paused: preflight.was_paused,
                state: HandoffNodeState::Planned,
                blockers,
            });
        }
        nodes.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
        Ok(nodes)
    }

    async fn materialize_rollout_path(
        &self,
        thread: &CodexThread,
    ) -> Result<PathBuf, JSONRPCErrorError> {
        thread.ensure_rollout_materialized().await;
        thread.flush_rollout().await.map_err(|error| {
            internal_error(format!(
                "could not flush rollout for {}: {error}",
                thread.id()
            ))
        })?;
        let stored = thread.read_thread(/*include_archived*/ true, /*include_history*/ false).await.map_err(|error| {
            internal_error(format!(
                "could not read materialized rollout for {}: {error}",
                thread.id()
            ))
        })?;
        let path = stored
            .rollout_path
            .or_else(|| thread.rollout_path())
            .ok_or_else(|| {
                internal_error(format!(
                    "thread {} has no materialized rollout path",
                    thread.id()
                ))
            })?;
        codex_rollout::existing_rollout_path(&path)
            .await
            .ok_or_else(|| {
                internal_error(format!(
                    "thread {} rollout does not exist at {}",
                    thread.id(),
                    path.display()
                ))
            })
    }

    async fn loaded_chain_root(&self, thread_id: ThreadId) -> Option<ThreadId> {
        let mut current = thread_id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                return None;
            }
            let thread = self.thread_manager.get_thread(current).await.ok()?;
            let Some(parent_thread_id) = thread.session_source().parent_thread_id() else {
                return Some(current);
            };
            current = parent_thread_id;
        }
    }
}
