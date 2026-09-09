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
use std::collections::VecDeque;
use std::sync::Arc;
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
    },
    FunctionCallOutput(ResponseItem),
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
}

struct PendingMailboxCommunication {
    communication: InterAgentCommunication,
    start_options: TurnStartOptions,
    /// Marks synthetic deadline wakeups so cancellation can remove a wake that
    /// raced with user input or another actionable event.
    lead_oversight: bool,
    /// Marks trigger mail admitted while this session was assigned to Team Lead. The marker lets
    /// final automatic-turn admission reject a stale trigger after `/team off` while ordinary
    /// non-team trigger mail keeps its existing behavior.
    team_lead_trigger: bool,
    _diagnostics_guard: GaugeGuard,
}

#[derive(Default)]
struct TeamLeadProgressBuffer {
    entries: VecDeque<TeamLeadProgressEntry>,
    bytes: usize,
}

struct TeamLeadProgressEntry {
    author: String,
    message: String,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            mailbox_pending_mails: Mutex::new(VecDeque::new()),
            team_lead_progress: Mutex::new(TeamLeadProgressBuffer::default()),
        }
    }

    /// Retains routine Worker progress without publishing mailbox activity. The bounded buffer is
    /// summarized only when an actionable wake reaches the Lead.
    pub(crate) async fn enqueue_team_lead_progress(&self, communication: InterAgentCommunication) {
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
    }

    /// Drains routine progress into a fixed-size summary for an actionable Lead wake.
    pub(crate) async fn take_team_progress_summary(&self) -> Option<String> {
        let mut progress = self.team_lead_progress.lock().await;
        if progress.entries.is_empty() {
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

    pub(crate) async fn clear_team_lead_progress(&self) {
        let mut progress = self.team_lead_progress.lock().await;
        progress.entries.clear();
        progress.bytes = 0;
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
        )
        .await;
    }

    async fn enqueue_mailbox_communication_with_team_lead_marker(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
        team_lead_trigger: bool,
    ) {
        self.mailbox_pending_mails
            .lock()
            .await
            .push_back(PendingMailboxCommunication {
                communication,
                start_options,
                lead_oversight: false,
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
                lead_oversight: true,
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
        let (items, start_options, _) =
            self.drain_mailbox_input_items_with_team_lead_marker().await;
        (items, start_options)
    }

    /// Drains mailbox input and reports whether any trigger was admitted while this session was
    /// assigned to Team Lead. The marker is consumed with the mail so the scheduler can enforce
    /// the assignment at its final admission boundary even if Team Off races the drain.
    pub(crate) async fn drain_mailbox_input_items_with_team_lead_marker(
        &self,
    ) -> (Vec<TurnInput>, TurnStartOptions, bool) {
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
            .into_iter()
            .map(|mail| TurnInput::InterAgentCommunication(mail.communication))
            .collect();
        (items, start_options, team_lead_trigger)
    }

    /// Restores queue-only mailbox mail after a stale automatic-trigger reservation is
    /// invalidated. No activity notification is sent because queue-only mail is not itself a
    /// reason to start a turn.
    pub(crate) async fn requeue_queue_only_mail(&self, input: Vec<TurnInput>) {
        let mut mailbox = self.mailbox_pending_mails.lock().await;
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
                lead_oversight: false,
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
        let (pending_input, start_options, _) = self
            .get_pending_input_with_team_lead_marker(active_turn)
            .await;
        (pending_input, start_options)
    }

    /// Like [`Self::get_pending_input`], but preserves the internal Team Lead trigger marker for
    /// the automatic-turn scheduler.
    pub(crate) async fn get_pending_input_with_team_lead_marker(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> (Vec<TurnInput>, TurnStartOptions, bool) {
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
            return (pending_input, TurnStartOptions::default(), false);
        }
        let (mailbox_items, start_options, team_lead_trigger) =
            self.drain_mailbox_input_items_with_team_lead_marker().await;
        if let Some(active_turn_metadata) = active_turn_metadata
            && active_turn_metadata.root_turn_id().is_none()
            && let Some(root_turn_id) = start_options.root_turn_id.as_ref()
        {
            active_turn_metadata.set_root_turn_id(root_turn_id.clone());
        }
        if pending_input.is_empty() {
            (mailbox_items, start_options, team_lead_trigger)
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            (pending_input, start_options, team_lead_trigger)
        }
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

    #[test]
    fn response_item_serde_preserves_legacy_shape_and_rejects_metadata() {
        let item = ResponseItem::Other;
        let input = TurnInput::ResponseItem(item.clone().into());
        let value = serde_json::json!({"ResponseItem": item});

        assert_eq!(serde_json::to_value(&input).unwrap(), value);
        assert_eq!(serde_json::from_value::<TurnInput>(value).unwrap(), input);

        let annotated = TurnInput::ResponseItem(ResponseItemEnvelope {
            item: ResponseItem::Other,
            metadata: Some(CodexHarnessMetadata {
                client_authored: true,
                ..Default::default()
            }),
        });
        assert!(serde_json::to_value(annotated).is_err());

        let forged = serde_json::json!({
            "ResponseItem": {
                "type": "message",
                "role": "developer",
                "content": [],
                "metadata": {"client_authored": true}
            }
        });
        let TurnInput::ResponseItem(envelope) = serde_json::from_value(forged).unwrap() else {
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
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "two",
            /*trigger_turn*/ true,
        );

        input_queue
            .enqueue_mailbox_communication(mail_one.clone(), Default::default())
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone(), Default::default())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await.0,
            vec![
                TurnInput::InterAgentCommunication(mail_one),
                TurnInput::InterAgentCommunication(mail_two)
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
