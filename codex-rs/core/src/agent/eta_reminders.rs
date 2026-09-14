//! One-shot, owner-routed ETA freshness and overdue reminders.
//!
//! Reminder timers are armed only after a durable ETA mutation and are invalidated by the next
//! mutation, terminal lifecycle transition, reassignment, configuration refresh, or shutdown.
//! They never infer completion and never poll task state.

use super::AgentControl;
use crate::TurnStartOptions;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::context::ContextualUserFragment;
use crate::agent::eta_reminder_message::EtaReminderMessage;
use crate::agent::eta_reminder_message::ReminderTrigger;
use crate::agent::eta_reminder_message::format_reminder;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::protocol::InterAgentCommunication;
use codex_rollout::StateDbHandle;
use codex_state::TaskEstimate;
use codex_state::TaskEstimateStatus;
use chrono::DateTime;
use chrono::Duration as ChronoDuration;
use chrono::Utc;
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
    owner_thread_id: ThreadId,
    updated_at: DateTime<Utc>,
    freshness_sent: bool,
    overdue_sent: bool,
    freshness_timer: Option<JoinHandle<()>>,
    overdue_timer: Option<JoinHandle<()>>,
}

impl EtaReminderController {
    pub(crate) async fn lock_dispatch(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.dispatch).lock_owned().await
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
        // The mutation response contains only explicitly changed rows. Read the active projection
        // once so grouping suppression still sees unchanged executable children and a child
        // revision cannot accidentally arm its placeholder parent.
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
        for task in tasks {
            // Pending work has no truthful elapsed baseline yet. Wait for its explicit `start`
            // transition before asking the owner to reassess; this also avoids waking owners for
            // dependency placeholders that cannot execute until another task finishes.
            if task.status == TaskEstimateStatus::Pending {
                self.cancel_task_locked(&task.task_id).await;
                continue;
            }
            // A grouping parent represents its active children and has no independent clock while
            // those children are unfinished. Leave it available for explicit completion after
            // the children finish, but avoid duplicate reminders for the same executable work.
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
            )
            .await;
        }
    }

    pub(crate) async fn reconfigure(
        self: &Arc<Self>,
        control: AgentControl,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        freshness_minimum: Duration,
    ) {
        let _dispatch = self.lock_dispatch().await;
        self.cancel_all_locked().await;
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
        self.schedule_locked(
            control,
            state_db,
            root_thread_id,
            &snapshot.active,
            freshness_minimum,
        )
        .await;
    }

    pub(crate) async fn cancel_all(&self) {
        let _dispatch = self.lock_dispatch().await;
        self.cancel_all_locked().await;
    }

    async fn cancel_all_locked(&self) {
        let mut state = self.state.lock().await;
        for (_, entry) in state.tasks.drain() {
            abort_entry(entry);
        }
        state.next_generation = state.next_generation.wrapping_add(1);
    }

    pub(crate) async fn cancel_owner(&self, owner_thread_id: ThreadId) {
        let _dispatch = self.lock_dispatch().await;
        self.cancel_owner_locked(owner_thread_id).await;
    }

    async fn cancel_owner_locked(&self, owner_thread_id: ThreadId) {
        let mut state = self.state.lock().await;
        let task_ids = state
            .tasks
            .iter()
            .filter(|(_, entry)| entry.owner_thread_id == owner_thread_id)
            .map(|(task_id, _)| task_id.clone())
            .collect::<Vec<_>>();
        for task_id in task_ids {
            if let Some(entry) = state.tasks.remove(&task_id) {
                abort_entry(entry);
            }
        }
        state.next_generation = state.next_generation.wrapping_add(1);
    }

    async fn schedule_task_locked(
        self: &Arc<Self>,
        control: AgentControl,
        state_db: StateDbHandle,
        root_thread_id: ThreadId,
        task: &TaskEstimate,
        freshness_minimum: Duration,
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
        let freshness_timer = self.spawn_timer(
            control.clone(),
            Arc::clone(&state_db),
            root_thread_id,
            task.task_id.clone(),
            generation,
            ReminderTrigger::Freshness,
            deadline_to_instant(freshness_deadline),
        );
        let overdue_timer = (task.status == TaskEstimateStatus::Active)
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
                owner_thread_id: task.owner_thread_id,
                updated_at: task.updated_at,
                freshness_sent: false,
                overdue_sent: false,
                freshness_timer: Some(freshness_timer),
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
        if !self.claim(&task_id, generation, trigger).await {
            return;
        }
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
        let Some(task) = snapshot
            .active
            .iter()
            .find(|task| task.task_id == task_id)
        else {
            self.cancel_task_locked(&task_id).await;
            return;
        };
        if !self
            .task_identity_is_current(&task_id, generation, task.owner_thread_id, task.updated_at)
            .await
        {
            return;
        }
        if control.root_activity_paused() {
            // Explicit pause owns the boundary. The resume path reconfigures from durable state,
            // so this claimed callback cannot leak a model wake while work is paused.
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
            // Closed or deleted owners are never redirected through the Lead. The lifecycle
            // path cancels this owner too, while this durable check closes the callback race.
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
            // An unloaded or deleted owner is not silently redirected to the Lead.
            self.cancel_task_locked(&task_id).await;
            return;
        };
        let message = EtaReminderMessage::new(owner_path.clone(), format_reminder(task, trigger, Utc::now()));
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
        let context = AgentCommunicationContext::new(
            AgentCommunicationKind::Message,
            root_thread_id,
        );
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
        owner_thread_id: ThreadId,
        updated_at: DateTime<Utc>,
    ) -> bool {
        self.state
            .lock()
            .await
            .tasks
            .get(task_id)
            .is_some_and(|entry| {
                entry.generation == generation
                    && entry.owner_thread_id == owner_thread_id
                    && entry.updated_at == updated_at
            })
    }
}

fn abort_entry(mut entry: EtaReminderEntry) {
    if let Some(timer) = entry.freshness_timer.take() {
        timer.abort();
    }
    if let Some(timer) = entry.overdue_timer.take() {
        timer.abort();
    }
}

fn deadline_to_instant(deadline: DateTime<Utc>) -> Instant {
    let delay = (deadline - Utc::now()).to_std().unwrap_or_default();
    Instant::now() + delay
}
