use super::*;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

#[test]
fn parses_team_commands_case_insensitively() {
    assert_eq!(parse_team_command(""), Ok(TeamCommand::Status));
    assert_eq!(parse_team_command(" status "), Ok(TeamCommand::Status));
    assert_eq!(parse_team_command("ON"), Ok(TeamCommand::On));
    assert_eq!(parse_team_command("off"), Ok(TeamCommand::Off));
}

#[test]
fn parses_team_profile_commands() {
    assert_eq!(
        parse_team_command("lead"),
        Ok(TeamCommand::SelectProfile {
            role: TeamRole::Lead
        })
    );
    assert_eq!(
        parse_team_command("WORKER gpt-5.6-sol high"),
        Ok(TeamCommand::ConfigureProfile {
            role: TeamRole::Worker,
            model: "gpt-5.6-sol".to_string(),
            effort: ReasoningEffort::High,
        })
    );
}

#[test]
fn parses_lead_balance_commands() {
    assert_eq!(
        parse_team_command("balance"),
        Ok(TeamCommand::SelectBalance)
    );
    assert_eq!(
        parse_team_command("BALANCE 5"),
        Ok(TeamCommand::ConfigureBalance { balance: 5 })
    );
    assert_eq!(parse_team_command("balance 0"), Err(TEAM_USAGE));
    assert_eq!(parse_team_command("balance 6"), Err(TEAM_USAGE));
    assert_eq!(parse_team_command("balance 3 now"), Err(TEAM_USAGE));
}

#[test]
fn parses_lead_work_policy_commands() {
    assert_eq!(
        parse_team_command("WORK-POLICY"),
        Ok(TeamCommand::SelectWorkPolicy)
    );
    assert_eq!(
        parse_team_command("work-policy manager_only"),
        Ok(TeamCommand::ConfigureWorkPolicy {
            policy: codex_app_server_protocol::TeamLeadWorkPolicy::ManagerOnly,
        })
    );
    assert_eq!(
        parse_team_command("work-policy PROMPT_GUIDED"),
        Ok(TeamCommand::ConfigureWorkPolicy {
            policy: codex_app_server_protocol::TeamLeadWorkPolicy::PromptGuided,
        })
    );
    assert_eq!(parse_team_command("work-policy unknown"), Err(TEAM_USAGE));
    assert_eq!(parse_team_command("work-policy manager_only now"), Err(TEAM_USAGE));
}

#[test]
fn work_policy_changes_require_an_active_lead_assignment() {
    let lead = ThreadTeamSettings {
        mode: TeamMode::LeadWorker,
        role: Some(TeamRole::Lead),
        lead_model: None,
        lead_reasoning_effort: None,
        lead_balance: None,
        lead_work_policy: None,
        worker_model: None,
        worker_reasoning_effort: None,
        previous_model: None,
        previous_reasoning_effort: None,
    };
    let worker = ThreadTeamSettings {
        role: Some(TeamRole::Worker),
        ..lead.clone()
    };
    let disabled = ThreadTeamSettings {
        mode: TeamMode::Off,
        ..lead.clone()
    };

    assert!(super::super::team_work_policy::is_active_lead_work_policy_target(Some(&lead)));
    assert!(!super::super::team_work_policy::is_active_lead_work_policy_target(Some(&worker)));
    assert!(!super::super::team_work_policy::is_active_lead_work_policy_target(Some(&disabled)));
    assert!(!super::super::team_work_policy::is_active_lead_work_policy_target(None));
}

#[test]
fn rejects_extra_team_arguments() {
    assert_eq!(parse_team_command("on now"), Err(TEAM_USAGE));
    assert_eq!(parse_team_command("maybe"), Err(TEAM_USAGE));
    assert_eq!(parse_team_command("lead gpt-5.6-sol"), Err(TEAM_USAGE));
    assert_eq!(
        parse_team_command("worker gpt-5.6-sol high now"),
        Err(TEAM_USAGE)
    );
}

#[test]
fn formats_authoritative_team_snapshot() {
    let settings = ThreadTeamSettings {
        mode: TeamMode::LeadWorker,
        role: Some(TeamRole::Lead),
        lead_model: Some("gpt-lead".to_string()),
        lead_reasoning_effort: Some(ReasoningEffort::Max),
        lead_balance: Some(3),
        lead_work_policy: Some(codex_app_server_protocol::TeamLeadWorkPolicy::ManagerOnly),
        worker_model: Some("gpt-worker".to_string()),
        worker_reasoning_effort: Some(ReasoningEffort::High),
        previous_model: Some("gpt-single".to_string()),
        previous_reasoning_effort: Some(ReasoningEffort::Medium),
    };
    insta::assert_snapshot!(format_team_status(Some(&settings)), @r"
Lead/Worker team: on
Role: Lead
Lead work policy: manager only
Lead: gpt-lead (max)
Worker: gpt-worker (high)
");
}

#[test]
fn formats_non_default_lead_balance() {
    let settings = ThreadTeamSettings {
        mode: TeamMode::LeadWorker,
        role: Some(TeamRole::Lead),
        lead_model: None,
        lead_reasoning_effort: None,
        lead_balance: Some(5),
        lead_work_policy: None,
        worker_model: None,
        worker_reasoning_effort: None,
        previous_model: None,
        previous_reasoning_effort: None,
    };
    assert!(
        format_team_status(Some(&settings))
            .contains("Lead usage/confidence balance: 5 (Maximum confidence)")
    );
    assert!(format_team_status(Some(&settings)).contains("Lead work policy: prompt guided"));
}

#[test]
fn missing_snapshot_is_reported_without_config_inference() {
    assert_eq!(
        format_team_status(None),
        "Team mode is not configured for this session."
    );
}
