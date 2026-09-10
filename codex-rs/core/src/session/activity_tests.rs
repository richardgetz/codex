use super::classify_active_turn_activity;
use codex_protocol::protocol::ThreadActivity;
use codex_protocol::protocol::ThreadActivityWaitReason;
use pretty_assertions::assert_eq;

#[test]
fn lead_activity_prioritizes_admitted_operation_over_worker_wait() {
    assert_eq!(
        classify_active_turn_activity(
            /*in_flight_operations*/ 1, /*active_direct_worker_count*/ 1
        ),
        (ThreadActivity::Working, None)
    );
}

#[test]
fn lead_activity_reports_agents_wait_after_operations_quiesce() {
    assert_eq!(
        classify_active_turn_activity(
            /*in_flight_operations*/ 0, /*active_direct_worker_count*/ 1
        ),
        (
            ThreadActivity::Waiting,
            Some(ThreadActivityWaitReason::Agents)
        )
    );
}
