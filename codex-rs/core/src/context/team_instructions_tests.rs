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
    assert!(lead_body.contains("Dynamic handoff is enabled"));
    assert!(lead_body.contains("quick preflight judgment"));
    assert!(lead_body.contains("selected code/log/web/artifact evidence"));
    assert!(lead_body.contains("browser/UI/CLI"));
    assert!(lead_body.contains("code/docs/Git/PR work"));
    assert!(lead_body.contains("MCP/Apps/connectors"));
    assert!(lead_body.contains("CI monitoring"));
    assert!(lead_body.contains("authorized release operations"));
    assert!(lead_body.contains("Higher Lead balance adds targeted review/checkpoints"));
    assert!(lead_body.contains("Workers obey existing auth"));
    assert!(lead_body.contains("Reuse supported work"));
    assert!(lead_body.contains("hard runtime or token-use guarantee"));

    let worker_body = TeamInstructions::new(TeamRole::Worker, None)
        .with_dynamic_handoff(true)
        .body();
    assert!(worker_body.contains("filter irrelevant material"));
    assert!(worker_body.contains("complete the scoped work"));
    assert!(worker_body.contains("another CLI wrapper"));
    assert!(worker_body.contains("selected code, log, web, or artifact evidence"));
    assert!(worker_body.contains("Do not dump raw logs"));
    assert!(worker_body.contains("do not bypass a boundary"));
    assert!(!worker_body.contains("Before reading a large source"));

    let default_lead_body = TeamInstructions::new(TeamRole::Lead, None).body();
    assert!(!default_lead_body.contains("Dynamic handoff"));
    assert!(!default_lead_body.contains("browser/UI/CLI"));
    let disabled_body = TeamInstructions::disabled()
        .with_dynamic_handoff(true)
        .body();
    assert!(!disabled_body.contains("Dynamic handoff"));
}

#[test]
fn lead_balance_guidance_is_lead_only_and_default_is_unchanged() {
    let default_body = TeamInstructions::new(TeamRole::Lead, None).body();
    let explicit_default_body = TeamInstructions::new(TeamRole::Lead, None)
        .with_lead_balance(3)
        .body();
    assert_eq!(explicit_default_body, default_body);

    let savings_body = TeamInstructions::new(TeamRole::Lead, None)
        .with_lead_balance(1)
        .body();
    assert!(savings_body.contains("Lead usage/confidence balance: Maximum savings"));
    assert!(savings_body.contains("discretionary Lead oversight only"));

    let confidence_body = TeamInstructions::new(TeamRole::Lead, None)
        .with_lead_balance(5)
        .body();
    assert!(confidence_body.contains("Lead usage/confidence balance: Maximum confidence"));

    let worker_body = TeamInstructions::new(TeamRole::Worker, None)
        .with_lead_balance(5)
        .body();
    assert!(!worker_body.contains("Lead usage/confidence balance"));
}

#[test]
fn manager_only_guidance_is_lead_only_and_keeps_worker_tools_independent() {
    let default_body = TeamInstructions::new(TeamRole::Lead, None).body();
    let manager_body = TeamInstructions::new(TeamRole::Lead, None)
        .with_lead_work_policy(codex_config::TeamLeadWorkPolicy::ManagerOnly)
        .body();
    let explicit_default_body = TeamInstructions::new(TeamRole::Lead, None)
        .with_lead_work_policy(codex_config::TeamLeadWorkPolicy::PromptGuided)
        .body();
    assert_eq!(explicit_default_body, default_body);
    assert!(!default_body.contains("Lead work policy: manager_only"));
    assert!(manager_body.contains("VP of Engineering"));
    assert!(manager_body.contains("changes execution ownership, not tool access"));
    assert!(
        manager_body.contains("align with the user on goals and consequential decisions")
    );
    assert!(manager_body.contains("Workers own substantive execution end to end"));
    assert!(manager_body.contains("completion criteria, and requested evidence"));
    assert!(manager_body.contains("reuse a suitable available Worker"));
    assert!(manager_body.contains("truly independent and has little overlap"));
    assert!(manager_body.contains("in parallel within the configured runtime limit"));
    assert!(manager_body.contains("do not default to one Worker"));
    assert!(manager_body.contains("A separate capacity lookup is not required"));
    assert!(
        manager_body.contains("Do not duplicate their execution or request routine progress updates")
    );
    assert!(manager_body.contains("blocker, stalled work, clearly wrong direction"));
    assert!(manager_body.contains("Workers have normal tool access"));
    assert!(manager_body.contains("without routine Lead permission or check-ins"));
    assert!(manager_body.contains("review concise returned evidence"));

    let worker_body = TeamInstructions::new(TeamRole::Worker, None)
        .with_lead_work_policy(codex_config::TeamLeadWorkPolicy::ManagerOnly)
        .body();
    assert!(!worker_body.contains("Lead work policy: manager_only"));
    assert!(!worker_body.contains("VP of Engineering"));
}
