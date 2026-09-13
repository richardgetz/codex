use super::*;
use chrono::Duration;
use pretty_assertions::assert_eq;

fn task(now: DateTime<Utc>) -> TaskEstimate {
    TaskEstimate {
        task_id: "task".to_string(),
        root_thread_id: ThreadId::new(),
        owner_thread_id: ThreadId::new(),
        parent_task_id: None,
        depends_on_task_ids: Vec::new(),
        title: "Task".to_string(),
        status: TaskEstimateStatus::Active,
        current_lower_seconds: Some(10),
        current_upper_seconds: Some(20),
        original_lower_seconds: Some(10),
        original_upper_seconds: Some(20),
        created_at: now,
        started_at: Some(now),
        terminal_at: None,
        actual_elapsed_seconds: None,
        updated_at: now,
        revisions: Vec::new(),
    }
}

#[test]
fn remaining_range_clamps_overdue_estimates_to_zero() {
    let started_at = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("timestamp");
    let remaining = task(started_at).remaining_range(started_at + Duration::seconds(30));
    assert_eq!(
        remaining,
        TaskEstimateRange {
            lower_seconds: Some(0),
            upper_seconds: Some(0),
        }
    );
}
