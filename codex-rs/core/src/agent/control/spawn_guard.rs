//! Owns a spawned child until its initial input is accepted.

use super::AgentTreeMembership;
use super::AgentTreeTeardownGuard;
use crate::agent::control::HandoffAdmissionGuard;
use crate::thread_manager::ThreadManagerState;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_protocol::ThreadId;
use codex_thread_store::ThreadStoreError;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::warn;

pub(super) struct PendingSpawn {
    state: Arc<ThreadManagerState>,
    pending: Option<(ThreadId, AgentTreeMembership)>,
    edge_write: Option<JoinHandle<()>>,
    handoff_admission: Option<HandoffAdmissionGuard>,
}

impl PendingSpawn {
    pub(super) fn new(
        state: Arc<ThreadManagerState>,
        child: ThreadId,
        handoff_admission: &HandoffAdmissionGuard,
        membership: AgentTreeMembership,
    ) -> Self {
        Self {
            state,
            pending: Some((child, membership)),
            edge_write: None,
            handoff_admission: Some(handoff_admission.fork()),
        }
    }

    pub(super) fn set_edge_write(&mut self, edge_write: JoinHandle<()>) {
        self.edge_write = Some(edge_write);
    }

    pub(super) async fn wait_for_edge(&mut self) {
        if let Some(edge_write) = self.edge_write.as_mut() {
            assert!(
                edge_write.await.is_ok(),
                "spawn edge write task should complete"
            );
        }
        self.edge_write = None;
    }

    pub(super) async fn rollback(mut self) {
        let Some((child, membership)) = self.pending.take() else {
            return;
        };
        // Move cleanup ownership before awaiting so cancellation detaches the cleanup task instead
        // of dropping the child ID after this guard has relinquished it.
        let cleanup = spawn_pending_spawn_cleanup(
            Arc::clone(&self.state),
            child,
            self.edge_write.take(),
            membership,
            self.handoff_admission.take(),
        );
        if let Err(error) = cleanup.await {
            warn!("failed to finish cancelled child spawn cleanup: {error}");
        }
    }

    pub(super) fn disarm(mut self) -> AgentTreeMembership {
        let Some((_child, membership)) = self.pending.take() else {
            unreachable!("pending spawn must own agent-tree membership");
        };
        membership
    }
}

impl Drop for PendingSpawn {
    fn drop(&mut self) {
        let Some((child, membership)) = self.pending.take() else {
            return;
        };
        let state = Arc::clone(&self.state);
        let edge_write = self.edge_write.take();
        drop(spawn_pending_spawn_cleanup(
            state,
            child,
            edge_write,
            membership,
            self.handoff_admission.take(),
        ));
    }
}

fn spawn_pending_spawn_cleanup(
    state: Arc<ThreadManagerState>,
    child: ThreadId,
    edge_write: Option<JoinHandle<()>>,
    membership: AgentTreeMembership,
    handoff_admission: Option<HandoffAdmissionGuard>,
) -> JoinHandle<()> {
    let teardown = membership.into_teardown_guard();
    tokio::spawn(async move {
        let _handoff_admission = handoff_admission;
        if let Some(thread) = state.remove_thread(&child).await {
            if let Err(error) = thread.shutdown_and_wait().await {
                teardown.record_shutdown_failure();
                warn!("failed to stop cancelled child spawn: {error}");
            }
            if let Some(live_thread) = thread.session.live_thread() {
                match live_thread.discard().await {
                    Ok(()) | Err(ThreadStoreError::ThreadNotFound { .. }) => {}
                    Err(error) => {
                        teardown.record_shutdown_failure();
                        warn!("failed to discard cancelled child spawn: {error}");
                    }
                }
            }
        }
        // A pending Open write must finish before cleanup writes Closed.
        if let Some(edge_write) = edge_write {
            let _ = edge_write.await;
        }
        if let Some(store) = state.agent_graph_store()
            && let Err(error) = store
                .set_thread_spawn_edge_status(child, ThreadSpawnEdgeStatus::Closed)
                .await
        {
            teardown.record_shutdown_failure();
            warn!("failed to close cancelled child spawn edge: {error}");
        }
        teardown.complete();
    })
}
