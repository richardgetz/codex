use super::team_profiles_from_snapshot;
use codex_config::TeamModelProfile;
use codex_config::TeamModelProfiles;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::TeamMode;
use codex_protocol::protocol::ThreadTeamSettings;
use pretty_assertions::assert_eq;

fn snapshot(lead_model: &str, worker_model: &str) -> ThreadTeamSettings {
    ThreadTeamSettings {
        mode: TeamMode::Off,
        lead_model: Some(lead_model.to_string()),
        lead_reasoning_effort: Some(ReasoningEffortConfig::High),
        worker_model: Some(worker_model.to_string()),
        worker_reasoning_effort: Some(ReasoningEffortConfig::High),
        ..Default::default()
    }
}

#[test]
fn team_snapshot_models_are_trimmed() {
    let profiles = team_profiles_from_snapshot(&snapshot(" lead ", " worker "))
        .expect("valid snapshot")
        .expect("profiles");

    assert_eq!(
        profiles,
        TeamModelProfiles {
            lead: TeamModelProfile {
                model: "lead".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
            },
            worker: TeamModelProfile {
                model: "worker".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
            },
        }
    );
}

#[test]
fn team_snapshot_rejects_blank_models() {
    for (lead_model, worker_model, expected) in [
        ("", "worker", "Lead"),
        ("   ", "worker", "Lead"),
        ("lead", "", "Worker"),
        ("lead", "\t", "Worker"),
    ] {
        let error = team_profiles_from_snapshot(&snapshot(lead_model, worker_model))
            .expect_err("blank team snapshot model should be rejected");
        assert_eq!(
            error,
            format!("thread team snapshot {expected} model must be a non-empty string")
        );
    }
}
