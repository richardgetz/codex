//! Shares the root user's selected routing tier across the entire agent tree.

use super::AgentControl;
use crate::session::new_submission_id;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use std::sync::Arc;
use tokio::sync::MutexGuard;

impl AgentControl {
    /// Returns the latest user-selected tier for this root and all its descendants.
    pub(crate) fn root_service_tier(&self) -> Option<String> {
        self.root_service_tier
            .load_full()
            .map(|service_tier| (*service_tier).clone())
    }

    /// Publishes a root-owned tier without mutating individual child sessions.
    pub(crate) fn set_root_service_tier(&self, service_tier: Option<String>) {
        self.root_service_tier.store(service_tier.map(Arc::new));
    }

    /// Serializes a root tier settings commit with its descendant synchronization pass.
    pub(crate) async fn lock_root_service_tier_update(&self) -> MutexGuard<'_, ()> {
        self.root_service_tier_update.lock().await
    }

    /// Synchronizes the latest root tier to loaded thread-spawn descendants.
    ///
    /// New turns already read the shared tier while capturing their step context. This pass keeps
    /// loaded child settings snapshots and client-facing notifications in sync without touching a
    /// turn that has already captured its immutable request settings.
    pub(crate) async fn propagate_root_service_tier(&self) {
        let _propagation_guard = self.root_service_tier_propagation.lock().await;
        let service_tier = self.root_service_tier();
        let root_thread_id = ThreadId::from(self.session_id);
        let Ok(descendant_ids) = self.live_thread_spawn_descendants(root_thread_id).await else {
            return;
        };
        let Ok(state) = self.upgrade() else {
            return;
        };

        for thread_id in descendant_ids {
            let Ok(thread) = state.get_thread(thread_id).await else {
                continue;
            };
            let Some(thread_settings) = thread
                .session
                .apply_root_service_tier(service_tier.clone())
                .await
            else {
                continue;
            };
            thread
                .session
                .send_event_raw_without_materializing_rollout(Event {
                    id: new_submission_id(),
                    msg: EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
                        thread_id: Some(thread_id),
                        thread_settings,
                    }),
                })
                .await;
        }
    }
}
