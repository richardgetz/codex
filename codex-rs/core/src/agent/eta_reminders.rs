//! One-shot, owner-routed ETA freshness and overdue reminders.
//!
//! Durable updates arm timers; lifecycle, configuration, and shutdown changes invalidate them.
//! They never infer completion or poll task state.

use super::AgentControl;
use crate::TurnStartOptions;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::context::ContextualUserFragment;
use crate::context::EtaReminderMessage;
use crate::context::ReminderTrigger;
use crate::context::format_reminder;
use chrono::DateTime;
use chrono::Duration as ChronoDuration;
use chrono::Utc;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::protocol::InterAgentCommunication;
use codex_rollout::StateDbHandle;
use codex_state::TaskEstimate;
use codex_state::TaskEstimateStatus;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tracing::warn;

#[derive(Default)]
pub(crate) struct EtaReminderController {
    state: Mutex<EtaReminderState>,
    dispatch: Arc<Mutex<()>>,
}

#[derive(Default)]
struct EtaReminderState {
    next_generation: u64,
    tasks: HashMap<String, EtaReminderEntry>,
}

struct EtaReminderEntry {
    generation: u64,
    task: TaskEstimate,
    freshness_sent: bool,
    overdue_sent: bool,
    freshness_timer: Option<JoinHandle<()>>,
    overdue_timer: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, Default)]
struct ReminderSentState {
    freshness_sent: bool,
    overdue_sent: bool,
}

struct ReminderProgress {
    task: TaskEstimate,
    sent: ReminderSentState,
}

impl EtaReminderController {
    pub(crate) async fn lock_dispatch(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.dispatch).lock_owned().await
    }

    #[cfg(test)]
    pub(crate) async fn state_for_tests(&self, task_id: &str) -> Option<(bool, bool, bool, bool)> {
        let state = self.state.lock().await;
        state.tasks.get(task_id).map(|entry| {
            (
                entry.freshness_sent,
                entry.overdue_sent,
                entry.freshness_timer.is_some(),
                entry.overdue_timer.is_some(),
            )
        })
    }

    pub(crate) async fn schedule(
        self: &Arc<Self>,
        control: AgentControl,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        tasks: &[TaskEstimate],
        freshness_minimum: Duration,
    ) {
        let _dispatch = self.lock_dispatch().await;
        self.schedule_locked(control, state_db, root_thread_id, tasks, freshness_minimum)
            .await;
    }

    pub(crate) async fn schedule_locked(
        self: &Arc<Self>,
        control: AgentControl,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        tasks: &[TaskEstimate],
        freshness_minimum: Duration,
    ) {
        let previous = HashMap::new();
        self.schedule_locked_with_previous(
            control,
            state_db,
            root_thread_id,
            tasks,
            freshness_minimum,
            &previous,
        )
        .await;
    }

    async fn schedule_locked_with_previous(
        self: &Arc<Self>,
        control: AgentControl,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        tasks: &[TaskEstimate],
        freshness_minimum: Duration,
        previous: &HashMap<String, ReminderProgress>,
    ) {
        // Read the active projection so grouping suppression sees unchanged executable children.
        let freshness_minimum_seconds =
            i64::try_from(freshness_minimum.as_secs()).unwrap_or(i64::MAX);
        let scheduling_tasks = state_db
            .read_task_estimate_snapshot_with_freshness_minimum(
                root_thread_id,
                Utc::now(),
                None,
                None,
                freshness_minimum_seconds,
            )
            .await
            .map(|snapshot| snapshot.active)
            .unwrap_or_else(|_| tasks.to_vec());
        let active_task_ids = scheduling_tasks
            .iter()
            .filter(|task| !task.status.is_terminal())
            .map(|task| task.task_id.as_str())
            .collect::<HashSet<_>>();
        // A parent may predate its child; cancel every grouped parent from the full projection
        // before replacing changed rows.
        let grouping_parent_ids = scheduling_tasks
            .iter()
            .filter(|task| {
                scheduling_tasks.iter().any(|child| {
                    child.parent_task_id.as_deref() == Some(task.task_id.as_str())
                        && active_task_ids.contains(child.task_id.as_str())
                })
            })
            .map(|task| task.task_id.clone())
            .collect::<HashSet<_>>();
        if !grouping_parent_ids.is_empty() {
            let mut state = self.state.lock().await;
            for task_id in grouping_parent_ids {
                if let Some(entry) = state.tasks.remove(&task_id) {
                    abort_entry(entry);
                }
            }
        }
        for task in tasks {
            // Prefer the durable row when the caller supplied a higher-precision in-memory
            // timestamp. SQLite stores ETA timestamps at epoch-second precision; scheduling and
            // delivery must compare the same persisted task identity.
            let task = scheduling_tasks
                .iter()
                .find(|persisted| persisted.task_id == task.task_id)
                .unwrap_or(task);
            // Pending work has no elapsed baseline; wait for explicit `start` and avoid dependency
            // placeholders that cannot execute until another task finishes.
            if task.status == TaskEstimateStatus::Pending {
                self.cancel_task_locked(&task.task_id).await;
                continue;
            }
            // Grouping parents have no independent clock while active children are unfinished;
            // leave them for explicit completion without duplicate executable-work reminders.
            if scheduling_tasks.iter().any(|child| {
                child.parent_task_id.as_deref() == Some(task.task_id.as_str())
                    && active_task_ids.contains(child.task_id.as_str())
            }) {
                self.cancel_task_locked(&task.task_id).await;
                continue;
            }
            self.schedule_task_locked(
                control.clone(),
                Arc::clone(&state_db),
                root_thread_id,
                task,
                freshness_minimum,
                previous
                    .get(&task.task_id)
                    .filter(|previous| previous.task == *task)
                    .map(|previous| previous.sent)
                    .unwrap_or_default(),
            )
            .await;
        }
    }

    pub(crate) async fn reconfigure_locked(
        self: &Arc<Self>,
        control: AgentControl,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        freshness_minimum: Duration,
    ) {
        // A configuration refresh may race an explicit root pause. The pause path owns the
        // dispatch boundary and will reconfigure again after `/continue`; leave the suspended
        // entries intact while the tree remains paused.
        if control.root_activity_paused() {
            return;
        }
        let freshness_minimum_seconds =
            i64::try_from(freshness_minimum.as_secs()).unwrap_or(i64::MAX);
        let Ok(snapshot) = state_db
            .read_task_estimate_snapshot_with_freshness_minimum(
                root_thread_id,
                Utc::now(),
                None,
                None,
                freshness_minimum_seconds,
            )
            .await
        else {
            return;
        };
        // Keep the suspended state if a pause began while the durable snapshot was loading. The
        // pause path will acquire dispatch next and invalidate its timer handles.
        if control.root_activity_paused() {
            return;
        }
        let previous = {
            let mut state = self.state.lock().await;
            let previous = state
                .tasks
                .drain()
                .map(|(task_id, entry)| {
                    let progress = ReminderProgress {
                        task: entry.task.clone(),
                        sent: ReminderSentState {
                            freshness_sent: entry.freshness_sent,
                            overdue_sent: entry.overdue_sent,
                        },
                    };
                    abort_entry(entry);
                    (task_id, progress)
                })
                .collect::<HashMap<_, _>>();
            state.next_generation = state.next_generation.wrapping_add(1);
            previous
        };
        self.schedule_locked_with_previous(
            control,
            state_db,
            root_thread_id,
            &snapshot.active,
            freshness_minimum,
            &previous,
        )
        .await;
    }

    pub(crate) async fn cancel_all(&self) {
        let _dispatch = self.lock_dispatch().await;
        self.cancel_all_locked().await;
    }

    pub(crate) async fn cancel_all_locked(&self) {
        let mut state = self.state.lock().await;
        for (_, entry) in state.tasks.drain() {
            abort_entry(entry);
        }
        state.next_generation = state.next_generation.wrapping_add(1);
    }

    /// Suspend all reminders at an explicit pause boundary while retaining delivery latches and
    /// durable task identity for the resume reconfiguration.
    pub(crate) async fn suspend_all(&self) {
        let _dispatch = self.lock_dispatch().await;
        let mut state = self.state.lock().await;
        state.next_generation = state.next_generation.wrapping_add(1);
        let generation = state.next_generation;
        for entry in state.tasks.values_mut() {
            suspend_entry(entry, generation);
        }
    }

    pub(crate) async fn cancel_owner(&self, owner_thread_id: ThreadId) {
        let _dispatch = self.lock_dispatch().await;
        self.cancel_owner_locked(owner_thread_id).await;
    }

    pub(crate) async fn cancel_owner_locked(&self, owner_thread_id: ThreadId) {
        let mut state = self.state.lock().await;
        let task_ids = state
            .tasks
            .iter()
            .filter(|(_, entry)| entry.task.owner_thread_id == owner_thread_id)
            .map(|(task_id, _)| task_id.clone())
            .collect::<Vec<_>>();
        for task_id in task_ids {
            if let Some(entry) = state.tasks.remove(&task_id) {
                abort_entry(entry);
            }
        }
        state.next_generation = state.next_generation.wrapping_add(1);
    }

    /// Suspend reminders owned by one Worker while retaining their delivery latches for resume.
    pub(crate) async fn suspend_owner(&self, owner_thread_id: ThreadId) {
        let _dispatch = self.lock_dispatch().await;
        let mut state = self.state.lock().await;
        state.next_generation = state.next_generation.wrapping_add(1);
        let generation = state.next_generation;
        for entry in state.tasks.values_mut() {
            if entry.task.owner_thread_id == owner_thread_id {
                suspend_entry(entry, generation);
            }
        }
    }

    async fn schedule_task_locked(
        self: &Arc<Self>,
        control: AgentControl,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        task: &TaskEstimate,
        freshness_minimum: Duration,
        sent: ReminderSentState,
    ) {
        if task.status.is_terminal() || task.status == TaskEstimateStatus::Blocked {
            self.cancel_task_locked(&task.task_id).await;
            return;
        }
        let mut state = self.state.lock().await;
        if let Some(previous) = state.tasks.remove(&task.task_id) {
            abort_entry(previous);
        }
        state.next_generation = state.next_generation.wrapping_add(1);
        let generation = state.next_generation;
        let minimum_seconds = i64::try_from(freshness_minimum.as_secs()).unwrap_or(i64::MAX);
        let freshness_seconds = task.freshness_delay_seconds(minimum_seconds);
        let freshness_deadline = task
            .updated_at
            .checked_add_signed(ChronoDuration::seconds(freshness_seconds))
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        let freshness_timer = (!sent.freshness_sent).then(|| {
            self.spawn_timer(
                control.clone(),
                Arc::clone(&state_db),
                root_thread_id,
                task.task_id.clone(),
                generation,
                ReminderTrigger::Freshness,
                deadline_to_instant(freshness_deadline),
            )
        });
        let overdue_timer = (task.status == TaskEstimateStatus::Active && !sent.overdue_sent)
            .then_some(task.current_upper_seconds)
            .flatten()
            .map(|upper| {
                let deadline = task
                    .updated_at
                    .checked_add_signed(ChronoDuration::seconds(upper.max(0)))
                    .unwrap_or(DateTime::<Utc>::MAX_UTC);
                self.spawn_timer(
                    control,
                    Arc::clone(&state_db),
                    root_thread_id,
                    task.task_id.clone(),
                    generation,
                    ReminderTrigger::Overdue,
                    deadline_to_instant(deadline),
                )
            });
        state.tasks.insert(
            task.task_id.clone(),
            EtaReminderEntry {
                generation,
                task: task.clone(),
                freshness_sent: sent.freshness_sent,
                overdue_sent: sent.overdue_sent,
                freshness_timer,
                overdue_timer,
            },
        );
    }

    async fn cancel_task_locked(&self, task_id: &str) {
        if let Some(entry) = self.state.lock().await.tasks.remove(task_id) {
            abort_entry(entry);
        }
    }

    fn spawn_timer(
        self: &Arc<Self>,
        control: AgentControl,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        task_id: String,
        generation: u64,
        trigger: ReminderTrigger,
        deadline: Instant,
    ) -> JoinHandle<()> {
        let controller = Arc::clone(self);
        tokio::spawn(async move {
            sleep_until(deadline).await;
            controller
                .fire(
                    control,
                    state_db,
                    root_thread_id,
                    task_id,
                    generation,
                    trigger,
                )
                .await;
        })
    }

    async fn fire(
        &self,
        control: AgentControl,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        task_id: String,
        generation: u64,
        trigger: ReminderTrigger,
    ) {
        // Serialize the claim with scheduling, reconfiguration, and lifecycle cancellation so a
        // callback cannot consume a sent latch while waiting behind a replacement generation.
        let _dispatch = self.dispatch.lock().await;
        if !self.generation_is_current(&task_id, generation).await {
            return;
        }
        let Ok(snapshot) = state_db
            .read_task_estimate_snapshot(root_thread_id, Utc::now(), None, None)
            .await
        else {
            return;
        };
        let Some(task) = snapshot.active.iter().find(|task| task.task_id == task_id) else {
            self.cancel_task_locked(&task_id).await;
            return;
        };
        if !self
            .task_identity_is_current(&task_id, generation, task)
            .await
        {
            return;
        }
        if control.root_activity_paused() {
            return;
        }
        if matches!(
            control.get_status(task.owner_thread_id).await,
            crate::agent::AgentStatus::NotFound | crate::agent::AgentStatus::Shutdown
        ) {
            self.cancel_task_locked(&task_id).await;
            return;
        }
        if task.status.is_terminal() || task.status == TaskEstimateStatus::Blocked {
            self.cancel_task_locked(&task_id).await;
            return;
        }
        if !state_db
            .thread_is_open_descendant(root_thread_id, task.owner_thread_id)
            .await
            .unwrap_or(false)
        {
            self.cancel_task_locked(&task_id).await;
            return;
        }
        let owner_path = control
            .get_agent_metadata(task.owner_thread_id)
            .and_then(|metadata| metadata.agent_path)
            .or_else(|| (task.owner_thread_id == root_thread_id).then(AgentPath::root));
        let owner_path = if owner_path.is_some() {
            owner_path
        } else {
            state_db
                .get_thread(task.owner_thread_id)
                .await
                .ok()
                .flatten()
                .and_then(|metadata| metadata.agent_path)
                .and_then(|path| AgentPath::try_from(path).ok())
        };
        let Some(owner_path) = owner_path else {
            self.cancel_task_locked(&task_id).await;
            return;
        };
        // Claim only once all delivery gates have passed. A paused root or a transiently
        // unavailable owner can therefore be rearmed by the next lifecycle/configuration pass.
        if !self.claim(&task_id, generation, trigger).await {
            return;
        }
        let message = EtaReminderMessage::new(
            owner_path.clone(),
            format_reminder(task, trigger, Utc::now()),
        );
        let mut communication = InterAgentCommunication::new(
            AgentPath::root(),
            owner_path,
            Vec::new(),
            message.render(),
            true,
        );
        communication.internal_chat_message_metadata_passthrough =
            Some(InternalChatMessageMetadataPassthrough {
                content_item_kinds: Some(vec![message.content_kind()]),
                ..Default::default()
            });
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Message, root_thread_id);
        let delivery = control
            .send_inter_agent_communication(
                task.owner_thread_id,
                communication.clone(),
                context,
                TurnStartOptions::default(),
            )
            .await;
        if let Err(error) = delivery
            && let Err(persist_error) = crate::session::persist_handoff_inter_agent_communication(
                &state_db,
                task.owner_thread_id,
                Some(root_thread_id),
                &communication,
                &TurnStartOptions::default(),
                false,
            )
            .await
        {
            warn!(
                task_id = %task.task_id,
                owner_thread_id = %task.owner_thread_id,
                %error,
                %persist_error,
                "failed to deliver or retain ETA reminder"
            );
        }
    }

    async fn claim(&self, task_id: &str, generation: u64, trigger: ReminderTrigger) -> bool {
        let mut state = self.state.lock().await;
        let Some(entry) = state.tasks.get_mut(task_id) else {
            return false;
        };
        if entry.generation != generation {
            return false;
        }
        let sent = match trigger {
            ReminderTrigger::Freshness => &mut entry.freshness_sent,
            ReminderTrigger::Overdue => &mut entry.overdue_sent,
        };
        if *sent {
            return false;
        }
        *sent = true;
        if matches!(trigger, ReminderTrigger::Freshness) {
            entry.freshness_timer.take();
        } else {
            entry.overdue_timer.take();
        }
        true
    }

    async fn generation_is_current(&self, task_id: &str, generation: u64) -> bool {
        self.state
            .lock()
            .await
            .tasks
            .get(task_id)
            .is_some_and(|entry| entry.generation == generation)
    }

    async fn task_identity_is_current(
        &self,
        task_id: &str,
        generation: u64,
        task: &TaskEstimate,
    ) -> bool {
        self.state
            .lock()
            .await
            .tasks
            .get(task_id)
            .is_some_and(|entry| entry.generation == generation && entry.task == *task)
    }
}

fn suspend_entry(entry: &mut EtaReminderEntry, generation: u64) {
    abort_timers(entry);
    entry.generation = generation;
}

fn abort_timers(entry: &mut EtaReminderEntry) {
    if let Some(timer) = entry.freshness_timer.take() {
        timer.abort();
    }
    if let Some(timer) = entry.overdue_timer.take() {
        timer.abort();
    }
}

fn abort_entry(mut entry: EtaReminderEntry) {
    abort_timers(&mut entry);
}

fn deadline_to_instant(deadline: DateTime<Utc>) -> Instant {
    let delay = (deadline - Utc::now()).to_std().unwrap_or_default();
    Instant::now() + delay
}
