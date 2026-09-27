use super::InputQueueActivity;
use super::WaitOutcome;
use super::wait_for_activity;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

#[tokio::test]
async fn a_fresh_completion_from_a_reused_worker_wakes_the_next_wait() {
    let (activity_tx, mut activity_rx) = watch::channel(InputQueueActivity::Mailbox);
    let deadline = || Instant::now() + Duration::from_secs(/*secs*/ 1);

    let first_wait = wait_for_activity(&mut activity_rx, None, deadline());
    activity_tx.send_replace(InputQueueActivity::Mailbox);
    assert_eq!(first_wait.await, WaitOutcome::MailboxActivity);

    let second_wait = wait_for_activity(&mut activity_rx, None, deadline());
    activity_tx.send_replace(InputQueueActivity::Mailbox);
    assert_eq!(second_wait.await, WaitOutcome::MailboxActivity);
}

#[tokio::test]
async fn steer_and_policy_change_are_actionable_wait_events() {
    let (activity_tx, mut activity_rx) = watch::channel(InputQueueActivity::Mailbox);
    let deadline = || Instant::now() + Duration::from_secs(/*secs*/ 1);

    let steer_wait = wait_for_activity(&mut activity_rx, None, deadline());
    activity_tx.send_replace(InputQueueActivity::Steer);
    assert_eq!(steer_wait.await, WaitOutcome::Steered);

    let policy_wait = wait_for_activity(&mut activity_rx, None, deadline());
    activity_tx.send_replace(InputQueueActivity::TeamPolicyChanged);
    assert_eq!(policy_wait.await, WaitOutcome::TeamPolicyChanged);
}
