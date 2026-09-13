use super::*;
use chrono::Duration;
use codex_state::TaskEstimateSnapshot;
use pretty_assertions::assert_eq;

fn task(now: DateTime<Utc>, status: TaskEstimateStatus) -> TaskEstimate {
    TaskEstimate {
        task_id: "task".to_string(),
        root_thread_id: ThreadId::new(),
        owner_thread_id: ThreadId::new(),
        parent_task_id: None,
        depends_on_task_ids: Vec::new(),
        title: "Task".to_string(),
        status,
        current_lower_seconds: Some(20),
        current_upper_seconds: Some(30),
        original_lower_seconds: Some(10),
        original_upper_seconds: Some(20),
        created_at: now,
        started_at: (status != TaskEstimateStatus::Pending).then_some(now),
        terminal_at: status.is_terminal().then_some(now + Duration::seconds(15)),
        actual_elapsed_seconds: status.is_terminal().then_some(15),
        updated_at: now,
        revisions: Vec::new(),
    }
}

#[test]
fn api_task_exposes_remaining_estimate_and_accuracy() {
    let now = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("timestamp");
    let active = task(now - Duration::seconds(25 * 60), TaskEstimateStatus::Active);
    let active_api = api_task(&active, now);
    assert!(active_api.is_stale);
    assert_eq!(
        (
            active_api.current_lower_seconds,
            active_api.current_upper_seconds,
            active_api.accuracy,
        ),
        (Some(0), Some(0), ThreadEtaAccuracy::Unknown),
    );

    let completed = task(now, TaskEstimateStatus::Completed);
    let completed_api = api_task(&completed, now);
    assert_eq!(completed_api.accuracy, ThreadEtaAccuracy::Within);
    assert_eq!(
        (
            completed_api.current_lower_seconds,
            completed_api.current_upper_seconds,
        ),
        (Some(20), Some(30)),
    );

    let cancelled = task(now, TaskEstimateStatus::Cancelled);
    assert_eq!(
        api_task(&cancelled, now).accuracy,
        ThreadEtaAccuracy::Unknown
    );
}

#[test]
fn api_snapshot_marks_stale_active_work_unknown() {
    let now = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("timestamp");
    let root = ThreadId::new();
    let stale = task(
        now - Duration::seconds(STALE_AFTER_SECONDS + 1),
        TaskEstimateStatus::Active,
    );
    let snapshot = TaskEstimateSnapshot {
        root_thread_id: root,
        generated_at: now,
        sequence: 3,
        active: vec![stale],
        history: Vec::new(),
        next_cursor: None,
        overall: TaskEstimateOverall {
            finish_at: Some(now + Duration::seconds(10)),
            remaining_lower_seconds: Some(1),
            remaining_upper_seconds: Some(10),
            unknown_reason: None,
        },
    };
    let api = api_snapshot(snapshot);
    assert_eq!(
        api.overall,
        ThreadEtaOverall {
            finish_at: None,
            remaining_lower_seconds: None,
            remaining_upper_seconds: None,
            unknown_reason: Some("stale task update".to_string()),
        }
    );
}
