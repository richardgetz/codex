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
    assert!(manager_body.contains("changes execution ownership, not tool access"));
    assert!(manager_body.contains("Keep the user conversation, align goals"));
    assert!(manager_body.contains("one coherent outcome"));
    assert!(
        manager_body.contains("Workers own substantive research, logs, code and documentation")
    );
    assert!(manager_body.contains("through completion"));
    assert!(manager_body.contains("Do not duplicate that execution"));
    assert!(manager_body.contains("edit-return/test-return/fix-return handoffs"));
    assert!(manager_body.contains(
        "Include applicable repository and skill instructions with each Worker assignment"
    ));
    assert!(manager_body.contains("do not move execution back to the Lead"));
    assert!(manager_body.contains("reuse the suitable Worker already familiar with the work"));
    assert!(manager_body.contains("substantial independent work with little overlap"));
    assert!(manager_body.contains("use parallel Workers within runtime limits"));
    assert!(manager_body.contains("do not default to one Worker"));
    assert!(manager_body.contains("a capacity lookup is not required"));
    assert!(manager_body.contains("ask the responsible Worker one scoped fact/status question"));
    assert!(manager_body.contains("Do not check routine progress"));
    assert!(manager_body.contains(
        "After delegation, finish the assessment and park under existing wake/deadline behavior"
    ));
    assert!(manager_body.contains("on human guidance or actionable reports, unblock, redirect, or reassign the responsible Worker as needed"));
    assert!(manager_body.contains(
        "Intervene for blockers, stalled work, clearly wrong direction, or consequential decisions"
    ));
    assert!(manager_body.contains("otherwise leave execution with the Worker"));
    assert!(manager_body.contains("configured oversight deadline"));
    assert!(manager_body.contains("Lead balance changes final-review depth"));
    assert!(!manager_body.contains("Higher Lead balance adds targeted review/checkpoints"));
    assert!(!manager_body.contains("quick preflight judgment"));

    let worker_body = TeamInstructions::new(TeamRole::Worker, None)
        .with_lead_work_policy(codex_config::TeamLeadWorkPolicy::ManagerOnly)
        .body();
    assert!(!worker_body.contains("Lead work policy: manager_only"));
    assert!(worker_body.contains("Own your assigned outcome through completion"));
    assert!(worker_body.contains("applicable validation, debugging, and in-scope corrections"));
    assert!(worker_body.contains("Do not hand back separate edit, test, or fix phases"));
    assert!(worker_body.contains("without waiting for routine Lead permission or check-ins"));
    assert!(worker_body.contains("one concise completion report"));
    assert!(worker_body.contains("requested scope and cadence"));
    assert!(worker_body.contains("Avoid command-by-command or build narration and raw dumps"));
}

#[test]
fn manager_only_dynamic_handoff_and_balance_do_not_add_progress_checkpoints() {
    for dynamic_handoff in [false, true] {
        for balance in [1, 2, 3, 4, 5] {
            let body = TeamInstructions::new(TeamRole::Lead, None)
                .with_dynamic_handoff(dynamic_handoff)
                .with_lead_balance(balance)
                .with_lead_work_policy(codex_config::TeamLeadWorkPolicy::ManagerOnly)
                .body();

            assert!(body.contains("Lead work policy: manager_only"));
            assert!(body.contains("Lead balance changes final-review depth"));
            assert!(body.contains("configured oversight deadline"));
            assert!(!body.contains("Higher Lead balance adds targeted review/checkpoints"));
            assert!(!body.contains("optional Lead oversight checkpoints"));
            assert!(!body.contains("Dynamic handoff is enabled"));
            assert!(!body.contains("quick preflight judgment"));
            assert!(!body.contains("progress checkpoint"));
        }
    }

    let prompt_guided = TeamInstructions::new(TeamRole::Lead, None)
        .with_dynamic_handoff(true)
        .with_lead_balance(1)
        .with_lead_work_policy(codex_config::TeamLeadWorkPolicy::PromptGuided)
        .body();
    assert!(prompt_guided.contains("Higher Lead balance adds targeted review/checkpoints"));
    assert!(prompt_guided.contains("quick preflight judgment"));

    let worker_dynamic = TeamInstructions::new(TeamRole::Worker, None)
        .with_dynamic_handoff(true)
        .with_lead_work_policy(codex_config::TeamLeadWorkPolicy::ManagerOnly)
        .body();
    assert!(worker_dynamic.contains("Dynamic handoff is enabled for this team"));
    assert!(worker_dynamic.contains("complete the scoped work"));
}
