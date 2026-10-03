//! Owns a spawned child until its initial input is accepted.

use crate::agent::control::HandoffAdmissionGuard;
use crate::thread_manager::ThreadManagerState;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_protocol::ThreadId;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::warn;

pub(super) struct PendingSpawn {
    state: Arc<ThreadManagerState>,
    child: Option<ThreadId>,
    edge_write: Option<JoinHandle<()>>,
    handoff_admission: Option<HandoffAdmissionGuard>,
}

impl PendingSpawn {
    pub(super) fn new(
        state: Arc<ThreadManagerState>,
        child: ThreadId,
        handoff_admission: &HandoffAdmissionGuard,
    ) -> Self {
        Self {
            state,
            child: Some(child),
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

    pub(super) fn disarm(mut self) {
        self.child = None;
    }

    pub(super) async fn rollback(mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        // Move cleanup ownership before awaiting so cancellation detaches the cleanup task instead
        // of dropping the child ID after this guard has relinquished it.
        let cleanup = spawn_pending_spawn_cleanup(
            Arc::clone(&self.state),
            child,
            self.edge_write.take(),
            self.handoff_admission.take(),
        );
        if let Err(error) = cleanup.await {
            warn!("failed to finish cancelled child spawn cleanup: {error}");
        }
    }
}

impl Drop for PendingSpawn {
    fn drop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        let handoff_admission = self.handoff_admission.take();
        let state = Arc::clone(&self.state);
        let edge_write = self.edge_write.take();
        drop(spawn_pending_spawn_cleanup(
            state,
            child,
            edge_write,
            handoff_admission,
        ));
    }
}

fn spawn_pending_spawn_cleanup(
    state: Arc<ThreadManagerState>,
    child: ThreadId,
    edge_write: Option<JoinHandle<()>>,
    handoff_admission: Option<HandoffAdmissionGuard>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let _handoff_admission = handoff_admission;
        cleanup_pending_spawn(state, child, edge_write).await;
    })
}

async fn cleanup_pending_spawn(
    state: Arc<ThreadManagerState>,
    child: ThreadId,
    edge_write: Option<JoinHandle<()>>,
) {
    if let Some(thread) = state.remove_thread(&child).await {
        if let Err(error) = thread.shutdown_and_wait().await {
            warn!("failed to stop cancelled child spawn: {error}");
        }
        if let Some(live_thread) = thread.session.live_thread()
            && let Err(error) = live_thread.discard().await
        {
            warn!("failed to discard cancelled child spawn: {error}");
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
        warn!("failed to close cancelled child spawn edge: {error}");
    }
}
