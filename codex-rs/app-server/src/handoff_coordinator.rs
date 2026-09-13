//! App-server coordinator for safe process handoff and exact-turn recovery.
//!
//! The coordinator owns the durable boundary between the process-local Core
//! fences and the app-server protocol. It never replays an input: a replacement
//! loads the recorded rollout and asks Core to resume the exact interrupted turn.

mod prepare;
mod recovery;

use crate::error_code::{internal_error, invalid_params};
use codex_app_server_protocol::{
    JSONRPCErrorError, ThreadHandoffNode, ThreadHandoffNodeState, ThreadHandoffPrepareResponse,
    ThreadHandoffReceipt, ThreadHandoffState, ThreadHandoffStatusParams,
    ThreadHandoffStatusResponse,
};
use codex_core::config::Config;
use codex_core::{
    HandoffGuard, HandoffJournal, HandoffJournalState, HandoffNode, HandoffNodeState,
    ThreadManager, ThreadManagerHandoffGuard,
};
use codex_protocol::ThreadId;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

struct ActiveHandoff {
    manager_guard: ThreadManagerHandoffGuard,
    tree_guards: Vec<HandoffGuard>,
}

/// Coordinates one durable, all-loaded-roots handoff at a time.
pub(crate) struct HandoffCoordinator {
    thread_manager: Arc<ThreadManager>,
    config: Arc<Config>,
    codex_home: PathBuf,
    runtime_version: String,
    operation: Mutex<()>,
    active: Mutex<HashMap<String, ActiveHandoff>>,
}

impl HandoffCoordinator {
    pub(crate) fn new(
        thread_manager: Arc<ThreadManager>,
        config: Arc<Config>,
        codex_home: PathBuf,
        runtime_version: String,
    ) -> Self {
        Self {
            thread_manager,
            config,
            codex_home,
            runtime_version,
            operation: Mutex::new(()),
            active: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn status(
        &self,
        params: ThreadHandoffStatusParams,
    ) -> Result<ThreadHandoffStatusResponse, JSONRPCErrorError> {
        let journal = self.load_journal(&params.handoff_id).await?;
        Ok(ThreadHandoffStatusResponse {
            receipt: receipt_from_journal(&journal),
        })
    }
    async fn load_journal(&self, handoff_id: &str) -> Result<HandoffJournal, JSONRPCErrorError> {
        validate_handoff_id(handoff_id)?;
        HandoffJournal::load_all(&self.codex_home)
            .await
            .map_err(|error| internal_error(format!("could not read handoff journal: {error}")))?
            .into_iter()
            .find(|journal| journal.handoff_id == handoff_id)
            .ok_or_else(|| invalid_params(format!("unknown handoff id {handoff_id}")))
    }

    async fn persist_journal(&self, journal: &HandoffJournal) -> Result<(), JSONRPCErrorError> {
        journal.persist(&self.codex_home).await.map_err(|error| {
            internal_error(format!("could not persist handoff receipt: {error}"))
        })?;
        Ok(())
    }

    fn receipt_response(&self, journal: &HandoffJournal) -> ThreadHandoffPrepareResponse {
        ThreadHandoffPrepareResponse {
            receipt: receipt_from_journal(journal),
        }
    }
}

fn parse_thread_id(value: &str) -> Result<ThreadId, JSONRPCErrorError> {
    ThreadId::from_string(value)
        .map_err(|error| invalid_params(format!("invalid rootThreadId {value}: {error}")))
}

fn validate_handoff_id(value: &str) -> Result<(), JSONRPCErrorError> {
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(invalid_params("handoffId contains unsupported characters"));
    }
    Ok(())
}

fn core_error(error: impl std::fmt::Display) -> JSONRPCErrorError {
    internal_error(error.to_string())
}

fn ordered_indices(nodes: &[HandoffNode], child_first: bool) -> Vec<usize> {
    let mut depths = vec![None; nodes.len()];
    let mut indices = (0..nodes.len()).collect::<Vec<_>>();
    for index in 0..nodes.len() {
        let _ = node_depth(index, nodes, &mut depths, &mut HashSet::new());
    }
    indices.sort_by(|left, right| {
        let left_depth = depths[*left].unwrap_or_default();
        let right_depth = depths[*right].unwrap_or_default();
        let depth_order = if child_first {
            right_depth.cmp(&left_depth)
        } else {
            left_depth.cmp(&right_depth)
        };
        depth_order.then_with(|| nodes[*left].thread_id.cmp(&nodes[*right].thread_id))
    });
    indices
}

fn node_depth(
    index: usize,
    nodes: &[HandoffNode],
    depths: &mut [Option<usize>],
    visiting: &mut HashSet<usize>,
) -> usize {
    if let Some(depth) = depths[index] {
        return depth;
    }
    if !visiting.insert(index) {
        return 0;
    }
    let depth = nodes[index]
        .parent_thread_id
        .as_deref()
        .and_then(|parent| nodes.iter().position(|node| node.thread_id == parent))
        .map(|parent| node_depth(parent, nodes, depths, visiting) + 1)
        .unwrap_or_default();
    visiting.remove(&index);
    depths[index] = Some(depth);
    depth
}

fn receipt_from_journal(journal: &HandoffJournal) -> ThreadHandoffReceipt {
    ThreadHandoffReceipt {
        handoff_id: journal.handoff_id.clone(),
        state: journal.state.into(),
        runtime_version: journal.runtime_version.clone(),
        created_at: journal.created_at_ms.max(0).div_euclid(1000),
        nodes: journal.nodes.iter().map(api_node_from_core).collect(),
    }
}

fn api_node_from_core(node: &HandoffNode) -> ThreadHandoffNode {
    ThreadHandoffNode {
        thread_id: node.thread_id.clone(),
        root_thread_id: node.root_thread_id.clone(),
        parent_thread_id: node.parent_thread_id.clone(),
        agent_path: node.agent_path.clone(),
        turn_id: node.turn_id.clone(),
        rollout_path: node.rollout_path.clone(),
        was_running: node.was_running,
        was_paused: node.was_paused,
        state: match (node.state, node.turn_id.is_some()) {
            (HandoffNodeState::Suspended, false) => ThreadHandoffNodeState::NotActive,
            (HandoffNodeState::Planned, _) => ThreadHandoffNodeState::Planned,
            (HandoffNodeState::Suspending, _) => ThreadHandoffNodeState::Suspending,
            (HandoffNodeState::Suspended, true) => ThreadHandoffNodeState::Suspended,
            (HandoffNodeState::Recovering, _) => ThreadHandoffNodeState::Recovering,
            (HandoffNodeState::Restored, _) => ThreadHandoffNodeState::Restored,
            (HandoffNodeState::Paused, _) => ThreadHandoffNodeState::Paused,
            (HandoffNodeState::NeedsAttention, _) => ThreadHandoffNodeState::NeedsAttention,
        },
        blockers: node.blockers.iter().cloned().map(Into::into).collect(),
    }
}

impl From<HandoffJournalState> for ThreadHandoffState {
    fn from(value: HandoffJournalState) -> Self {
        match value {
            HandoffJournalState::Prepared => Self::Prepared,
            HandoffJournalState::Draining => Self::Draining,
            HandoffJournalState::Suspended => Self::Suspended,
            HandoffJournalState::Restoring => Self::Restoring,
            HandoffJournalState::Completed => Self::Completed,
            HandoffJournalState::NeedsAttention => Self::NeedsAttention,
        }
    }
}

#[cfg(test)]
#[path = "handoff_coordinator_tests.rs"]
mod tests;
