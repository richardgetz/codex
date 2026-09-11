use super::AgentControl;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn pause_serializes_activity_admission() {
    let control = AgentControl::default();
    let in_flight = AtomicU32::new(0);

    assert!(control.admit_activity_operation(&in_flight).await);
    assert_eq!(in_flight.load(Ordering::Acquire), 1);

    control.set_root_activity(true).await;
    assert!(!control.admit_activity_operation(&in_flight).await);
    assert_eq!(in_flight.load(Ordering::Acquire), 1);

    control.set_root_activity(false).await;
    assert!(control.admit_activity_operation(&in_flight).await);
    assert_eq!(in_flight.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn activity_resume_notification_unblocks_a_retained_wait() {
    let control = AgentControl::default();
    control.set_root_activity(true).await;
    let waiter_control = control.clone();
    let waiter = tokio::spawn(async move {
        loop {
            let notify = waiter_control.root_activity_resume_notify();
            let notified = notify.notified();
            if !waiter_control.root_activity_paused() {
                return;
            }
            notified.await;
        }
    });

    control.set_root_activity(false).await;
    timeout(Duration::from_secs(1), waiter)
        .await
        .expect("continue should release the retained wait")
        .expect("waiter task should finish");
}
