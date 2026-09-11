//! Event-driven parking for an idle Lead while direct Workers continue.
//!
//! A parked Lead has no automatic model work queued. Worker progress is retained in the bounded
//! input-queue summary and only reaches the Lead when an actionable message or the oversight
//! deadline wakes it.

use super::session::Session;
use chrono::DateTime;
use chrono::Utc;
use codex_config::DEFAULT_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES;
use codex_config::TeamRole as ConfigTeamRole;
use codex_protocol::AgentPath;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::TeamMode;
use codex_protocol::protocol::WarningEvent;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::sleep_until;

const MAX_OVERSIGHT_MESSAGE_BYTES: usize = 1_024;

/// Distinguishes a new parking interval from an explicit wait inside the current Lead turn.
/// Explicit waits may reuse an existing interval but must not rearm a deadline that already woke
/// the Lead until the turn has completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeadIdleArmMode {
    CompletedLeadTurn,
    ExplicitWait,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LeadIdleDeadline {
    pub(crate) instant: Instant,
    pub(crate) unix_secs: i64,
}

/// Session-local state for one outstanding Lead oversight deadline.
///
/// A controller is reused for each parked interval. Claiming a deadline clears
/// the current interval; the next completed Lead assessment can arm another
/// one with a fresh generation.
pub(crate) struct LeadIdleController {
    state: Mutex<LeadIdleState>,
    session: std::sync::OnceLock<Weak<Session>>,
}

struct LeadIdleState {
    generation: u64,
    deadline: Option<Instant>,
    deadline_unix_secs: Option<i64>,
    rearm_allowed: bool,
    timer: Option<JoinHandle<()>>,
}

impl Default for LeadIdleState {
    fn default() -> Self {
        Self {
            generation: 0,
            deadline: None,
            deadline_unix_secs: None,
            rearm_allowed: true,
            timer: None,
        }
    }
}

impl Default for LeadIdleController {
    fn default() -> Self {
        Self {
            state: Mutex::new(LeadIdleState::default()),
            session: std::sync::OnceLock::new(),
        }
    }
}

impl LeadIdleController {
    pub(crate) fn set_session(&self, session: Weak<Session>) {
        let _ = self.session.set(session);
    }

    fn session(&self) -> Option<Weak<Session>> {
        self.session.get().cloned()
    }

    async fn cancel(&self) {
        let mut state = self.state.lock().await;
        state.generation = state.generation.wrapping_add(1);
        state.deadline = None;
        state.deadline_unix_secs = None;
        state.rearm_allowed = true;
        if let Some(timer) = state.timer.take() {
            timer.abort();
        }
    }

    async fn arm(
        &self,
        timeout: Duration,
        deadline_unix_secs: i64,
        session: Weak<Session>,
        mode: LeadIdleArmMode,
    ) -> Option<LeadIdleDeadline> {
        let mut state = self.state.lock().await;
        if let Some(deadline) = state.deadline {
            return Some(LeadIdleDeadline {
                instant: deadline,
                unix_secs: state.deadline_unix_secs.unwrap_or(deadline_unix_secs),
            });
        }
        if mode == LeadIdleArmMode::ExplicitWait && !state.rearm_allowed {
            return None;
        }
        let deadline = Instant::now().checked_add(timeout)?;
        if let Some(timer) = state.timer.take() {
            timer.abort();
        }
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.deadline = Some(deadline);
        state.deadline_unix_secs = Some(deadline_unix_secs);
        state.rearm_allowed = false;
        let timer = tokio::spawn(async move {
            sleep_until(deadline).await;
            if let Some(session) = session.upgrade() {
                session.handle_lead_oversight_deadline(generation).await;
            }
        });
        state.timer = Some(timer);
        Some(LeadIdleDeadline {
            instant: deadline,
            unix_secs: deadline_unix_secs,
        })
    }

    async fn claim_deadline(&self, generation: u64) -> bool {
        let mut state = self.state.lock().await;
        if state.generation != generation || state.deadline.is_none() {
            return false;
        }
        state.deadline = None;
        state.deadline_unix_secs = None;
        state.rearm_allowed = false;
        state.timer.take();
        true
    }

    async fn generation_is_current(&self, generation: u64) -> bool {
        self.state.lock().await.generation == generation
    }
}

impl Session {
    /// Returns whether this session currently owns the Lead assignment.
    pub(crate) async fn is_team_lead(&self) -> bool {
        let config = self.get_config().await;
        let source = self.session_source().await;
        config.team_mode == TeamMode::LeadWorker
            && crate::session::team::effective_role_for_session_source(&config, &source)
                == Some(ConfigTeamRole::Lead)
    }

    /// Cancels any outstanding Lead oversight deadline. Explicit user input, actionable worker
    /// mail, interruption, and team disablement all invalidate the parked generation.
    pub(crate) async fn cancel_lead_oversight(&self) {
        self.lead_idle_controller.cancel().await;
        self.input_queue.clear_lead_oversight_mailbox().await;
    }

    /// Arms the Lead oversight interval when direct Workers remain active. Ordinary progress does
    /// not call this method and therefore cannot extend the current deadline.
    pub(crate) async fn arm_lead_oversight(
        self: &Arc<Self>,
        mode: LeadIdleArmMode,
    ) -> Option<(usize, LeadIdleDeadline)> {
        self.lead_idle_controller.set_session(Arc::downgrade(self));
        self.arm_lead_oversight_with_session(mode, Arc::downgrade(self))
            .await
    }

    async fn arm_lead_oversight_with_session(
        &self,
        mode: LeadIdleArmMode,
        session: Weak<Session>,
    ) -> Option<(usize, LeadIdleDeadline)> {
        // Serialize timer installation with `/team off` cleanup. The role check alone is not
        // enough: a settings commit could otherwise publish Off between the check and the
        // controller arm, leaving a fresh deadline behind after cancellation.
        let _team_lead_turn_admission = self.team_lead_turn_admission.lock().await;
        if !self.is_team_lead().await || self.shutdown_requested() || self.is_interrupted() {
            self.cancel_lead_oversight().await;
            return None;
        }
        if self.is_activity_paused() {
            self.cancel_lead_oversight().await;
            return None;
        }
        if mode == LeadIdleArmMode::CompletedLeadTurn && self.active_turn.lock().await.is_some() {
            self.cancel_lead_oversight().await;
            return None;
        }
        let active_workers = self
            .services
            .agent_control
            .active_direct_worker_count(self.thread_id)
            .await;
        if active_workers == 0 {
            self.cancel_lead_oversight().await;
            return None;
        }
        if self.input_queue.has_trigger_turn_mailbox_items().await {
            // An actionable wake already awaits scheduling. Do not arm an oversight timer across
            // that assessment turn; the terminal turn handler will arm the next parked interval.
            self.cancel_lead_oversight().await;
            return None;
        }
        let config = self.get_config().await;
        let timeout_minutes = config
            .effective_team_profiles()
            .map(|profiles| profiles.lead_oversight_timeout_minutes)
            .unwrap_or(DEFAULT_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES);
        let Some(timeout_secs) = timeout_minutes.checked_mul(60) else {
            self.cancel_lead_oversight().await;
            return None;
        };
        let timeout = Duration::from_secs(timeout_secs);
        let now_unix_secs = crate::turn_timing::now_unix_timestamp_ms()
            .checked_div(1_000)
            .unwrap_or_default();
        let deadline_unix_secs =
            now_unix_secs.saturating_add(i64::try_from(timeout_secs).unwrap_or(i64::MAX));
        let deadline = self
            .lead_idle_controller
            .arm(timeout, deadline_unix_secs, session, mode)
            .await?;
        if mode == LeadIdleArmMode::CompletedLeadTurn && self.active_turn.lock().await.is_some() {
            self.cancel_lead_oversight().await;
            return None;
        }
        Some((active_workers, deadline))
    }

    /// Re-arms oversight after a live Team Off -> Team On transition when this session was
    /// already parked. The session's weak self-reference is installed at construction and avoids
    /// retaining a strong cycle through the timer task.
    pub(crate) async fn rearm_lead_oversight_after_team_enable(
        &self,
    ) -> Option<(usize, LeadIdleDeadline)> {
        let session = self.lead_idle_controller.session()?;
        self.arm_lead_oversight_with_session(LeadIdleArmMode::CompletedLeadTurn, session)
            .await
    }

    /// Drains the bounded routine-progress summary for an explicit user action. The caller can
    /// place the returned communication in the new turn's initial input, avoiding a second
    /// inference solely to deliver the summary.
    pub(crate) async fn take_lead_progress_summary(&self) -> Option<String> {
        self.input_queue.take_team_progress_summary().await
    }

    /// Returns whether passive Lead idle and parked-wait notifications are enabled.
    ///
    /// This is a global debugging preference from `[team.lead]`; actionable oversight deadline
    /// warnings remain visible regardless of this setting.
    pub(crate) async fn lead_idle_notifications_enabled(&self) -> bool {
        self.get_config().await.team.lead_show_idle_notifications
    }

    /// Drops buffered routine progress when team mode is disabled so stale updates do not leak
    /// into a later re-enabled Lead assignment.
    pub(crate) async fn clear_lead_progress(&self) {
        self.input_queue.clear_team_lead_progress().await;
    }

    /// Checks that a previously returned deadline still belongs to the current parked interval.
    /// This closes the setup race where cancellation happens before a wait subscribes to the
    /// activity watch and therefore cannot deliver the cancellation notification retroactively.
    pub(crate) async fn lead_oversight_deadline_is_current(&self, deadline: Instant) -> bool {
        self.lead_idle_controller.state.lock().await.deadline == Some(deadline)
    }

    /// Called after a Lead turn becomes idle. A deadline is armed only while direct Workers are
    /// still active; ordinary progress does not call this method and therefore cannot extend it.
    pub(crate) async fn update_lead_idle_after_turn(self: &Arc<Self>) {
        if let Some((active_workers, deadline)) = self
            .arm_lead_oversight(LeadIdleArmMode::CompletedLeadTurn)
            .await
            && self.lead_idle_notifications_enabled().await
        {
            self.emit_lead_idle_event(format_lead_idle_message(active_workers, deadline.unix_secs))
                .await;
        }
    }

    /// Handles one oversight wake for the current parked interval without polling. A stale or
    /// cancelled timer is a no-op; if all Workers finished before it fired there is no synthetic
    /// Lead turn. A subsequent completed Lead assessment may arm a fresh interval.
    pub(crate) async fn handle_lead_oversight_deadline(self: &Arc<Self>, generation: u64) {
        if !self.lead_idle_controller.claim_deadline(generation).await
            || !self.is_team_lead().await
            || self.is_activity_paused()
            || self.shutdown_requested()
            || self.is_interrupted()
        {
            return;
        }
        let active_workers = self
            .services
            .agent_control
            .active_direct_worker_count(self.thread_id)
            .await;
        if active_workers == 0 {
            return;
        }
        if !self
            .lead_idle_controller
            .generation_is_current(generation)
            .await
        {
            return;
        }
        self.emit_lead_idle_event(format!(
            "Lead oversight deadline reached; waking for review while {active_workers} direct Worker(s) remain active.",
        ))
        .await;
        if !self
            .lead_idle_controller
            .generation_is_current(generation)
            .await
        {
            return;
        }
        if !self
            .enqueue_lead_oversight_wakeup(
                generation,
            "The Lead oversight deadline has elapsed. Review Worker progress, resolve blockers, or escalate the work now.",
            )
            .await
        {
            return;
        }
        self.maybe_start_turn_for_pending_work().await;
    }

    /// Enqueues an actionable Lead wake and flushes the bounded routine-progress summary first.
    /// This is shared by the deadline and target-side message handling paths.
    pub(crate) async fn enqueue_lead_wakeup(&self, message: &str) {
        // Keep the summary and wake in the same admission boundary as Team Off cleanup. V1
        // completion notifications call this helper directly, so the marker cannot be inferred
        // by the outer inter-agent handler.
        let _team_lead_turn_admission = self.team_lead_turn_admission.lock().await;
        if !self.is_team_lead().await {
            return;
        }
        if let Some(summary) = self.input_queue.take_team_progress_summary().await {
            self.input_queue
                .enqueue_team_lead_mailbox_communication(
                    lead_progress_communication(summary),
                    Default::default(),
                )
                .await;
        }
        self.input_queue
            .enqueue_team_lead_mailbox_communication(
                InterAgentCommunication::new(
                    AgentPath::root(),
                    AgentPath::root(),
                    Vec::new(),
                    truncate_message(message),
                    true,
                ),
                Default::default(),
            )
            .await;
    }

    /// Enqueues a deadline wake while holding the controller generation lock.
    /// Cancellation takes that same lock before removing synthetic messages,
    /// so a cancelled timer cannot enqueue a stale trigger after the cleanup.
    async fn enqueue_lead_oversight_wakeup(&self, generation: u64, message: &str) -> bool {
        // Keep synthetic trigger insertion in the same boundary as Team Off cleanup and other
        // actionable mailbox insertion. This prevents a deadline wake from being cleared or
        // stranded between the mailbox drain and idle-sentinel cleanup.
        let _team_lead_turn_admission = self.team_lead_turn_admission.lock().await;
        if !self.is_team_lead().await || self.is_activity_paused() {
            return false;
        }
        let state = self.lead_idle_controller.state.lock().await;
        if state.generation != generation {
            return false;
        }
        if self
            .services
            .agent_control
            .active_direct_worker_count(self.thread_id)
            .await
            == 0
        {
            return false;
        }
        if let Some(summary) = self.input_queue.take_team_progress_summary().await {
            self.input_queue
                .enqueue_team_lead_mailbox_communication(
                    InterAgentCommunication::new(
                        AgentPath::root(),
                        AgentPath::root(),
                        Vec::new(),
                        summary,
                        false,
                    ),
                    Default::default(),
                )
                .await;
        }
        self.input_queue
            .enqueue_lead_oversight_communication(
                InterAgentCommunication::new(
                    AgentPath::root(),
                    AgentPath::root(),
                    Vec::new(),
                    truncate_message(message),
                    true,
                ),
                Default::default(),
            )
            .await;
        true
    }

    pub(crate) async fn emit_lead_idle_event(&self, message: String) {
        self.send_event_raw_without_materializing_rollout(Event {
            id: format!("lead-idle-{}", crate::turn_timing::now_unix_timestamp_ms()),
            msg: EventMsg::Warning(WarningEvent { message }),
        })
        .await;
    }
}

pub(crate) fn truncate_message(message: &str) -> String {
    if message.len() <= MAX_OVERSIGHT_MESSAGE_BYTES {
        return message.to_string();
    }
    let mut result = String::new();
    for character in message.chars() {
        if result.len() + character.len_utf8() + '…'.len_utf8() > MAX_OVERSIGHT_MESSAGE_BYTES {
            break;
        }
        result.push(character);
    }
    result.push('…');
    result
}

pub(crate) fn format_lead_idle_message(active_workers: usize, deadline_unix_secs: i64) -> String {
    format!(
        "Lead idle while {active_workers} direct Worker(s) run; routine progress will not trigger inference. Next oversight deadline: {}.",
        format_deadline(deadline_unix_secs)
    )
}

pub(crate) fn format_lead_wait_message(active_workers: usize, deadline_unix_secs: i64) -> String {
    format!(
        "Lead wait parked while {active_workers} direct Worker(s) run; routine progress will not trigger inference. Next oversight deadline: {}.",
        format_deadline(deadline_unix_secs)
    )
}

pub(crate) fn lead_progress_communication(summary: String) -> InterAgentCommunication {
    InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root(),
        Vec::new(),
        summary,
        true,
    )
}

fn format_deadline(deadline_unix_secs: i64) -> String {
    DateTime::<Utc>::from_timestamp(deadline_unix_secs, 0)
        .map(|timestamp| {
            format!(
                "{} ({deadline_unix_secs} Unix seconds)",
                timestamp.to_rfc3339()
            )
        })
        .unwrap_or_else(|| format!("{deadline_unix_secs} Unix seconds"))
}

#[cfg(test)]
#[path = "lead_idle_tests.rs"]
mod tests;
