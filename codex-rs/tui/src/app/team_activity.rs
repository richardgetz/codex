//! Event-driven Lead/Worker activity projection for the chat status row.
//!
//! Activity notifications are ephemeral and arrive for every loaded thread in
//! a team. The app keeps a per-root cache for loaded threads, derives counts from the
//! root id carried by each notification, and sends only the selected session's
//! compact projection to `ChatWidget`.

use std::collections::HashMap;

use codex_app_server_protocol::ThreadActivity;
use codex_app_server_protocol::ThreadActivityUpdatedNotification;
use codex_app_server_protocol::ThreadPauseState;
use codex_protocol::ThreadId;

use crate::chatwidget::TeamActivityStatus;
use crate::chatwidget::TeamPauseState as UiPauseState;
use crate::chatwidget::TeamRoleActivity;

#[derive(Clone, Debug)]
struct ThreadActivityEntry {
    root_thread_id: ThreadId,
    activity: ThreadActivity,
    pause_state: ThreadPauseState,
    in_flight_operations: u32,
}

#[derive(Debug, Default)]
pub(super) struct TeamActivityProjection {
    entries: HashMap<ThreadId, ThreadActivityEntry>,
}

impl TeamActivityProjection {
    pub(super) fn observe(&mut self, notification: &ThreadActivityUpdatedNotification) {
        let Ok(thread_id) = ThreadId::from_string(&notification.thread_id) else {
            tracing::warn!(
                thread_id = %notification.thread_id,
                "ignoring team activity notification with invalid thread_id"
            );
            return;
        };
        let Ok(root_thread_id) = ThreadId::from_string(&notification.root_thread_id) else {
            tracing::warn!(
                root_thread_id = %notification.root_thread_id,
                "ignoring team activity notification with invalid root_thread_id"
            );
            return;
        };
        self.entries.insert(
            thread_id,
            ThreadActivityEntry {
                root_thread_id,
                activity: notification.activity,
                pause_state: notification.pause_state,
                in_flight_operations: notification.in_flight_operations,
            },
        );
    }

    pub(super) fn remove_thread(&mut self, thread_id: ThreadId) {
        let root_thread_id = self
            .entries
            .remove(&thread_id)
            .map(|entry| entry.root_thread_id);
        if root_thread_id == Some(thread_id) {
            self.entries
                .retain(|_, entry| entry.root_thread_id != thread_id);
        }
    }

    pub(super) fn status_for_root(&self, root_thread_id: ThreadId) -> Option<TeamActivityStatus> {
        // The root entry is authoritative for the session pause state. A descendant event can
        // arrive after a root toggle and must not make a paused tree appear running again.
        let root_entry = self.entries.get(&root_thread_id)?;
        let lead = role_activity(root_entry.activity);
        let mut workers_working = 0;
        let mut workers_waiting = 0;
        let mut in_flight_operations = root_entry.in_flight_operations;
        let mut descendants_draining = false;
        for (_thread_id, entry) in self.entries.iter().filter(|(thread_id, entry)| {
            **thread_id != root_thread_id && entry.root_thread_id == root_thread_id
        }) {
            match entry.activity {
                ThreadActivity::Working => workers_working += 1,
                ThreadActivity::Waiting => workers_waiting += 1,
                ThreadActivity::Idle => {}
            }
            descendants_draining |=
                entry.pause_state == ThreadPauseState::Pausing || entry.in_flight_operations > 0;
            in_flight_operations = in_flight_operations.saturating_add(entry.in_flight_operations);
        }
        // The root entry owns the pause intent. Descendants may still be draining after the root
        // reports Paused, so keep the row in Pausing until their in-flight work reaches zero.
        let pause_state = match root_entry.pause_state {
            ThreadPauseState::Paused
                if root_entry.in_flight_operations > 0 || descendants_draining =>
            {
                UiPauseState::Pausing
            }
            pause_state => pause_state.into(),
        };
        // Every other entry carrying the same root id is a direct or nested Worker; a different
        // root is unrelated and must never affect this row.
        let status = TeamActivityStatus {
            lead,
            workers_working,
            workers_waiting,
            pause_state,
            in_flight_operations,
        };
        status.is_visible().then_some(status)
    }
}

fn role_activity(activity: ThreadActivity) -> TeamRoleActivity {
    match activity {
        ThreadActivity::Idle => TeamRoleActivity::Idle,
        ThreadActivity::Working => TeamRoleActivity::Working,
        ThreadActivity::Waiting => TeamRoleActivity::Waiting,
    }
}

impl From<ThreadPauseState> for UiPauseState {
    fn from(value: ThreadPauseState) -> Self {
        match value {
            ThreadPauseState::Running => Self::Running,
            ThreadPauseState::Pausing => Self::Pausing,
            ThreadPauseState::Paused => Self::Paused,
        }
    }
}

impl super::App {
    pub(super) fn observe_thread_activity(
        &mut self,
        notification: &ThreadActivityUpdatedNotification,
    ) {
        self.team_activity.observe(notification);
        self.sync_team_activity_status();
    }

    pub(super) fn remove_thread_activity(&mut self, thread_id: ThreadId) {
        self.team_activity.remove_thread(thread_id);
        self.sync_team_activity_status();
    }

    pub(super) fn sync_team_activity_status(&mut self) {
        let status = self
            .primary_thread_id
            .and_then(|root_thread_id| self.team_activity.status_for_root(root_thread_id));
        self.chat_widget.set_team_activity(status);
    }
}

#[cfg(test)]
#[path = "team_activity_tests.rs"]
mod tests;
