use crate::PendingMailboxBlockerDetail;
use crate::PendingMailboxBlockerSource;
use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_diagnostics::Gauge;
use codex_diagnostics::GaugeGuard;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::sync::watch;

static PENDING_MAILBOX_MESSAGES: Gauge = Gauge::new("core.mailbox.pending");

pub(crate) const TEAM_LEAD_PROGRESS_SUMMARY_MAX_BYTES: usize = 8 * 1024;
const TEAM_LEAD_PROGRESS_MAX_ITEMS: usize = 32;
const TEAM_LEAD_PROGRESS_ITEM_MAX_BYTES: usize = 512;

/// Input consumed by a regular turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acceptance_order: Option<u64>,
    },
    FunctionCallOutput(#[serde(with = "turn_input_response_item")] ResponseItemEnvelope),
    // Preserve the existing serialized format while carrying injection API metadata
    // through the in-memory queue.
    ResponseItem(#[serde(with = "turn_input_response_item")] ResponseItemEnvelope),
    InterAgentCommunication(InterAgentCommunication),
}

mod turn_input_response_item {
    use super::ResponseItem;
    use super::ResponseItemEnvelope;
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serialize;
    use serde::Serializer;
    use serde::ser::Error as _;

    pub(super) fn serialize<S>(
        item: &ResponseItemEnvelope,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if item.metadata.is_some() {
            return Err(S::Error::custom(
                "annotated response items cannot cross the turn-input serialization boundary",
            ));
        }
        item.item.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<ResponseItemEnvelope, D::Error>
    where
        D: Deserializer<'de>,
    {
        ResponseItem::deserialize(deserializer).map(ResponseItemEnvelope::new)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputQueueActivity {
    Mailbox,
    Steer,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PendingInputStatus {
    pub(crate) has_pending_input: bool,
    pub(crate) has_user_input: bool,
    /// Raw response-item injections require a same-turn follow-up, but do not authorize it as an
    /// explicit user turn for usage-floor checks.
    pub(crate) has_pending_response_items: bool,
}

/// Turn-local pending input storage owned by the input queue flow.
#[derive(Default)]
pub(crate) struct TurnInputQueue {
    pub(crate) items: Vec<TurnInput>,
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    mailbox_pending_mails: Mutex<VecDeque<PendingMailboxCommunication>>,
    team_lead_progress: Mutex<TeamLeadProgressBuffer>,
    dependency_free_wait_handoff_turn: Mutex<Option<String>>,
}

struct PendingMailboxCommunication {
    communication: InterAgentCommunication,
    start_options: TurnStartOptions,
    enqueued_at: Instant,
    /// Marks synthetic deadline wakeups so cancellation can remove a wake that
    /// raced with user input or another actionable event.
    lead_oversight: bool,
    /// Marks manager-generated progress summaries so diagnostics do not attribute them to mail.
    lead_progress_summary: bool,
    /// Marks trigger mail admitted while this session was assigned to Team Lead. The marker lets
    /// final automatic-turn admission reject a stale trigger after `/team off` while ordinary
    /// non-team trigger mail keeps its existing behavior.
    team_lead_trigger: bool,
    _diagnostics_guard: GaugeGuard,
}

/// Mailbox contents consumed for a turn, retaining queue-only metadata until scheduler admission
/// is final. `TurnInput` stays wire-compatible; this sidecar is used only if a stale reservation
/// needs to put queue-only mail back.
pub(crate) struct DrainedMailboxInput {
    pub(crate) items: Vec<TurnInput>,
    pub(crate) start_options: TurnStartOptions,
    pub(crate) team_lead_trigger: bool,
    pending_mails: Vec<PendingMailboxCommunication>,
}

impl DrainedMailboxInput {
    fn empty() -> Self {
        Self {
            items: Vec::new(),
            start_options: TurnStartOptions::default(),
            team_lead_trigger: false,
            pending_mails: Vec::new(),
        }
    }
}

#[derive(Default)]
struct TeamLeadProgressBuffer {
    entries: VecDeque<TeamLeadProgressEntry>,
    bytes: usize,
    completion_generation: u64,
    completion_pending: bool,
    completion_pending_since: Option<Instant>,
    pending_completion_delivery_acks: HashSet<String>,
}

#[derive(Clone, Copy)]
enum TeamLeadProgressKind {
    Routine,
    Completion,
}

struct TeamLeadProgressEntry {
    author: String,
    message: String,
}

pub(crate) struct ClaimedManagerCompletionBatch {
    pub(crate) progress_summary: Option<String>,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            mailbox_pending_mails: Mutex::new(VecDeque::new()),
            team_lead_progress: Mutex::new(TeamLeadProgressBuffer::default()),
            dependency_free_wait_handoff_turn: Mutex::new(None),
        }
    }

    /// Claims the one parent handoff allowed for a dependency-free wait in one model turn.
    ///
    /// A wait call can be retried by the model without any new work arriving. Keep that retry
    /// quiet until a new turn or explicit input gives the Worker a meaningful reason to ask its
    /// parent for attention again.
    pub(crate) async fn claim_dependency_free_wait_handoff(&self, sub_id: &str) -> bool {
        if sub_id.is_empty() {
            return false;
        }
        let mut claimed_turn = self.dependency_free_wait_handoff_turn.lock().await;
        if claimed_turn.as_deref() == Some(sub_id) {
            return false;
        }
        *claimed_turn = Some(sub_id.to_string());
        true
    }

    /// Allows meaningful incoming work to arm a later dependency-free wait handoff in the same
    /// active turn. Routine queue-only progress does not call this method.
    async fn reset_dependency_free_wait_handoff(&self) {
        *self.dependency_free_wait_handoff_turn.lock().await = None;
    }

    /// Retains routine Worker progress without publishing mailbox activity. The bounded buffer is
    /// summarized only when an actionable wake reaches the Lead.
    pub(crate) async fn enqueue_team_lead_progress(&self, communication: InterAgentCommunication) {
        self.enqueue_team_lead_progress_entry(communication, TeamLeadProgressKind::Routine)
            .await;
    }

    /// Retains a bounded terminal Worker result for the manager-only completion batch.
    /// The generation lets a short quiet-window flush coalesce concurrent completions while
    /// actionable input can drain the same buffer first.
    pub(crate) async fn enqueue_team_lead_completion(
        &self,
        communication: InterAgentCommunication,
    ) -> u64 {
        self.enqueue_team_lead_progress_entry(communication, TeamLeadProgressKind::Completion)
            .await
    }

    async fn enqueue_team_lead_progress_entry(
        &self,
        communication: InterAgentCommunication,
        kind: TeamLeadProgressKind,
    ) -> u64 {
        let message = if communication.encrypted_content.is_some() {
            "[encrypted routine progress]".to_string()
        } else {
            truncate_progress_message(&communication.content)
        };
        let entry = TeamLeadProgressEntry {
            author: communication.author.to_string(),
            message,
        };
        let entry_bytes = entry.author.len() + entry.message.len() + 4;
        let mut progress = self.team_lead_progress.lock().await;
        if let TeamLeadProgressKind::Completion = kind {
            if !progress.completion_pending {
                progress.completion_pending_since = Some(Instant::now());
            }
            progress.completion_generation = progress.completion_generation.wrapping_add(1);
            progress.completion_pending = true;
        }
        progress.bytes = progress.bytes.saturating_add(entry_bytes);
        progress.entries.push_back(entry);
        while progress.entries.len() > TEAM_LEAD_PROGRESS_MAX_ITEMS
            || progress.bytes > TEAM_LEAD_PROGRESS_SUMMARY_MAX_BYTES
        {
            let Some(removed) = progress.entries.pop_front() else {
                progress.bytes = 0;
                break;
            };
            progress.bytes = progress
                .bytes
                .saturating_sub(removed.author.len() + removed.message.len() + 4);
        }
        progress.completion_generation
    }

    /// Atomically claims a pending generation and drains its summary before an explicit user turn
    /// can consume the same progress buffer.
    pub(crate) async fn take_manager_completion_batch(
        &self,
        generation: u64,
    ) -> Option<ClaimedManagerCompletionBatch> {
        let mut progress = self.team_lead_progress.lock().await;
        if !progress.completion_pending
            || progress.completion_generation != generation
            || !progress.pending_completion_delivery_acks.is_empty()
        {
            return None;
        }
        progress.completion_pending = false;
        progress.completion_pending_since = None;
        Some(ClaimedManagerCompletionBatch {
            progress_summary: take_team_progress_summary_locked(&mut progress),
        })
    }

    pub(crate) async fn has_pending_manager_completion(&self) -> bool {
        self.team_lead_progress.lock().await.completion_pending
    }

    /// Captures queue counts and oldest ages without copying mailbox payload or identity fields.
    pub(crate) async fn pending_handoff_blocker_diagnostics(
        &self,
        thread_id: &str,
    ) -> Vec<PendingMailboxBlockerDetail> {
        let mut diagnostics = Vec::new();
        {
            let mailbox = self.mailbox_pending_mails.lock().await;
            let count = mailbox
                .iter()
                .filter(|mail| !mail.lead_oversight && !mail.lead_progress_summary)
                .count();
            if count > 0 {
                let oldest_age = mailbox
                    .iter()
                    .filter(|mail| !mail.lead_oversight && !mail.lead_progress_summary)
                    .map(|mail| mail.enqueued_at.elapsed())
                    .max()
                    .unwrap_or_default();
                diagnostics.push(PendingMailboxBlockerDetail {
                    thread_id: thread_id.to_string(),
                    source: PendingMailboxBlockerSource::InterAgentMailbox,
                    count: u64::try_from(count).unwrap_or(u64::MAX),
                    oldest_age_ms: Some(duration_millis(oldest_age)),
                });
            }
            let count = mailbox
                .iter()
                .filter(|mail| mail.lead_progress_summary)
                .count();
            if count > 0 {
                let oldest_age = mailbox
                    .iter()
                    .filter(|mail| mail.lead_progress_summary)
                    .map(|mail| mail.enqueued_at.elapsed())
                    .max()
                    .unwrap_or_default();
                diagnostics.push(PendingMailboxBlockerDetail {
                    thread_id: thread_id.to_string(),
                    source: PendingMailboxBlockerSource::LeadProgressSummary,
                    count: u64::try_from(count).unwrap_or(u64::MAX),
                    oldest_age_ms: Some(duration_millis(oldest_age)),
                });
            }
            let count = mailbox.iter().filter(|mail| mail.lead_oversight).count();
            if count > 0 {
                let oldest_age = mailbox
                    .iter()
                    .filter(|mail| mail.lead_oversight)
                    .map(|mail| mail.enqueued_at.elapsed())
                    .max()
                    .unwrap_or_default();
                diagnostics.push(PendingMailboxBlockerDetail {
                    thread_id: thread_id.to_string(),
                    source: PendingMailboxBlockerSource::LeadOversightWake,
                    count: u64::try_from(count).unwrap_or(u64::MAX),
                    oldest_age_ms: Some(duration_millis(oldest_age)),
                });
            }
        }
        {
            let progress = self.team_lead_progress.lock().await;
            if progress.completion_pending {
                let oldest_age = progress
                    .completion_pending_since
                    .map(|since| since.elapsed())
                    .unwrap_or_default();
                diagnostics.push(PendingMailboxBlockerDetail {
                    thread_id: thread_id.to_string(),
                    source: PendingMailboxBlockerSource::ManagerCompletionBatch,
                    count: 1,
                    oldest_age_ms: Some(duration_millis(oldest_age)),
                });
            }
        }
        diagnostics
    }

    /// Returns the latest buffered completion generation so a Lead work-policy update can
    /// release a batch whose original quiet-window callback already observed active Workers.
    pub(crate) async fn pending_manager_completion_generation(&self) -> Option<u64> {
        let progress = self.team_lead_progress.lock().await;
        progress
            .completion_pending
            .then_some(progress.completion_generation)
    }

    /// Registers a queued completion before it enters the parent submission loop. Batch claims
    /// share this lock so an older quiet timer cannot split the batch around this delivery.
    pub(crate) async fn register_manager_completion_delivery_ack(&self, submission_id: String) {
        self.team_lead_progress
            .lock()
            .await
            .pending_completion_delivery_acks
            .insert(submission_id);
    }

    /// Releases one queued completion after insertion, rejection, or enqueue failure.
    pub(crate) async fn release_manager_completion_delivery_ack(
        &self,
        submission_id: &str,
    ) -> bool {
        self.team_lead_progress
            .lock()
            .await
            .pending_completion_delivery_acks
            .remove(submission_id)
    }

    pub(crate) async fn clear_manager_completion_delivery_acks(&self) {
        self.team_lead_progress
            .lock()
            .await
            .pending_completion_delivery_acks
            .clear();
    }

    /// Drains routine progress into a fixed-size summary for an actionable Lead wake.
    pub(crate) async fn take_team_progress_summary(&self) -> Option<String> {
        let mut progress = self.team_lead_progress.lock().await;
        progress.completion_pending = false;
        progress.completion_pending_since = None;
        take_team_progress_summary_locked(&mut progress)
    }

    pub(crate) async fn clear_team_lead_progress(&self) {
        let mut progress = self.team_lead_progress.lock().await;
        progress.entries.clear();
        progress.bytes = 0;
        progress.completion_generation = progress.completion_generation.wrapping_add(1);
        progress.completion_pending = false;
        progress.completion_pending_since = None;
    }

    pub(crate) async fn subscribe_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> (
        watch::Receiver<InputQueueActivity>,
        Option<InputQueueActivity>,
    ) {
        let activity_rx = self.activity_tx.subscribe();
        let has_pending_steer = if let Some(turn_state) = turn_state {
            turn_state.lock().await.pending_input.has_pending_input()
        } else {
            false
        };
        let pending_activity = if has_pending_steer {
            Some(InputQueueActivity::Steer)
        } else if self.has_pending_mailbox_items().await {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        };
        (activity_rx, pending_activity)
    }

    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
    ) {
        self.enqueue_mailbox_communication_with_team_lead_marker(
            communication,
            start_options,
            /*team_lead_trigger*/ false,
            /*lead_progress_summary*/ false,
        )
        .await;
    }

    /// Enqueues trigger mail created while the target session was assigned to Team Lead. This
    /// internal marker does not change wire behavior; it only protects the final scheduler
    /// admission from a concurrent Team Off update.
    pub(crate) async fn enqueue_team_lead_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
    ) {
        self.enqueue_mailbox_communication_with_team_lead_marker(
            communication,
            start_options,
            /*team_lead_trigger*/ true,
            /*lead_progress_summary*/ false,
        )
        .await;
    }

    /// Enqueues a manager-generated progress summary as Lead input without classifying it as mail.
    pub(crate) async fn enqueue_team_lead_progress_summary(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
    ) {
        self.enqueue_mailbox_communication_with_team_lead_marker(
            communication,
            start_options,
            /*team_lead_trigger*/ true,
            /*lead_progress_summary*/ true,
        )
        .await;
    }

    async fn enqueue_mailbox_communication_with_team_lead_marker(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
        team_lead_trigger: bool,
        lead_progress_summary: bool,
    ) {
        if communication.trigger_turn {
            self.reset_dependency_free_wait_handoff().await;
        }
        self.mailbox_pending_mails
            .lock()
            .await
            .push_back(PendingMailboxCommunication {
                communication,
                start_options,
                enqueued_at: Instant::now(),
                lead_oversight: false,
                lead_progress_summary,
                team_lead_trigger,
                _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
            });
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
    }

    /// Enqueues a synthetic Lead oversight message. These entries are kept
    /// distinct from ordinary mailbox traffic so a cancellation can remove a
    /// stale deadline wake without touching actionable Worker or user mail.
    pub(crate) async fn enqueue_lead_oversight_communication(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
    ) {
        self.mailbox_pending_mails
            .lock()
            .await
            .push_back(PendingMailboxCommunication {
                communication,
                start_options,
                enqueued_at: Instant::now(),
                lead_oversight: true,
                lead_progress_summary: false,
                team_lead_trigger: true,
                _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
            });
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
    }

    /// Drops synthetic Lead deadline messages that raced with cancellation.
    pub(crate) async fn clear_lead_oversight_mailbox(&self) {
        self.mailbox_pending_mails
            .lock()
            .await
            .retain(|mail| !mail.lead_oversight);
        // Wake an in-flight Lead `wait_agent` call after cancellation. The
        // watch value is only a notification; no input is inserted into the
        // turn, so a later wait still observes the real pending state.
        self.activity_tx.send_replace(InputQueueActivity::Steer);
    }

    /// Drops automatic trigger mail when a Lead turns team mode off. Queue-only
    /// communication remains available for an explicit subsequent turn.
    pub(crate) async fn clear_trigger_turn_mailbox(&self) {
        self.mailbox_pending_mails
            .lock()
            .await
            .retain(|mail| !mail.communication.trigger_turn);
    }

    /// Drops only trigger mail that was admitted while this session owned the Team Lead role.
    /// Ordinary trigger mail queued after Team Off remains available for its normal explicit
    /// action semantics.
    pub(crate) async fn clear_team_lead_trigger_mailbox(&self) {
        self.mailbox_pending_mails
            .lock()
            .await
            .retain(|mail| !(mail.team_lead_trigger && mail.communication.trigger_turn));
    }

    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        !self.mailbox_pending_mails.lock().await.is_empty()
    }

    pub(crate) async fn has_trigger_turn_mailbox_items(&self) -> bool {
        self.mailbox_pending_mails
            .lock()
            .await
            .iter()
            .any(|mail| mail.communication.trigger_turn)
    }

    pub(crate) async fn drain_mailbox_input_items(&self) -> (Vec<TurnInput>, TurnStartOptions) {
        let drained = self.drain_mailbox_input_items_with_team_lead_marker().await;
        (drained.items, drained.start_options)
    }

    /// Drains mailbox input and reports whether any trigger was admitted while this session was
    /// assigned to Team Lead. The marker is consumed with the mail so the scheduler can enforce
    /// the assignment at its final admission boundary even if Team Off races the drain.
    pub(crate) async fn drain_mailbox_input_items_with_team_lead_marker(
        &self,
    ) -> DrainedMailboxInput {
        let pending_mails = self
            .mailbox_pending_mails
            .lock()
            .await
            .drain(..)
            .collect::<Vec<_>>();
        let team_lead_trigger = pending_mails
            .iter()
            .any(|mail| mail.team_lead_trigger && mail.communication.trigger_turn);
        // A later follow-up supersedes the earlier choice, including an omitted choice.
        let mut start_options = pending_mails
            .iter()
            .rev()
            .find(|mail| mail.communication.trigger_turn)
            .map(|mail| mail.start_options.clone())
            .unwrap_or_default();
        start_options.parent_turn_id = pending_mails
            .iter()
            .filter(|mail| mail.communication.trigger_turn)
            .map(|mail| mail.start_options.parent_turn_id.as_deref())
            .reduce(|expected, candidate| expected.filter(|id| candidate == Some(*id)))
            .and_then(|id| id.filter(|id| !id.trim().is_empty()).map(str::to_string));
        start_options.root_turn_id = pending_mails
            .iter()
            .find(|mail| mail.communication.trigger_turn)
            .and_then(|mail| {
                mail.start_options
                    .parent_turn_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .and(mail.start_options.root_turn_id.as_deref())
                    .filter(|id| !id.trim().is_empty())
            })
            .map(str::to_string);
        let items = pending_mails
            .iter()
            .map(|mail| TurnInput::InterAgentCommunication(mail.communication.clone()))
            .collect();
        DrainedMailboxInput {
            items,
            start_options,
            team_lead_trigger,
            pending_mails,
        }
    }

    /// Restores queue-only mailbox mail after a stale automatic-trigger reservation is
    /// invalidated. No activity notification is sent because queue-only mail is not itself a
    /// reason to start a turn.
    pub(crate) async fn requeue_queue_only_mail(
        &self,
        input: Vec<TurnInput>,
        drained_mailbox: DrainedMailboxInput,
    ) {
        let mut mailbox = self.mailbox_pending_mails.lock().await;
        for mail in drained_mailbox.pending_mails.into_iter().rev() {
            if !mail.communication.trigger_turn {
                mailbox.push_front(mail);
            }
        }
        for item in input.into_iter().rev() {
            let TurnInput::InterAgentCommunication(communication) = item else {
                continue;
            };
            if communication.trigger_turn {
                continue;
            }
            mailbox.push_front(PendingMailboxCommunication {
                communication,
                start_options: TurnStartOptions::default(),
                enqueued_at: Instant::now(),
                lead_oversight: false,
                lead_progress_summary: false,
                team_lead_trigger: false,
                _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
            });
        }
    }

    pub(crate) async fn turn_state_for_sub_id(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) -> Option<Arc<Mutex<TurnState>>> {
        let active = active_turn.lock().await;
        active.as_ref().and_then(|active_turn| {
            active_turn
                .task
                .as_ref()
                .is_some_and(|task| task.turn_context.sub_id == sub_id)
                .then(|| Arc::clone(&active_turn.turn_state))
        })
    }

    pub(crate) async fn clear_pending(&self, active_turn: &ActiveTurn) {
        let mut turn_state = active_turn.turn_state.lock().await;
        turn_state.clear_pending_waiters();
        turn_state.pending_input.items.clear();
    }

    pub(crate) async fn defer_mailbox_delivery_to_next_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        let mut turn_state = turn_state.lock().await;
        // Explicit same-turn work still needs a follow-up. Queue-only child mail does not: keep
        // it pending so task completion records it for the next turn without sampling again.
        if turn_state.pending_input.items.iter().any(|input| {
            !matches!(
                input,
                TurnInput::InterAgentCommunication(communication) if !communication.trigger_turn
            )
        }) {
            return;
        }
        turn_state.set_mailbox_delivery_phase(MailboxDeliveryPhase::NextTurn);
    }

    pub(crate) async fn accept_mailbox_delivery_for_current_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        self.accept_mailbox_delivery_for_turn_state(turn_state.as_ref())
            .await;
    }

    pub(super) async fn accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) {
        turn_state
            .lock()
            .await
            .accept_mailbox_delivery_for_current_turn();
    }

    pub(super) async fn extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        self.reset_dependency_free_wait_handoff().await;
        {
            let mut turn_state = turn_state.lock().await;
            for input in input {
                turn_state.push_pending_input(input);
            }
            turn_state.accept_mailbox_delivery_for_current_turn();
        }
        self.activity_tx.send_replace(InputQueueActivity::Steer);
    }

    pub(crate) async fn extend_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        let mut turn_state = turn_state.lock().await;
        for input in input {
            turn_state.push_pending_input(input);
        }
    }

    pub(crate) async fn take_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        turn_state.lock().await.take_pending_input()
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> (Vec<TurnInput>, TurnStartOptions) {
        let (mut pending_input, drained_mailbox) = self
            .get_pending_input_with_team_lead_marker(active_turn)
            .await;
        pending_input.extend(drained_mailbox.items);
        (pending_input, drained_mailbox.start_options)
    }

    /// Like [`Self::get_pending_input`], but preserves the internal Team Lead trigger marker for
    /// the automatic-turn scheduler.
    pub(crate) async fn get_pending_input_with_team_lead_marker(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> (Vec<TurnInput>, DrainedMailboxInput) {
        let (pending_input, accepts_mailbox_delivery, active_turn_metadata) = {
            let mut active = active_turn.lock().await;
            match active.as_mut() {
                Some(active_turn) => {
                    let active_turn_metadata = active_turn
                        .task
                        .as_ref()
                        .map(|task| Arc::clone(&task.turn_context.turn_metadata_state));
                    let mut turn_state = active_turn.turn_state.lock().await;
                    let accepts_mailbox_delivery =
                        turn_state.accepts_mailbox_delivery_for_current_turn();
                    let pending_input = if accepts_mailbox_delivery {
                        turn_state.pending_input.items.split_off(0)
                    } else {
                        Vec::new()
                    };
                    (
                        pending_input,
                        accepts_mailbox_delivery,
                        active_turn_metadata,
                    )
                }
                None => (Vec::new(), true, None),
            }
        };
        if !accepts_mailbox_delivery {
            return (pending_input, DrainedMailboxInput::empty());
        }
        let drained_mailbox = self.drain_mailbox_input_items_with_team_lead_marker().await;
        if let Some(active_turn_metadata) = active_turn_metadata
            && active_turn_metadata.root_turn_id().is_none()
            && let Some(root_turn_id) = drained_mailbox.start_options.root_turn_id.as_ref()
        {
            active_turn_metadata.set_root_turn_id(root_turn_id.clone());
        }
        (pending_input, drained_mailbox)
    }

    pub(crate) async fn has_pending_input(&self, active_turn: &Mutex<Option<ActiveTurn>>) -> bool {
        self.pending_input_status(active_turn)
            .await
            .has_pending_input
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state reads must remain atomic"
    )]
    pub(crate) async fn pending_input_status(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> PendingInputStatus {
        let (turn_status, accepts_mailbox_delivery) = {
            let active = active_turn.lock().await;
            match active.as_ref() {
                Some(active_turn) => {
                    let turn_state = active_turn.turn_state.lock().await;
                    (
                        turn_state.pending_input.status(),
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                    )
                }
                None => (PendingInputStatus::default(), true),
            }
        };
        if !accepts_mailbox_delivery {
            return PendingInputStatus::default();
        }
        if turn_status.has_pending_input {
            return turn_status;
        }
        PendingInputStatus {
            has_pending_input: self.has_pending_mailbox_items().await,
            ..turn_status
        }
    }
}

fn take_team_progress_summary_locked(progress: &mut TeamLeadProgressBuffer) -> Option<String> {
    if progress.entries.is_empty() {
        progress.bytes = 0;
        return None;
    }
    let entries = progress.entries.drain(..).collect::<Vec<_>>();
    progress.bytes = 0;
    let mut lines = Vec::new();
    let mut summary_bytes = "Routine Worker progress summary (".len()
        + entries.len().to_string().len()
        + " retained updates):".len();
    for entry in entries.into_iter().rev() {
        let line = format!("\n- {}: {}", entry.author, entry.message);
        if summary_bytes + line.len() > TEAM_LEAD_PROGRESS_SUMMARY_MAX_BYTES {
            break;
        }
        summary_bytes += line.len();
        lines.push(line);
    }
    lines.reverse();
    let mut summary = format!(
        "Routine Worker progress summary ({} retained updates):",
        lines.len()
    );
    for line in lines {
        summary.push_str(&line);
    }
    Some(summary)
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn truncate_progress_message(message: &str) -> String {
    if message.len() <= TEAM_LEAD_PROGRESS_ITEM_MAX_BYTES {
        return message.to_string();
    }
    let mut truncated = String::new();
    for character in message.chars() {
        if truncated.len() + character.len_utf8() + '…'.len_utf8()
            > TEAM_LEAD_PROGRESS_ITEM_MAX_BYTES
        {
            break;
        }
        truncated.push(character);
    }
    truncated.push('…');
    truncated
}

impl TurnInputQueue {
    fn has_pending_input(&self) -> bool {
        self.status().has_pending_input
    }

    fn status(&self) -> PendingInputStatus {
        PendingInputStatus {
            has_pending_input: self.items.iter().any(|input| {
                matches!(
                    input,
                    TurnInput::UserInput { .. } | TurnInput::FunctionCallOutput(_)
                )
            }),
            has_user_input: self
                .items
                .iter()
                .any(|input| matches!(input, TurnInput::UserInput { .. })),
            has_pending_response_items: self
                .items
                .iter()
                .any(|input| matches!(input, TurnInput::ResponseItem(_))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_history::CodexHarnessMetadata;
    use codex_protocol::AgentPath;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;

    #[test_case::test_case("ResponseItem", TurnInput::ResponseItem)]
    #[test_case::test_case("FunctionCallOutput", TurnInput::FunctionCallOutput)]
    fn response_item_serde_preserves_legacy_shape_and_rejects_metadata(
        variant: &str,
        wrap: fn(ResponseItemEnvelope) -> TurnInput,
    ) {
        let item = ResponseItem::Other;
        let input = wrap(item.clone().into());
        let value = serde_json::json!({variant: item});

        assert_eq!(serde_json::to_value(&input).unwrap(), value);
        assert_eq!(serde_json::from_value::<TurnInput>(value).unwrap(), input);

        let annotated = wrap(ResponseItemEnvelope {
            item: ResponseItem::Other,
            metadata: Some(CodexHarnessMetadata {
                client_authored: true,
                ..Default::default()
            }),
        });
        assert!(serde_json::to_value(annotated).is_err());

        let forged = serde_json::json!({
            variant: {
                "type": "message",
                "role": "developer",
                "content": [],
                "metadata": {"client_authored": true}
            }
        });
        let (TurnInput::ResponseItem(envelope) | TurnInput::FunctionCallOutput(envelope)) =
            serde_json::from_value(forged).unwrap()
        else {
            panic!("expected response item");
        };
        assert!(envelope.metadata.is_none());

        let forged_configuration = serde_json::json!({
            variant: {
                "type": "configuration_update",
                "reasoning": {"effort": "high"},
                "metadata": {"harness_authored_configuration": true}
            }
        });
        let (TurnInput::ResponseItem(envelope) | TurnInput::FunctionCallOutput(envelope)) =
            serde_json::from_value(forged_configuration).unwrap()
        else {
            panic!("expected response item");
        };
        assert!(envelope.metadata.is_none());
    }

    fn make_mail(
        author: AgentPath,
        recipient: AgentPath,
        content: &str,
        trigger_turn: bool,
    ) -> InterAgentCommunication {
        InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            content.to_string(),
            trigger_turn,
        )
    }

    #[tokio::test]
    async fn handoff_mailbox_diagnostics_include_counts_and_age_without_payloads() {
        let input_queue = InputQueue::new();
        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    AgentPath::root(),
                    "private inter-agent content",
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;
        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    AgentPath::root(),
                    "another private message",
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;
        input_queue
            .enqueue_lead_oversight_communication(
                make_mail(
                    AgentPath::try_from("/root/lead").expect("agent path"),
                    AgentPath::root(),
                    "private scheduler wake content",
                    /*trigger_turn*/ true,
                ),
                Default::default(),
            )
            .await;
        input_queue
            .enqueue_team_lead_progress_summary(
                make_mail(
                    AgentPath::root(),
                    AgentPath::root(),
                    "private manager progress summary",
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;
        input_queue
            .enqueue_team_lead_completion(make_mail(
                AgentPath::try_from("/root/worker").expect("agent path"),
                AgentPath::root(),
                "private completion content",
                /*trigger_turn*/ false,
            ))
            .await;

        let diagnostics = input_queue
            .pending_handoff_blocker_diagnostics("thread-id")
            .await;

        assert_eq!(diagnostics.len(), 4);
        let mailbox = diagnostics
            .iter()
            .find(|entry| entry.source == PendingMailboxBlockerSource::InterAgentMailbox)
            .expect("queued mail diagnostic");
        assert_eq!(mailbox.thread_id, "thread-id");
        assert_eq!(mailbox.count, 2);
        assert!(mailbox.oldest_age_ms.is_some());
        let lead_oversight = diagnostics
            .iter()
            .find(|entry| entry.source == PendingMailboxBlockerSource::LeadOversightWake)
            .expect("scheduler wake diagnostic");
        assert_eq!(lead_oversight.count, 1);
        assert!(lead_oversight.oldest_age_ms.is_some());
        let progress_summary = diagnostics
            .iter()
            .find(|entry| entry.source == PendingMailboxBlockerSource::LeadProgressSummary)
            .expect("manager progress summary diagnostic");
        assert_eq!(progress_summary.count, 1);
        assert!(progress_summary.oldest_age_ms.is_some());
        let completion = diagnostics
            .iter()
            .find(|entry| entry.source == PendingMailboxBlockerSource::ManagerCompletionBatch)
            .expect("completion diagnostic");
        assert_eq!(completion.count, 1);
        assert!(completion.oldest_age_ms.is_some());

        let serialized = serde_json::to_string(&diagnostics).expect("serialize diagnostics");
        assert!(!serialized.contains("private inter-agent content"));
        assert!(!serialized.contains("another private message"));
        assert!(!serialized.contains("private scheduler wake content"));
        assert!(!serialized.contains("private manager progress summary"));
        assert!(!serialized.contains("private completion content"));
    }

    #[tokio::test]
    async fn requeue_preserves_mailbox_source_and_enqueue_age() {
        let input_queue = InputQueue::new();
        let older_pending_input = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "older pending input",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_team_lead_progress_summary(
                make_mail(
                    AgentPath::root(),
                    AgentPath::root(),
                    "private manager progress summary",
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;
        let original_enqueued_at = {
            let mut mailbox = input_queue.mailbox_pending_mails.lock().await;
            let mail = mailbox.front_mut().expect("queued progress summary");
            mail.enqueued_at = Instant::now() - Duration::from_secs(9);
            mail.enqueued_at
        };

        let drained = input_queue
            .drain_mailbox_input_items_with_team_lead_marker()
            .await;
        input_queue
            .requeue_queue_only_mail(
                vec![TurnInput::InterAgentCommunication(
                    older_pending_input.clone(),
                )],
                drained,
            )
            .await;

        let mailbox = input_queue.mailbox_pending_mails.lock().await;
        assert_eq!(mailbox.len(), 2);
        let requeued_contents = mailbox
            .iter()
            .map(|mail| mail.communication.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            requeued_contents,
            ["older pending input", "private manager progress summary"]
        );
        let mail = mailbox.back().expect("requeued progress summary");
        assert!(mail.lead_progress_summary);
        assert_eq!(mail.enqueued_at, original_enqueued_at);
        drop(mailbox);

        let diagnostics = input_queue
            .pending_handoff_blocker_diagnostics("thread-id")
            .await;
        assert_eq!(diagnostics.len(), 2);
        let progress_summary = diagnostics
            .iter()
            .find(|entry| entry.source == PendingMailboxBlockerSource::LeadProgressSummary)
            .expect("manager progress summary diagnostic");
        assert_eq!(
            progress_summary.source,
            PendingMailboxBlockerSource::LeadProgressSummary
        );
        assert_eq!(progress_summary.count, 1);
        assert!(
            progress_summary
                .oldest_age_ms
                .is_some_and(|age| age >= 9_000)
        );
    }

    #[tokio::test]
    async fn input_queue_notifies_mailbox_subscribers() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending_activity, None);

        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_one, Default::default())
            .await;
        let mail_two = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "two",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_two, Default::default())
            .await;

        activity_rx.changed().await.expect("mailbox update");
        assert_eq!(
            *activity_rx.borrow_and_update(),
            InputQueueActivity::Mailbox
        );
    }

    #[tokio::test]
    async fn input_queue_notifies_steer_subscribers() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;
        assert_eq!(pending_activity, None);

        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    acceptance_order: None,
                    content: vec![UserInput::Text {
                        text: "steer".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        activity_rx.changed().await.expect("steer update");
        assert_eq!(*activity_rx.borrow_and_update(), InputQueueActivity::Steer);
    }

    #[tokio::test]
    async fn input_queue_reports_already_pending_steer() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let passive_output = serde_json::from_value(serde_json::json!({
            "ResponseItem": {"type": "function_call_output", "name": "notify", "output": "passive"}
        }))
        .unwrap();
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, vec![passive_output])
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            None
        );
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    acceptance_order: None,
                    content: vec![UserInput::Text {
                        text: "already pending".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        let (_activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;

        assert_eq!(pending_activity, Some(InputQueueActivity::Steer));
    }

    #[test]
    fn turn_input_queue_distinguishes_user_and_automatic_pending_input() {
        let automatic_output = TurnInput::FunctionCallOutput(ResponseItem::Other);
        let user_input = TurnInput::UserInput {
            acceptance_order: None,
            content: vec![UserInput::Text {
                text: "user steer".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        };

        assert_eq!(
            TurnInputQueue {
                items: vec![automatic_output, user_input],
            }
            .status(),
            PendingInputStatus {
                has_pending_input: true,
                has_user_input: true,
                has_pending_response_items: false,
            }
        );
        assert_eq!(
            TurnInputQueue {
                items: vec![TurnInput::FunctionCallOutput(ResponseItem::Other)],
            }
            .status(),
            PendingInputStatus {
                has_pending_input: true,
                has_user_input: false,
                has_pending_response_items: false,
            }
        );
        assert_eq!(
            TurnInputQueue {
                items: vec![TurnInput::ResponseItem(ResponseItem::Other.into())],
            }
            .status(),
            PendingInputStatus {
                has_pending_input: false,
                has_user_input: false,
                has_pending_response_items: true,
            }
        );
    }

    #[tokio::test]
    async fn input_queue_drains_mailbox_in_delivery_order() {
        let input_queue = InputQueue::new();
        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        let mail_two = make_mail(
            AgentPath::try_from("/root/worker_a").expect("agent path"),
            AgentPath::root(),
            "two",
            /*trigger_turn*/ true,
        );
        let mail_three = make_mail(
            AgentPath::try_from("/root/worker_b").expect("agent path"),
            AgentPath::root(),
            "three",
            /*trigger_turn*/ true,
        );

        input_queue
            .enqueue_mailbox_communication(mail_one.clone(), Default::default())
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone(), Default::default())
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_three.clone(), Default::default())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await.0,
            vec![
                TurnInput::InterAgentCommunication(mail_one),
                TurnInput::InterAgentCommunication(mail_two),
                TurnInput::InterAgentCommunication(mail_three),
            ]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_uses_unambiguous_trigger_parent_and_first_root() {
        let (parent, peer, root, root2) = (Some("a"), Some("b"), Some("r"), Some("s"));
        for (pending_mails, expected_parent_turn_id, expected_root_turn_id) in [
            (Vec::new(), None, None),
            (vec![(false, Some("q"), root)], None, None),
            (vec![(true, Some(""), root)], None, None),
            (vec![(true, Some("   "), root)], None, None),
            (vec![(true, None, root)], None, None),
            (vec![(true, parent, None)], parent, None),
            (vec![(true, parent, Some(""))], parent, None),
            (vec![(true, parent, root), (true, peer, root)], None, root),
            (vec![(true, parent, root), (true, peer, root2)], None, root),
            (vec![(true, parent, root), (true, None, root)], None, root),
            (
                vec![(true, parent, root), (true, parent, root)],
                parent,
                root,
            ),
            (
                vec![(false, Some("q"), root2), (true, parent, root)],
                parent,
                root,
            ),
        ] {
            let input_queue = InputQueue::new();
            for (trigger_turn, parent_turn_id, root_turn_id) in pending_mails {
                input_queue
                    .enqueue_mailbox_communication(
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn),
                        TurnStartOptions {
                            parent_turn_id: parent_turn_id.map(str::to_string),
                            root_turn_id: root_turn_id.map(str::to_string),
                            ..Default::default()
                        },
                    )
                    .await;
            }
            let (_, start_options) = input_queue.drain_mailbox_input_items().await;
            assert_eq!(
                start_options.parent_turn_id.as_deref(),
                expected_parent_turn_id
            );
            assert_eq!(start_options.root_turn_id.as_deref(), expected_root_turn_id);
        }
    }

    #[tokio::test]
    async fn input_queue_uses_latest_followup_choice_and_ignores_queue_only_mail() {
        use codex_protocol::turn_input::CyberAccessProgram;

        for latest in [Some(CyberAccessProgram::Standard), None] {
            let input_queue = InputQueue::new();
            for (trigger_turn, program) in [
                (true, Some(CyberAccessProgram::DaybreakBlue)),
                (true, latest),
                (false, Some(CyberAccessProgram::DaybreakRed)),
            ] {
                input_queue
                    .enqueue_mailbox_communication(
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn),
                        TurnStartOptions {
                            cyber_access_program: program,
                            ..Default::default()
                        },
                    )
                    .await;
            }
            let (_, start_options) = input_queue.drain_mailbox_input_items().await;
            assert_eq!(start_options.cyber_access_program, latest);
        }
    }

    #[tokio::test]
    async fn input_queue_tracks_pending_trigger_turn_mail() {
        let input_queue = InputQueue::new();

        let queued_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "queued",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(queued_mail, Default::default())
            .await;
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        let trigger_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "wake",
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(trigger_mail, Default::default())
            .await;
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }

    #[tokio::test]
    async fn manager_completion_batch_stays_bounded_and_drains_once() {
        let input_queue = InputQueue::new();
        let worker = AgentPath::try_from("/root/worker").expect("agent path");
        let latest_generation = input_queue
            .enqueue_team_lead_completion(make_mail(
                worker,
                AgentPath::root(),
                "completion result",
                /*trigger_turn*/ false,
            ))
            .await;

        assert!(input_queue.has_pending_manager_completion().await);
        assert_eq!(
            input_queue.take_team_progress_summary().await,
            Some(
                "Routine Worker progress summary (1 retained updates):\n- /root/worker: completion result"
                    .to_string()
            )
        );
        assert!(!input_queue.has_pending_manager_completion().await);
        assert!(
            input_queue
                .take_manager_completion_batch(latest_generation)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn explicit_progress_drain_races_atomically_with_completion_batch_claim() {
        let input_queue = std::sync::Arc::new(InputQueue::new());
        let generation = input_queue
            .enqueue_team_lead_completion(make_mail(
                AgentPath::try_from("/root/worker").expect("agent path"),
                AgentPath::root(),
                "completion result",
                /*trigger_turn*/ false,
            ))
            .await;
        let start = std::sync::Arc::new(tokio::sync::Barrier::new(3));

        let batch_queue = std::sync::Arc::clone(&input_queue);
        let batch_start = std::sync::Arc::clone(&start);
        let batch_claim = tokio::spawn(async move {
            batch_start.wait().await;
            batch_queue.take_manager_completion_batch(generation).await
        });

        let user_queue = std::sync::Arc::clone(&input_queue);
        let user_start = std::sync::Arc::clone(&start);
        let user_drain = tokio::spawn(async move {
            user_start.wait().await;
            user_queue.take_team_progress_summary().await
        });

        start.wait().await;
        let batch = batch_claim.await.expect("batch claim task");
        let user_summary = user_drain.await.expect("user drain task");

        assert_ne!(
            batch.is_some(),
            user_summary.is_some(),
            "the queue lock must give the completion summary to exactly one consumer"
        );
        if let Some(batch) = batch {
            assert!(
                batch
                    .progress_summary
                    .as_deref()
                    .is_some_and(|summary| summary.contains("completion result"))
            );
        }
        assert!(!input_queue.has_pending_manager_completion().await);
    }

    #[tokio::test]
    async fn dependency_free_wait_handoff_is_one_shot_until_new_work_arrives() {
        let input_queue = InputQueue::new();

        assert!(
            input_queue
                .claim_dependency_free_wait_handoff("turn-1")
                .await
        );
        assert!(
            !input_queue
                .claim_dependency_free_wait_handoff("turn-1")
                .await
        );
        assert!(
            input_queue
                .claim_dependency_free_wait_handoff("turn-2")
                .await
        );

        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    "routine",
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;
        assert!(
            !input_queue
                .claim_dependency_free_wait_handoff("turn-2")
                .await
        );

        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    "follow-up",
                    /*trigger_turn*/ true,
                ),
                Default::default(),
            )
            .await;
        assert!(
            input_queue
                .claim_dependency_free_wait_handoff("turn-2")
                .await
        );
    }

    #[tokio::test]
    async fn routine_lead_progress_does_not_publish_mailbox_activity() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending_activity, None);
        input_queue
            .enqueue_team_lead_progress(make_mail(
                AgentPath::try_from("/root/worker").expect("agent path"),
                AgentPath::root(),
                "routine update",
                /*trigger_turn*/ false,
            ))
            .await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), activity_rx.changed())
                .await
                .is_err()
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn routine_lead_progress_is_bounded_and_flushed_as_summary() {
        let input_queue = InputQueue::new();
        for index in 0..(TEAM_LEAD_PROGRESS_MAX_ITEMS + 8) {
            input_queue
                .enqueue_team_lead_progress(make_mail(
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    AgentPath::root(),
                    &format!("update {index} {}", "x".repeat(600)),
                    /*trigger_turn*/ false,
                ))
                .await;
        }
        let summary = input_queue
            .take_team_progress_summary()
            .await
            .expect("progress summary");
        assert!(summary.len() <= TEAM_LEAD_PROGRESS_SUMMARY_MAX_BYTES);
        assert!(summary.contains("retained updates"));
        assert!(summary.contains("update 39"));
        assert!(input_queue.take_team_progress_summary().await.is_none());
    }

    #[tokio::test]
    async fn clearing_oversight_mail_preserves_non_waking_progress_summary() {
        let input_queue = InputQueue::new();
        let progress = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "retained progress",
            /*trigger_turn*/ false,
        );
        let deadline = make_mail(
            AgentPath::root(),
            AgentPath::root(),
            "deadline wake",
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(progress.clone(), Default::default())
            .await;
        input_queue
            .enqueue_lead_oversight_communication(deadline, Default::default())
            .await;

        input_queue.clear_lead_oversight_mailbox().await;
        let (items, _) = input_queue.drain_mailbox_input_items().await;
        assert_eq!(items, vec![TurnInput::InterAgentCommunication(progress)]);
    }

    #[tokio::test]
    async fn clearing_team_triggers_preserves_queue_only_mail() {
        let input_queue = InputQueue::new();
        let queued = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "queued progress",
            /*trigger_turn*/ false,
        );
        let trigger = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "stale completion",
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(queued.clone(), Default::default())
            .await;
        input_queue
            .enqueue_mailbox_communication(trigger, Default::default())
            .await;

        input_queue.clear_trigger_turn_mailbox().await;

        assert!(!input_queue.has_trigger_turn_mailbox_items().await);
        let (items, _) = input_queue.drain_mailbox_input_items().await;
        assert_eq!(items, vec![TurnInput::InterAgentCommunication(queued)]);
    }
}
