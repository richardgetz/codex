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

#[test]
fn dynamic_handoff_guidance_is_role_specific_and_opt_in() {
    let lead_body = TeamInstructions::new(TeamRole::Lead, None)
        .with_dynamic_handoff(true)
        .body();
    assert!(lead_body.contains("Dynamic lookup handoff is enabled"));
    assert!(lead_body.contains("quick preflight judgment"));
    assert!(lead_body.contains("selected relevant code, log, or web excerpts"));
    assert!(lead_body.contains("do not automatically repeat the lookup"));
    assert!(lead_body.contains("less Lead input does not mean zero Worker token use"));

    let worker_body = TeamInstructions::new(TeamRole::Worker, None)
        .with_dynamic_handoff(true)
        .body();
    assert!(worker_body.contains("filter irrelevant material"));
    assert!(worker_body.contains("selected evidence excerpts"));
    assert!(!worker_body.contains("Before reading a large source"));

    let default_lead_body = TeamInstructions::new(TeamRole::Lead, None).body();
    assert!(!default_lead_body.contains("Dynamic lookup handoff"));
    let disabled_body = TeamInstructions::disabled()
        .with_dynamic_handoff(true)
        .body();
    assert!(!disabled_body.contains("Dynamic lookup handoff"));
}
