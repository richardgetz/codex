//! Event-driven Lead/Worker activity projection for the chat status row.
//!
//! Activity notifications are ephemeral and arrive for every loaded thread in
//! a team. The app keeps a per-root cache for loaded threads, derives counts from the
//! root id carried by each notification, and sends only the selected session's
//! compact projection to `ChatWidget`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;

use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadActivity;
use codex_app_server_protocol::ThreadActivityUpdatedNotification;
use codex_app_server_protocol::ThreadActivityWaitReason;
use codex_app_server_protocol::ThreadPauseState;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SubAgentSource;

use crate::chatwidget::TeamActivityStatus;
use crate::chatwidget::TeamPauseState as UiPauseState;
use crate::chatwidget::TeamRoleActivity;

/// Keep a Worker visibly working through short routine coordination waits.
///
/// This is a display-only grace period. It does not alter backend activity, Lead oversight, or
/// any admission/polling behavior. The timer starts only on a real Working -> ordinary Waiting
/// transition and is never extended by repeated Waiting notifications.
pub(super) const ORDINARY_WAITING_GRACE: Duration = Duration::from_secs(30);
/// Keep an event-admitted parent edge briefly while persisted overview metadata catches up.
///
/// Activity updates can trigger several metadata syncs before a newly spawned thread appears in
/// the overview, so this is time-based rather than refresh-count based. Once the window expires,
/// an omitted edge is treated as stale and late activity is rejected.
pub(super) const LOCAL_PARENT_ADMISSION_GRACE: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
struct ThreadActivityEntry {
    root_thread_id: ThreadId,
    parent_thread_id: Option<ThreadId>,
    parent_thread_id_known: bool,
    activity: ThreadActivity,
    wait_reason: Option<ThreadActivityWaitReason>,
    pause_state: ThreadPauseState,
    in_flight_operations: u32,
    ordinary_waiting_since: Option<Instant>,
}

#[derive(Debug, Default)]
pub(super) struct TeamActivityProjection {
    entries: HashMap<ThreadId, ThreadActivityEntry>,
    /// Parent metadata is supplied by existing ThreadStarted/thread-read records. Activity
    /// notifications intentionally stay unchanged, so this remains a client-side classification.
    parent_thread_ids: HashMap<ThreadId, Option<ThreadId>>,
    /// Parent edges admitted from collab activity are provisional until the overview includes the
    /// corresponding thread. The value records when the edge was admitted; stale edges expire
    /// after a bounded grace window instead of being retained forever.
    locally_admitted_parent_ids: HashMap<ThreadId, Instant>,
    /// Terminal notifications can race detached activity updates. Keep barriers only for IDs in
    /// the currently admitted loaded tree; removed/unloaded children are rejected as unknown
    /// until a fresh ThreadStarted event admits them again.
    terminal_threads: HashSet<ThreadId>,
    /// A root close/delete/archive can race detached updates before any root metadata is loaded.
    /// This single selected-root barrier covers that uncached case without retaining old IDs.
    closed_root: Option<ThreadId>,
    selected_root: Option<ThreadId>,
}

impl TeamActivityProjection {
    pub(super) fn observe(&mut self, notification: &ThreadActivityUpdatedNotification) {
        self.observe_at(notification, Instant::now());
    }

    fn observe_at(&mut self, notification: &ThreadActivityUpdatedNotification, now: Instant) {
        self.expire_local_parent_edges(now);
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
        let is_root_activity = thread_id == root_thread_id;
        if !is_root_activity && !self.parent_thread_ids.contains_key(&thread_id) {
            return;
        }
        let parent_thread_id = self.parent_thread_ids.get(&thread_id).copied().flatten();
        let parent_thread_id_known = self.parent_thread_ids.contains_key(&thread_id);
        if self.closed_root == Some(root_thread_id) || self.terminal_threads.contains(&thread_id) {
            // Activity completion can race the detached guard's final update. Preserve pause
            // updates for a completed/idle thread, but never let stale activity or grace state
            // bring it back into the aggregate counts.
            let pause_state = self
                .entries
                .get(&thread_id)
                .map(|entry| entry.pause_state)
                .and_then(|pause_state| {
                    (notification.activity != ThreadActivity::Idle
                        && match pause_state {
                            ThreadPauseState::Paused => {
                                notification.pause_state != ThreadPauseState::Paused
                            }
                            ThreadPauseState::Pausing => {
                                notification.pause_state == ThreadPauseState::Running
                            }
                            ThreadPauseState::Running => false,
                        })
                    .then_some(pause_state)
                })
                .unwrap_or(notification.pause_state);
            self.entries.insert(
                thread_id,
                ThreadActivityEntry {
                    root_thread_id,
                    parent_thread_id,
                    parent_thread_id_known,
                    activity: ThreadActivity::Idle,
                    wait_reason: None,
                    pause_state,
                    in_flight_operations: 0,
                    ordinary_waiting_since: None,
                },
            );
            return;
        }
        if !is_root_activity
            && notification.activity != ThreadActivity::Idle
            && self
                .locally_admitted_parent_ids
                .contains_key(&thread_id)
        {
            // A real Worker can remain active without ever producing ThreadStarted metadata.
            // Keep its provisional edge alive while activity snapshots continue to arrive.
            self.locally_admitted_parent_ids
                .insert(thread_id, now);
        }
        let previous = self.entries.get(&thread_id);
        let ordinary_waiting = is_ordinary_waiting(notification.activity, notification.wait_reason);
        let ordinary_waiting_since = if ordinary_waiting
            && previous.is_some_and(|entry| {
                entry.pause_state == ThreadPauseState::Running
                    && notification.pause_state == ThreadPauseState::Running
                    && (entry.activity == ThreadActivity::Working
                        || (entry.activity == ThreadActivity::Waiting
                            && entry.ordinary_waiting_since.is_some()))
            }) {
            previous
                .and_then(|entry| entry.ordinary_waiting_since)
                .or(Some(now))
        } else {
            None
        };
        self.entries.insert(
            thread_id,
            ThreadActivityEntry {
                root_thread_id,
                parent_thread_id,
                parent_thread_id_known,
                activity: notification.activity,
                wait_reason: notification.wait_reason,
                pause_state: notification.pause_state,
                in_flight_operations: notification.in_flight_operations,
                ordinary_waiting_since,
            },
        );
    }

    /// Cache parent metadata from an existing app-server thread record.
    pub(super) fn observe_thread_metadata(&mut self, thread: &Thread) {
        let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
            return;
        };
        let parent_thread_id = thread
            .parent_thread_id
            .as_deref()
            .and_then(|parent| ThreadId::from_string(parent).ok())
            .or(match &thread.source {
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id, ..
                }) => Some(*parent_thread_id),
            _ => None,
        });
        self.set_thread_parent(thread_id, parent_thread_id);
        self.locally_admitted_parent_ids.remove(&thread_id);
    }

    /// Cache a parent edge for a thread and apply it to an activity entry already seen.
    pub(super) fn observe_thread_parent(
        &mut self,
        thread_id: ThreadId,
        parent_thread_id: Option<ThreadId>,
    ) {
        self.set_thread_parent(thread_id, parent_thread_id);
        self.locally_admitted_parent_ids
            .insert(thread_id, Instant::now());
    }

    fn set_thread_parent(&mut self, thread_id: ThreadId, parent_thread_id: Option<ThreadId>) {
        self.start_thread(thread_id);
        self.parent_thread_ids.insert(thread_id, parent_thread_id);
        if let Some(entry) = self.entries.get_mut(&thread_id) {
            entry.parent_thread_id = parent_thread_id;
            entry.parent_thread_id_known = true;
        }
    }

    /// Clear a terminal tombstone when a genuinely new turn starts for this thread.
    pub(super) fn start_thread(&mut self, thread_id: ThreadId) {
        self.terminal_threads.remove(&thread_id);
        if self.closed_root == Some(thread_id) {
            self.closed_root = None;
        }
    }

    /// Replace parent metadata with the currently selected loaded tree.
    pub(super) fn replace_thread_metadata(
        &mut self,
        selected_root: Option<ThreadId>,
        metadata: impl IntoIterator<Item = (ThreadId, Option<ThreadId>)>,
    ) {
        let now = Instant::now();
        self.expire_local_parent_edges(now);
        let preserve_existing = selected_root.is_some() && self.selected_root == selected_root;
        if !preserve_existing {
            self.terminal_threads.clear();
            self.closed_root = None;
        }
        self.selected_root = selected_root;
        let previous_parent_thread_ids = std::mem::take(&mut self.parent_thread_ids);
        let previous_locally_admitted_parent_ids =
            std::mem::take(&mut self.locally_admitted_parent_ids);
        let mut parent_thread_ids: HashMap<_, _> = metadata.into_iter().collect();
        let mut locally_admitted_parent_ids = HashMap::new();
        if preserve_existing {
            // Collab spawn notifications can admit a parent edge before the next overview
            // refresh sees the new thread. Keep each locally admitted edge for a bounded grace
            // window while letting fresh metadata replace stale relationships. Once the window
            // expires, an omitted edge is treated as authoritative and dropped.
            for (thread_id, admitted_at) in previous_locally_admitted_parent_ids {
                if parent_thread_ids.contains_key(&thread_id)
                    || now.saturating_duration_since(admitted_at) >= LOCAL_PARENT_ADMISSION_GRACE
                {
                    continue;
                }
                if let Some(parent_thread_id) = previous_parent_thread_ids.get(&thread_id) {
                    parent_thread_ids.insert(thread_id, *parent_thread_id);
                    locally_admitted_parent_ids.insert(thread_id, admitted_at);
                }
            }
        }
        self.parent_thread_ids = parent_thread_ids;
        self.locally_admitted_parent_ids = locally_admitted_parent_ids;
        if let Some(root_thread_id) = selected_root {
            self.parent_thread_ids.insert(root_thread_id, None);
            self.locally_admitted_parent_ids.remove(&root_thread_id);
            let admitted_thread_ids: HashSet<_> = self.parent_thread_ids.keys().copied().collect();
            self.entries.retain(|thread_id, entry| {
                entry.root_thread_id == root_thread_id && admitted_thread_ids.contains(thread_id)
            });
            self.terminal_threads
                .retain(|thread_id| admitted_thread_ids.contains(thread_id));
        } else {
            self.entries.clear();
            self.parent_thread_ids.clear();
            self.locally_admitted_parent_ids.clear();
            self.terminal_threads.clear();
            self.closed_root = None;
        }
        for (thread_id, entry) in &mut self.entries {
            if let Some(parent_thread_id) = self.parent_thread_ids.get(thread_id) {
                entry.parent_thread_id = *parent_thread_id;
                entry.parent_thread_id_known = true;
            } else {
                entry.parent_thread_id = None;
                entry.parent_thread_id_known = false;
            }
        }
    }

    /// Clear all event-derived activity and metadata after a thread reset or reconnect.
    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.parent_thread_ids.clear();
        self.locally_admitted_parent_ids.clear();
        self.terminal_threads.clear();
        self.closed_root = None;
        self.selected_root = None;
    }

    /// Mark terminal activity immediately while retaining root metadata for any other workers.
    pub(super) fn finish_thread(&mut self, thread_id: ThreadId) {
        if self.parent_thread_ids.contains_key(&thread_id)
            || self.entries.contains_key(&thread_id)
            || self.selected_root == Some(thread_id)
        {
            self.terminal_threads.insert(thread_id);
        }
        if let Some(entry) = self.entries.get_mut(&thread_id) {
            entry.activity = ThreadActivity::Idle;
            entry.wait_reason = None;
            entry.ordinary_waiting_since = None;
            entry.in_flight_operations = 0;
        }
    }

    /// Expire display-only grace periods against the UI's monotonic clock.
    pub(super) fn expire_waiting_graces(&mut self, now: Instant) {
        for entry in self.entries.values_mut() {
            if entry
                .ordinary_waiting_since
                .is_some_and(|since| now.saturating_duration_since(since) >= ORDINARY_WAITING_GRACE)
            {
                entry.ordinary_waiting_since = None;
            }
        }
    }

    fn expire_local_parent_edges(&mut self, now: Instant) {
        let stale_thread_ids: Vec<_> = self
            .locally_admitted_parent_ids
            .iter()
            .filter_map(|(thread_id, admitted_at)| {
                (now.saturating_duration_since(*admitted_at) >= LOCAL_PARENT_ADMISSION_GRACE
                    && self
                        .entries
                        .get(thread_id)
                        .is_none_or(|entry| entry.activity == ThreadActivity::Idle))
                    .then_some(*thread_id)
            })
            .collect();
        for thread_id in stale_thread_ids {
            self.remove_thread(thread_id);
        }
    }

    /// Return the next UI redraw deadline for an unexpired grace period.
    pub(super) fn next_waiting_grace_deadline(&self, now: Instant) -> Option<Instant> {
        self.entries
            .values()
            .filter_map(|entry| {
                entry
                    .ordinary_waiting_since
                    .map(|since| since + ORDINARY_WAITING_GRACE)
            })
            .filter(|deadline| *deadline > now)
            .min()
    }

    /// Return the cleanup deadline for the oldest provisional parent edge.
    pub(super) fn next_local_parent_admission_deadline(&self, now: Instant) -> Option<Instant> {
        self.locally_admitted_parent_ids
            .values()
            .map(|admitted_at| *admitted_at + LOCAL_PARENT_ADMISSION_GRACE)
            .filter(|deadline| *deadline > now)
            .min()
    }

    pub(super) fn remove_thread(&mut self, thread_id: ThreadId) {
        let entry_root_thread_id = self
            .entries
            .get(&thread_id)
            .map(|entry| entry.root_thread_id);
        let is_root = entry_root_thread_id == Some(thread_id)
            || self
                .parent_thread_ids
                .get(&thread_id)
                .is_some_and(Option::is_none)
            || self.selected_root == Some(thread_id)
            || self
                .entries
                .values()
                .any(|entry| entry.root_thread_id == thread_id);
        let mut removed = HashSet::from([thread_id]);
        loop {
            let descendants: Vec<_> = self
                .parent_thread_ids
                .iter()
                .filter_map(|(candidate, parent)| {
                    (!removed.contains(candidate)
                        && parent
                            .as_ref()
                            .is_some_and(|parent| removed.contains(parent)))
                    .then_some(*candidate)
                })
                .collect();
            if descendants.is_empty() {
                break;
            }
            removed.extend(descendants);
        }
        let admitted_thread_ids: HashSet<_> = self.parent_thread_ids.keys().copied().collect();
        self.entries.retain(|candidate, entry| {
            !removed.contains(candidate) && (!is_root || entry.root_thread_id != thread_id)
        });
        self.parent_thread_ids
            .retain(|candidate, _| !removed.contains(candidate));
        self.locally_admitted_parent_ids
            .retain(|candidate, _| !removed.contains(candidate));
        if is_root {
            self.entries
                .retain(|_, entry| entry.root_thread_id != thread_id);
            if self.selected_root == Some(thread_id) {
                self.closed_root = Some(thread_id);
            }
        }
        for candidate in removed {
            if admitted_thread_ids.contains(&candidate) || self.selected_root == Some(candidate) {
                self.terminal_threads.insert(candidate);
            }
        }
    }

    pub(super) fn status_for_root(
        &self,
        root_thread_id: ThreadId,
        worker_max_concurrent: Option<usize>,
    ) -> Option<TeamActivityStatus> {
        self.status_for_root_at(root_thread_id, worker_max_concurrent, Instant::now())
    }

    pub(super) fn status_for_root_at(
        &self,
        root_thread_id: ThreadId,
        worker_max_concurrent: Option<usize>,
        now: Instant,
    ) -> Option<TeamActivityStatus> {
        // The root entry is authoritative for the session pause state. A descendant event can
        // arrive after a root toggle and must not make a paused tree appear running again.
        let root_entry = self.entries.get(&root_thread_id)?;
        let lead = role_activity(display_activity(root_entry, now));
        let mut workers_working = 0;
        let mut workers_waiting = 0;
        let mut direct_workers = 0;
        let mut subagents = 0;
        let mut in_flight_operations = root_entry.in_flight_operations;
        let mut descendants_draining = false;
        for (_thread_id, entry) in self.entries.iter().filter(|(thread_id, entry)| {
            **thread_id != root_thread_id && entry.root_thread_id == root_thread_id
        }) {
            let activity = display_activity(entry, now);
            match activity {
                ThreadActivity::Working => workers_working += 1,
                ThreadActivity::Waiting => workers_waiting += 1,
                ThreadActivity::Idle => {}
            }
            // Keep the aggregate Team count useful while metadata is still loading, but do not
            // claim an unknown lineage is direct or nested until an existing parent edge arrives.
            if activity != ThreadActivity::Idle && entry.parent_thread_id_known {
                match entry.parent_thread_id {
                    Some(parent_thread_id) if parent_thread_id == root_thread_id => {
                        direct_workers += 1;
                    }
                    Some(_) => subagents += 1,
                    None => {}
                }
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
            direct_workers,
            subagents,
            worker_max_concurrent,
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

fn is_ordinary_waiting(
    activity: ThreadActivity,
    wait_reason: Option<ThreadActivityWaitReason>,
) -> bool {
    activity == ThreadActivity::Waiting
        && matches!(wait_reason, None | Some(ThreadActivityWaitReason::Agents))
}

fn display_activity(entry: &ThreadActivityEntry, now: Instant) -> ThreadActivity {
    if entry.activity == ThreadActivity::Waiting
        && entry.pause_state == ThreadPauseState::Running
        && entry
            .ordinary_waiting_since
            .is_some_and(|since| now.saturating_duration_since(since) < ORDINARY_WAITING_GRACE)
    {
        ThreadActivity::Working
    } else {
        entry.activity
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
    pub(super) fn start_thread_activity(&mut self, thread_id: ThreadId) {
        self.team_activity.start_thread(thread_id);
    }

    pub(super) fn observe_thread_activity(
        &mut self,
        notification: &ThreadActivityUpdatedNotification,
    ) {
        self.sync_team_activity_metadata();
        self.team_activity.observe(notification);
        self.sync_team_activity_status();
    }

    pub(super) fn remove_thread_activity(&mut self, thread_id: ThreadId) {
        self.team_activity.remove_thread(thread_id);
        self.sync_team_activity_status();
    }

    pub(super) fn sync_team_activity_status(&mut self) {
        let now = Instant::now();
        self.sync_team_activity_metadata();
        self.team_activity.expire_waiting_graces(now);
        if let Some(deadline) = self.team_activity.next_waiting_grace_deadline(now) {
            self.chat_widget
                .frame_requester()
                .schedule_frame_in(deadline.saturating_duration_since(now));
        }
        if let Some(deadline) = self
            .team_activity
            .next_local_parent_admission_deadline(now)
        {
            self.chat_widget
                .frame_requester()
                .schedule_frame_in(deadline.saturating_duration_since(now));
        }
        let status = self.primary_thread_id.and_then(|root_thread_id| {
            self.team_activity
                .status_for_root(root_thread_id, self.config.team.worker_max_concurrent)
        });
        self.chat_widget.set_team_activity(status);
    }

    fn sync_team_activity_metadata(&mut self) {
        let Some(primary_thread_id) = self.primary_thread_id else {
            self.team_activity.replace_thread_metadata(None, []);
            return;
        };

        let metadata: HashMap<ThreadId, Option<ThreadId>> = self
            .agents_overview
            .threads
            .values()
            .flatten()
            .filter(|thread| !matches!(thread.status, ThreadStatus::NotLoaded))
            .filter_map(|thread| {
                let thread_id = ThreadId::from_string(&thread.id).ok()?;
                Some((thread_id, thread_parent_thread_id(thread)))
            })
            .collect();
        let mut selected_thread_ids = HashSet::from([primary_thread_id]);
        loop {
            let descendants: Vec<_> = metadata
                .iter()
                .filter_map(|(thread_id, parent_thread_id)| {
                    parent_thread_id
                        .filter(|parent| selected_thread_ids.contains(parent))
                        .and_then(|_| selected_thread_ids.insert(*thread_id).then_some(*thread_id))
                })
                .collect();
            if descendants.is_empty() {
                break;
            }
        }
        let selected_metadata = metadata
            .into_iter()
            .filter(|(thread_id, _)| selected_thread_ids.contains(thread_id))
            .filter(|(thread_id, _)| *thread_id != primary_thread_id)
            .chain(std::iter::once((primary_thread_id, None)));
        self.team_activity
            .replace_thread_metadata(Some(primary_thread_id), selected_metadata);
    }

    pub(super) fn finish_thread_activity(&mut self, thread_id: ThreadId) {
        self.team_activity.finish_thread(thread_id);
        self.sync_team_activity_status();
    }
}

fn thread_parent_thread_id(thread: &Thread) -> Option<ThreadId> {
    thread
        .parent_thread_id
        .as_deref()
        .and_then(|parent| ThreadId::from_string(parent).ok())
        .or(match &thread.source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            }) => Some(*parent_thread_id),
            _ => None,
        })
}

#[cfg(test)]
#[path = "team_activity_tests.rs"]
mod tests;
