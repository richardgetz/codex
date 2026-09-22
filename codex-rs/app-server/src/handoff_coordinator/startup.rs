use super::HandoffCoordinator;
use super::HandoffJournal;
use crate::error_code::invalid_request;
use codex_app_server_protocol::JSONRPCErrorError;
use std::time::Duration;
use tokio::sync::MutexGuard;
use tokio::time::timeout;

const STARTUP_RECOVERY_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PENDING_HANDOFF_IDS_IN_ERROR: usize = 3;
const MAX_HANDOFF_ID_BYTES_IN_ERROR: usize = 64;

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
        self.cache_pending_handoff_ids(Vec::new()).await;
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
            return Err(self.recovery_pending_error().await);
        }

        match self.startup_recovery_state().await {
            StartupRecoveryState::Ready => Ok(()),
            StartupRecoveryState::Unknown
            | StartupRecoveryState::Pending
            | StartupRecoveryState::Unavailable => Err(self.recovery_pending_error().await),
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
            Ok(Ok(journals)) => {
                let has_pending = journals.iter().any(HandoffJournal::requires_recovery);
                let pending_ids = pending_handoff_ids(&journals);
                self.cache_pending_handoff_ids(pending_ids.clone()).await;
                if has_pending {
                    StartupRecoveryState::Pending
                } else {
                    StartupRecoveryState::Ready
                }
            }
            Ok(Err(_)) | Err(_) => {
                self.cache_pending_handoff_ids(Vec::new()).await;
                StartupRecoveryState::Unavailable
            }
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
            Ok(Ok(journals)) => {
                let has_pending = journals.iter().any(HandoffJournal::requires_recovery);
                let pending_ids = pending_handoff_ids(&journals);
                self.cache_pending_handoff_ids(pending_ids.clone()).await;
                if has_pending {
                    StartupRecoveryState::Pending
                } else {
                    StartupRecoveryState::Ready
                }
            }
            Ok(Err(_)) | Err(_) => {
                self.cache_pending_handoff_ids(Vec::new()).await;
                StartupRecoveryState::Unavailable
            }
        };
        let mut state = self.startup_recovery_state.lock().await;
        if *state == StartupRecoveryState::Unknown {
            *state = next;
        }
        *state
    }

    async fn cache_pending_handoff_ids(&self, ids: Vec<String>) {
        *self.pending_handoff_ids.lock().await = ids;
    }

    async fn recovery_pending_error(&self) -> JSONRPCErrorError {
        let mut ids = self.pending_handoff_ids.lock().await.clone();
        if ids.is_empty() {
            ids = self
                .active
                .lock()
                .await
                .keys()
                .filter(|id| is_displayable_handoff_id(id))
                .cloned()
                .collect();
            ids.sort();
            ids.truncate(MAX_PENDING_HANDOFF_IDS_IN_ERROR);
        }
        recovery_pending_error(&ids)
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

fn pending_handoff_ids(journals: &[HandoffJournal]) -> Vec<String> {
    let mut ids = journals
        .iter()
        .filter(|journal| journal.requires_recovery())
        .filter_map(|journal| {
            is_displayable_handoff_id(&journal.handoff_id).then(|| journal.handoff_id.clone())
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids.truncate(MAX_PENDING_HANDOFF_IDS_IN_ERROR);
    ids
}

fn recovery_pending_error(handoff_ids: &[String]) -> JSONRPCErrorError {
    if handoff_ids.is_empty() {
        return invalid_request(
            "app-server recovery is pending; only reads and handoff recovery are available; inspect thread/handoff/status or call thread/handoff/recover",
        );
    }
    invalid_request(format!(
        "app-server recovery is pending for handoff(s) {}; inspect thread/handoff/status, then call thread/handoff/recover with a handoffId; no new turn is admitted",
        handoff_ids.join(", "),
    ))
}

fn is_displayable_handoff_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_HANDOFF_ID_BYTES_IN_ERROR
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}
