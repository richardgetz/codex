use super::EtaReminderMessage;
use super::MAX_REMINDER_BYTES;
use super::ReminderTrigger;
use super::format_reminder;
use super::truncate_message;
use crate::context::ContextualUserFragment;
use chrono::Duration as ChronoDuration;
use chrono::Utc;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_state::TaskEstimate;
use codex_state::TaskEstimateStatus;

#[test]
fn trigger_labels_are_stable() {
    assert_eq!(ReminderTrigger::Freshness.label(), "freshness");
    assert_eq!(ReminderTrigger::Overdue.label(), "overdue");
}

#[test]
fn reminder_message_preserves_saved_and_from_now_ranges() {
    let now = Utc::now();
    let task = TaskEstimate {
        task_id: "task-1".to_string(),
        root_thread_id: ThreadId::new(),
        owner_thread_id: ThreadId::new(),
        parent_task_id: None,
        depends_on_task_ids: Vec::new(),
        title: "Review changes".to_string(),
        status: TaskEstimateStatus::Active,
        current_lower_seconds: Some(10),
        current_upper_seconds: Some(20),
        original_lower_seconds: Some(10),
        original_upper_seconds: Some(20),
        created_at: now - ChronoDuration::seconds(5),
        started_at: Some(now - ChronoDuration::seconds(5)),
        terminal_at: None,
        actual_elapsed_seconds: None,
        updated_at: now - ChronoDuration::seconds(5),
        revisions: Vec::new(),
    };
    let payload = format_reminder(&task, ReminderTrigger::Freshness, now);
    assert!(payload.contains("Task: task-1 — Review changes"));
    assert!(payload.contains("Saved range: 10–20 seconds"));
    assert!(payload.contains("Remaining from now: 5–15 seconds"));
    assert!(payload.contains("truthful remaining range"));
    assert!(payload.contains("elapsed time never completes"));
    let fragment = EtaReminderMessage::new(AgentPath::root(), payload);
    assert!(fragment.render().contains("Message Type: MESSAGE"));
    assert_eq!(fragment.content_kind().0, "multi_agent.eta_reminder");
}

#[test]
fn reminder_message_is_bounded() {
    let message = "x".repeat(2_000);
    let truncated = truncate_message(&message);
    assert!(truncated.len() <= MAX_REMINDER_BYTES);
    assert!(truncated.ends_with('…'));

    let long_owner = AgentPath::try_from(format!("/root/{}worker", "worker/".repeat(199)))
        .expect("long owner path should remain a valid path");
    let fragment = EtaReminderMessage::new(long_owner, message);
    assert!(fragment.body().len() <= MAX_REMINDER_BYTES);
}
