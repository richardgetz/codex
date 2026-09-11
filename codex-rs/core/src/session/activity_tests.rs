use super::classify_active_turn_activity;
use crate::session::tests::make_session_and_context;
use codex_protocol::protocol::ThreadActivity;
use codex_protocol::protocol::ThreadActivityWaitReason;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_quiescence_waits_for_operation_guard() {
    let (session, _turn_context) = make_session_and_context().await;
    let session = Arc::new(session);
    let cancellation_token = CancellationToken::new();
    let operation_guard = session
        .begin_activity_operation(&cancellation_token)
        .await
        .expect("operation should be admitted");

    let waiter_session = Arc::clone(&session);
    let waiter_cancellation = CancellationToken::new();
    let waiter = tokio::spawn(async move {
        waiter_session
            .wait_for_activity_quiescence(&waiter_cancellation)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());

    drop(operation_guard);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("quiescence waiter should wake after guard drop")
        .expect("quiescence task should not panic")
        .expect("quiescence should succeed");
}
