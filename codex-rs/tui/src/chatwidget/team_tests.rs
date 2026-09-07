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
fn rejects_extra_team_arguments() {
    assert_eq!(parse_team_command("on now"), Err(TEAM_USAGE));
    assert_eq!(parse_team_command("maybe"), Err(TEAM_USAGE));
}

#[test]
fn formats_authoritative_team_snapshot() {
    let settings = ThreadTeamSettings {
        mode: TeamMode::LeadWorker,
        role: Some(TeamRole::Lead),
        lead_model: Some("gpt-lead".to_string()),
        lead_reasoning_effort: Some(ReasoningEffort::Max),
        worker_model: Some("gpt-worker".to_string()),
        worker_reasoning_effort: Some(ReasoningEffort::High),
        previous_model: Some("gpt-single".to_string()),
        previous_reasoning_effort: Some(ReasoningEffort::Medium),
    };
    insta::assert_snapshot!(format_team_status(Some(&settings)), @r"
Lead/Worker team: on
Role: Lead
Lead: gpt-lead (max)
Worker: gpt-worker (high)
");
}

#[test]
fn missing_snapshot_is_reported_without_config_inference() {
    assert_eq!(
        format_team_status(None),
        "Team mode is not configured for this session."
    );
}
