use super::*;
use std::sync::Weak;

#[test]
fn deadline_message_is_bounded_and_explicit() {
    let message = format_lead_idle_message(2, 1_700_000_000);
    assert!(message.contains("routine progress will not trigger inference"));
    assert!(message.contains("1700000000"));
    assert!(message.contains("2023-11-14T22:13:20+00:00"));
}

#[test]
fn truncate_message_keeps_utf8_boundaries() {
    let message = truncate_message(&"é".repeat(MAX_OVERSIGHT_MESSAGE_BYTES));
    assert!(message.len() <= MAX_OVERSIGHT_MESSAGE_BYTES);
    assert!(message.ends_with('…'));
}

#[tokio::test]
async fn cancelling_an_armed_deadline_invalidates_its_generation() {
    let controller = LeadIdleController::default();
    controller
        .arm(
            Duration::from_secs(60),
            1_700_000_000,
            Weak::new(),
            LeadIdleArmMode::ExplicitWait,
        )
        .await
        .expect("deadline should arm");
    let generation = controller.state.lock().await.generation;
    controller.cancel().await;
    assert!(!controller.claim_deadline(generation).await);
}

#[tokio::test]
async fn a_deadline_generation_can_be_claimed_only_once_per_parking_interval() {
    let controller = LeadIdleController::default();
    controller
        .arm(
            Duration::from_secs(60),
            1_700_000_000,
            Weak::new(),
            LeadIdleArmMode::ExplicitWait,
        )
        .await
        .expect("deadline should arm");
    let generation = controller.state.lock().await.generation;
    assert!(controller.claim_deadline(generation).await);
    assert!(!controller.claim_deadline(generation).await);

    controller
        .arm(
            Duration::from_secs(60),
            1_700_000_060,
            Weak::new(),
            LeadIdleArmMode::CompletedLeadTurn,
        )
        .await
        .expect("a completed Lead assessment may arm another interval");
    let next_generation = controller.state.lock().await.generation;
    assert_ne!(generation, next_generation);
    assert!(controller.claim_deadline(next_generation).await);
}

#[tokio::test]
async fn explicit_wait_cannot_rearm_after_deadline_until_turn_assessment() {
    let controller = LeadIdleController::default();
    controller
        .arm(
            Duration::from_secs(60),
            1_700_000_000,
            Weak::new(),
            LeadIdleArmMode::ExplicitWait,
        )
        .await
        .expect("deadline should arm");
    let generation = controller.state.lock().await.generation;
    assert!(controller.claim_deadline(generation).await);
    assert!(
        controller
            .arm(
                Duration::from_secs(60),
                1_700_000_060,
                Weak::new(),
                LeadIdleArmMode::ExplicitWait,
            )
            .await
            .is_none(),
        "the same Lead turn must assess the deadline before another wait interval"
    );
    assert!(
        controller
            .arm(
                Duration::from_secs(60),
                1_700_000_060,
                Weak::new(),
                LeadIdleArmMode::CompletedLeadTurn,
            )
            .await
            .is_some(),
        "a completed assessment may start the next parking interval"
    );
}
