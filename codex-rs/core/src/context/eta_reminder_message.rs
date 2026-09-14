//! Bounded contextual payloads for owner-routed ETA reminders.

use super::ContextualUserFragment;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::AgentPath;
use codex_state::TaskEstimate;

const MAX_REMINDER_BYTES: usize = 768;

#[derive(Clone, Copy)]
pub(crate) enum ReminderTrigger {
    Freshness,
    Overdue,
}

impl ReminderTrigger {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Freshness => "freshness",
            Self::Overdue => "overdue",
        }
    }
}

/// A model-visible ETA reminder that carries the normal bounded inter-agent fragment metadata.
/// Keep the body short enough that the complete contextual item remains below the manual review
/// threshold for injected fragments.
pub(crate) struct EtaReminderMessage {
    task_name: AgentPath,
    payload: String,
}

impl EtaReminderMessage {
    pub(crate) fn new(task_name: AgentPath, payload: String) -> Self {
        Self { task_name, payload }
    }
}

impl ContextualUserFragment for EtaReminderMessage {
    fn content_kind(&self) -> codex_protocol::models::ContentItemKind {
        codex_protocol::models::ContentItemKind("multi_agent.eta_reminder".to_string())
    }

    fn role(&self) -> &'static str {
        "assistant"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        truncate_message(&format!(
            "Message Type: MESSAGE\nTask name: {}\nSender: root\nPayload:\n{}",
            self.task_name, self.payload
        ))
    }
}

pub(crate) fn format_reminder(
    task: &TaskEstimate,
    trigger: ReminderTrigger,
    now: DateTime<Utc>,
) -> String {
    let remaining = task.remaining_range(now);
    let range = format_range(remaining.lower_seconds, remaining.upper_seconds);
    let saved = format_range(task.current_lower_seconds, task.current_upper_seconds);
    truncate_message(&format!(
        "ETA reminder ({})\nTask: {} — {}\nOwner: {}\nStatus: {}\nSaved range: {} seconds\nRemaining from now: {} seconds\nReassess the work now and call update_eta with the truthful remaining range plus a short change or blocker reason. Mark complete or cancel only when that lifecycle state is true; elapsed time never completes work.",
        trigger.label(),
        task.task_id,
        task.title,
        task.owner_thread_id,
        task.status.as_str(),
        saved,
        range,
    ))
}

fn format_range(lower: Option<i64>, upper: Option<i64>) -> String {
    match (lower, upper) {
        (Some(lower), Some(upper)) => format!("{lower}–{upper}"),
        (Some(lower), None) => format!("{lower}–unknown"),
        (None, Some(upper)) => format!("unknown–{upper}"),
        (None, None) => "unknown".to_string(),
    }
}

fn truncate_message(message: &str) -> String {
    if message.len() <= MAX_REMINDER_BYTES {
        return message.to_string();
    }
    let mut truncated = String::new();
    for character in message.chars() {
        if truncated.len() + character.len_utf8() + '…'.len_utf8() > MAX_REMINDER_BYTES {
            break;
        }
        truncated.push(character);
    }
    truncated.push('…');
    truncated
}

#[cfg(test)]
#[path = "eta_reminder_message_tests.rs"]
mod tests;
