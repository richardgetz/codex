//! Shares the root automatic usage-resume switch across a loaded agent tree.

use super::AgentControl;
use crate::session::new_submission_id;
use crate::session::session::Session;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadUsagePolicy;
use std::sync::Arc;
use tokio::sync::MutexGuard;

impl AgentControl {
    /// Returns the root's latest automatic usage-resume setting.
    pub(crate) fn root_usage_auto_resume(&self) -> bool {
        self.root_usage_policy().auto_resume
    }

    /// Returns the root's latest usage policy for newly spawned descendants.
    pub(crate) fn root_usage_policy(&self) -> ThreadUsagePolicy {
        **self.root_usage_policy.load()
    }

    /// Publishes the root's automatic usage-resume setting for future children.
    pub(crate) fn set_root_usage_auto_resume(&self, enabled: bool) {
        let mut policy = self.root_usage_policy();
        policy.auto_resume = enabled;
        self.set_root_usage_policy(policy);
    }

    /// Publishes the complete root usage policy for future children.
    pub(crate) fn set_root_usage_policy(&self, policy: ThreadUsagePolicy) {
        self.root_usage_policy.store(Arc::new(policy));
    }

    /// Synchronizes a root usage-resume toggle to loaded thread-spawn descendants.
    ///
    /// No new or restarted turns are created by this propagation. A parked usage wait reads the
    /// live policy, while an already admitted request keeps its captured settings.
    pub(crate) async fn propagate_root_usage_auto_resume(&self) {
        let _propagation_guard = self.root_usage_auto_resume_propagation.lock().await;
        let enabled = self.root_usage_auto_resume();
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
            let Some(thread_settings) = thread.session.apply_root_usage_auto_resume(enabled).await
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

    /// Wakes usage-paused work in a loaded thread-spawn subtree.
    ///
    /// The returned count includes only waits that were active when the request was made. The
    /// request is coalesced by each session, so repeated `/continue` commands do not enqueue
    /// duplicate model turns. A nudge arriving during an in-flight refresh can schedule one
    /// subsequent check.
    pub(crate) async fn request_usage_resume_for_subtree(&self, root_thread_id: ThreadId) -> usize {
        let mut thread_ids = vec![root_thread_id];
        if let Ok(descendant_ids) = self.live_thread_spawn_descendants(root_thread_id).await {
            thread_ids.extend(descendant_ids);
        }
        let Ok(state) = self.upgrade() else {
            return 0;
        };

        let mut requested = 0;
        for thread_id in thread_ids {
            let Ok(thread) = state.get_thread(thread_id).await else {
                continue;
            };
            requested += usize::from(thread.session.request_usage_resume_check());
        }
        requested
    }

    /// Reconciles a child that finished startup after a root toggle was published.
    ///
    /// Session construction reads the root setting before the child is inserted into the live
    /// thread map. Serializing this final reconciliation with root propagation closes the small
    /// window where a toggle could otherwise miss an initializing child.
    pub(crate) async fn reconcile_spawned_usage_auto_resume(
        &self,
        session: &Session,
        inherited_root_auto_resume: Option<bool>,
    ) {
        // A manager-created control has no root session identity or shared root toggle state.
        // Its explicit inherited policy is authoritative; reconciling against the default would
        // erase that policy during test-only and cold-reload startup paths.
        if self.session_id == SessionId::default() {
            return;
        }
        let _update_guard = self.root_usage_auto_resume_update.lock().await;
        let enabled = self.root_usage_auto_resume();
        if inherited_root_auto_resume.is_some_and(|captured| captured == enabled) {
            return;
        }
        let Some(thread_settings) = session.apply_root_usage_auto_resume(enabled).await else {
            return;
        };
        session
            .send_event_raw_without_materializing_rollout(Event {
                id: new_submission_id(),
                msg: EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
                    thread_id: Some(session.thread_id()),
                    thread_settings,
                }),
            })
            .await;
    }

    /// Serializes root usage toggle publication with descendant synchronization.
    pub(crate) async fn lock_root_usage_auto_resume_update(&self) -> MutexGuard<'_, ()> {
        self.root_usage_auto_resume_update.lock().await
    }
}
