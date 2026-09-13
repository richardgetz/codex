use codex_app_server_protocol::ServerLifecyclePhase;
use codex_app_server_protocol::ServerLifecycleReadResponse;
use codex_app_server_protocol::ServerLifecycleUpdatedNotification;
use codex_app_server_protocol::ServerNotification;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use uuid::Uuid;

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::transport::ConnectionState;

pub(crate) const NEW_WORK_REJECTED_MESSAGE: &str =
    "app-server is draining for restart; retry new work after it reconnects";

const LIFECYCLE_NOTIFICATION_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// Deliver a lifecycle transition to opted-in clients before shutdown tears
/// down their writers. This is a delivery attempt only; a client may still
/// disconnect before consuming the frame.
pub(crate) async fn send_lifecycle_notification_and_wait(
    sender: &OutgoingMessageSender,
    connections: &HashMap<ConnectionId, ConnectionState>,
    notification: ServerLifecycleUpdatedNotification,
) {
    let connection_ids = connections
        .iter()
        .filter_map(|(connection_id, connection_state)| {
            (connection_state.session.initialized()
                && connection_state.session.experimental_api_enabled())
                .then_some(*connection_id)
        })
        .collect::<Vec<_>>();

    let deliveries = futures::future::join_all(connection_ids.into_iter().map(|connection_id| {
        let notification = notification.clone();
        async move {
            let delivered = tokio::time::timeout(
                LIFECYCLE_NOTIFICATION_WRITE_TIMEOUT,
                sender.send_server_notification_to_connection_and_wait(
                    connection_id,
                    ServerNotification::ServerLifecycleUpdated(notification),
                ),
            )
            .await
            .unwrap_or(false);
            (connection_id, delivered)
        }
    }))
    .await;

    for (connection_id, delivered) in deliveries {
        if !delivered {
            tracing::warn!(
                ?connection_id,
                "lifecycle notification delivery did not reach the client writer"
            );
        }
    }
}

/// Shared process-local state used to observe graceful app-server restarts.
///
/// The state intentionally describes only this daemon process. It is not a
/// durable thread handoff or a claim that external commands and pending client
/// callbacks can be resumed after a restart.
#[derive(Clone)]
pub(crate) struct ServerLifecycle {
    snapshot: Arc<RwLock<LifecycleSnapshot>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LifecycleSnapshot {
    daemon_instance_id: String,
    phase: ServerLifecyclePhase,
    transition_id: Option<String>,
    running_assistant_turns: u32,
}

impl ServerLifecycle {
    pub(crate) fn new() -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(LifecycleSnapshot {
                daemon_instance_id: Uuid::new_v4().to_string(),
                phase: ServerLifecyclePhase::Ready,
                transition_id: None,
                running_assistant_turns: 0,
            })),
        }
    }

    pub(crate) fn read(&self) -> ServerLifecycleReadResponse {
        self.snapshot().into_read_response()
    }

    pub(crate) fn begin_drain(&self) -> Option<LifecycleSnapshot> {
        self.update_snapshot(|snapshot| {
            if snapshot.phase != ServerLifecyclePhase::Ready {
                return false;
            }
            snapshot.phase = ServerLifecyclePhase::Draining;
            snapshot.transition_id = Some(Uuid::new_v4().to_string());
            true
        })
    }

    pub(crate) fn force_drain(&self) -> Option<LifecycleSnapshot> {
        self.update_snapshot(|snapshot| {
            if snapshot.phase != ServerLifecyclePhase::Draining {
                return false;
            }
            snapshot.phase = ServerLifecyclePhase::Forced;
            true
        })
    }

    pub(crate) fn update_running_assistant_turns(
        &self,
        running_assistant_turns: usize,
    ) -> Option<LifecycleSnapshot> {
        let running_assistant_turns = u32::try_from(running_assistant_turns).unwrap_or(u32::MAX);
        self.update_snapshot(|snapshot| {
            if snapshot.running_assistant_turns == running_assistant_turns {
                return false;
            }
            snapshot.running_assistant_turns = running_assistant_turns;
            snapshot.phase != ServerLifecyclePhase::Ready
        })
    }

    pub(crate) fn rejects_new_work(&self, method: &str) -> bool {
        let phase = self
            .snapshot
            .read()
            .expect("server lifecycle lock should not be poisoned")
            .phase;
        phase != ServerLifecyclePhase::Ready && rejects_new_work_method(method)
    }

    fn snapshot(&self) -> LifecycleSnapshot {
        self.snapshot
            .read()
            .expect("server lifecycle lock should not be poisoned")
            .clone()
    }

    fn update_snapshot(
        &self,
        update: impl FnOnce(&mut LifecycleSnapshot) -> bool,
    ) -> Option<LifecycleSnapshot> {
        let mut snapshot = self
            .snapshot
            .write()
            .expect("server lifecycle lock should not be poisoned");
        if !update(&mut snapshot) {
            return None;
        }
        Some(snapshot.clone())
    }
}

impl LifecycleSnapshot {
    pub(crate) fn into_read_response(self) -> ServerLifecycleReadResponse {
        ServerLifecycleReadResponse {
            daemon_instance_id: self.daemon_instance_id,
            phase: self.phase,
            transition_id: self.transition_id,
            running_assistant_turns: self.running_assistant_turns,
        }
    }

    pub(crate) fn into_updated_notification(self) -> ServerLifecycleUpdatedNotification {
        ServerLifecycleUpdatedNotification {
            daemon_instance_id: self.daemon_instance_id,
            phase: self.phase,
            transition_id: self.transition_id,
            running_assistant_turns: self.running_assistant_turns,
        }
    }
}

/// Requests that could admit new model or external work after a graceful
/// restart has started. Existing turn resolution, interruption, approval, and
/// read requests remain available while the daemon drains.
pub(crate) fn rejects_new_work_method(method: &str) -> bool {
    matches!(
        method,
        "thread/start"
            | "thread/resume"
            | "thread/fork"
            | "thread/queue/add"
            | "thread/queue/start"
            | "thread/usage/resume"
            | "thread/compact/start"
            | "thread/realtime/start"
            | "thread/realtime/appendAudio"
            | "thread/realtime/appendText"
            | "thread/realtime/appendSpeech"
            | "thread/goal/set"
            | "thread/activity/continue"
            | "thread/inject_items"
            | "thread/shellCommand"
            | "mcpServer/event/stream/start"
            | "mcpServer/tool/call"
            | "turn/start"
            | "turn/steer"
            | "review/start"
            | "command/exec"
            | "process/spawn"
            | "windowsSandbox/setupStart"
    )
}

#[cfg(test)]
#[path = "server_lifecycle_tests.rs"]
mod tests;
