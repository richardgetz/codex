use super::*;
use crate::context::ContextualUserFragment;
use codex_protocol::protocol::TeamRole;

#[test]
fn lead_receives_worker_ceiling_guidance() {
    let body = TeamInstructions::new(TeamRole::Lead, Some(3)).body();

    assert!(body.contains("Direct Worker concurrency ceiling: 3 concurrently active Workers"));
    assert!(body.contains("ceiling, not a target"));
    assert!(body.contains("Grandchildren are excluded"));
    assert!(body.contains("send_message_action"));
}

#[test]
fn worker_and_unbounded_lead_do_not_receive_ceiling_guidance() {
    let worker_body = TeamInstructions::new(TeamRole::Worker, Some(3)).body();
    let unbounded_lead_body = TeamInstructions::new(TeamRole::Lead, None).body();

    assert!(!worker_body.contains("Direct Worker concurrency ceiling"));
    assert!(!unbounded_lead_body.contains("Direct Worker concurrency ceiling"));
}
