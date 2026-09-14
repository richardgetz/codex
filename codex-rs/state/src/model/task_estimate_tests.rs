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

#[test]
fn freshness_delay_uses_configured_minimum_or_rounded_quarter() {
    let now = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("timestamp");
    let mut estimate = task(now);

    // 45 minutes / 4 is 11m15s, so the 15-minute minimum wins.
    estimate.current_upper_seconds = Some(45 * 60);
    assert_eq!(estimate.freshness_delay_seconds(15 * 60), 15 * 60);

    // A two-hour estimate extends the freshness delay to 30 minutes.
    estimate.current_upper_seconds = Some(2 * 60 * 60);
    assert_eq!(estimate.freshness_delay_seconds(15 * 60), 30 * 60);

    // Round a fractional quarter upward so a 5-second upper bound cannot wake at 1 second.
    estimate.current_upper_seconds = Some(5);
    assert_eq!(estimate.freshness_delay_seconds(0), 2);

    // Unknown upper bounds use only the configured minimum.
    estimate.current_upper_seconds = None;
    assert_eq!(estimate.freshness_delay_seconds(15 * 60), 15 * 60);
}

#[test]
fn duration_validation_rejects_values_beyond_supported_horizon() {
    let result = TaskEstimateRange {
        lower_seconds: Some(0),
        upper_seconds: Some(MAX_ESTIMATE_SECONDS + 1),
    }
    .validate();
    assert!(result.is_err());
}
