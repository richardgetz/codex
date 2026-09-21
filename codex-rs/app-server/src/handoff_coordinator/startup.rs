use super::HandoffCoordinator;
use super::HandoffJournal;
use crate::error_code::invalid_request;
use codex_app_server_protocol::JSONRPCErrorError;
use std::time::Duration;
use tokio::sync::MutexGuard;
use tokio::time::timeout;

const STARTUP_RECOVERY_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StartupRecoveryState {
    Unknown,
    Ready,
    Pending,
    Unavailable,
}

impl HandoffCoordinator {
    /// Invalidate a cached startup probe as soon as a new durable handoff begins. Any request
    /// arriving after a failed handoff must re-read the journal instead of trusting a prior Ready
    /// result from before the handoff existed.
    pub(super) async fn invalidate_startup_recovery_state(&self) {
        *self.startup_recovery_state.lock().await = StartupRecoveryState::Unknown;
    }

    /// Fence mutating requests while an unfinished handoff needs recovery.
    ///
    /// The probe is cached after the first request so normal traffic does not turn journal
    /// inspection into a polling loop. Status, reads, and the explicit handoff recovery route
    /// remain available while the replacement graph is being restored.
    pub(crate) async fn guard_request_method(&self, method: &str) -> Result<(), JSONRPCErrorError> {
        if recovery_read_method(method) {
            return Ok(());
        }

        if !self.active.lock().await.is_empty() {
            return Err(recovery_pending_error());
        }

        match self.startup_recovery_state().await {
            StartupRecoveryState::Ready => Ok(()),
            StartupRecoveryState::Unknown
            | StartupRecoveryState::Pending
            | StartupRecoveryState::Unavailable => Err(recovery_pending_error()),
        }
    }

    /// Serialize mutating request execution with handoff preparation/recovery. The initial
    /// dispatch probe runs before a request enters its per-connection queue, so this second
    /// operation lock and guard check close the race where a request passed while preparation was
    /// still acquiring its barrier.
    pub(crate) async fn acquire_request_operation(
        &self,
        method: &str,
    ) -> Option<MutexGuard<'_, ()>> {
        if recovery_read_method(method)
            || matches!(method, "thread/handoff/prepare" | "thread/handoff/recover")
        {
            None
        } else {
            Some(self.operation.lock().await)
        }
    }

    /// Re-read the durable handoff set after an explicit recovery attempt.
    pub(crate) async fn refresh_startup_recovery_state(&self) {
        let next = match timeout(
            STARTUP_RECOVERY_PROBE_TIMEOUT,
            HandoffJournal::load_pending(&self.codex_home),
        )
        .await
        {
            Ok(Ok(journals)) if journals.iter().any(HandoffJournal::requires_recovery) => {
                StartupRecoveryState::Pending
            }
            Ok(Ok(_)) => StartupRecoveryState::Ready,
            Ok(Err(_)) | Err(_) => StartupRecoveryState::Unavailable,
        };
        *self.startup_recovery_state.lock().await = next;
    }

    async fn startup_recovery_state(&self) -> StartupRecoveryState {
        {
            let state = self.startup_recovery_state.lock().await;
            if *state != StartupRecoveryState::Unknown {
                return *state;
            }
        }

        let next = match timeout(
            STARTUP_RECOVERY_PROBE_TIMEOUT,
            HandoffJournal::load_pending(&self.codex_home),
        )
        .await
        {
            Ok(Ok(journals)) if journals.iter().any(HandoffJournal::requires_recovery) => {
                StartupRecoveryState::Pending
            }
            Ok(Ok(_)) => StartupRecoveryState::Ready,
            Ok(Err(_)) | Err(_) => StartupRecoveryState::Unavailable,
        };
        let mut state = self.startup_recovery_state.lock().await;
        if *state == StartupRecoveryState::Unknown {
            *state = next;
        }
        *state
    }
}

fn recovery_read_method(method: &str) -> bool {
    method == "thread/handoff/recover"
        || method == "server/diagnostics"
        || method == "thread/search"
        || method == "thread/searchOccurrences"
        || method == "thread/realtime/listVoices"
        || method.ends_with("/read")
        || method.ends_with("/list")
        || method.ends_with("/status")
}

fn recovery_pending_error() -> JSONRPCErrorError {
    invalid_request(
        "app-server recovery is pending; only reads and handoff recovery are available; inspect thread/handoff/status or call thread/handoff/recover",
    )
}
