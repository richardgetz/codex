use super::team_profiles_from_snapshot;
use super::team_profiles_from_snapshot_with_timeout;
use crate::config::Config;
use crate::config::ConfigOverrides;
use codex_config::TeamModelProfile;
use codex_config::TeamModelProfileToml;
use codex_config::TeamModelProfiles;
use codex_config::TeamToml;
use codex_config::TeamWorkerProfileToml;
use codex_config::config_toml::ConfigToml;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::TeamMode;
use codex_protocol::protocol::TeamRole;
use codex_protocol::protocol::ThreadTeamSettings;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

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
            lead_dynamic_handoff: false,
            lead_balance: codex_config::DEFAULT_TEAM_LEAD_BALANCE,
            lead_oversight_timeout_minutes:
                codex_config::DEFAULT_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES,
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

#[test]
fn team_snapshot_preserves_configured_oversight_timeout() {
    let profiles = team_profiles_from_snapshot_with_timeout(&snapshot("lead", "worker"), 90)
        .expect("valid snapshot")
        .expect("profiles");
    assert_eq!(profiles.lead_oversight_timeout_minutes, 90);
}

#[test]
fn team_snapshot_preserves_dynamic_handoff_setting() {
    let mut settings = snapshot("lead", "worker");
    settings.dynamic_handoff = Some(true);
    let profiles = team_profiles_from_snapshot_with_timeout(&settings, 90)
        .expect("valid snapshot")
        .expect("profiles");

    assert!(profiles.lead_dynamic_handoff);
}

#[test]
fn team_snapshot_defaults_and_validates_lead_balance() {
    let profiles = team_profiles_from_snapshot(&snapshot("lead", "worker"))
        .expect("valid snapshot")
        .expect("profiles");
    assert_eq!(
        profiles.lead_balance,
        codex_config::DEFAULT_TEAM_LEAD_BALANCE
    );

    let mut settings = snapshot("lead", "worker");
    settings.lead_balance = Some(5);
    let profiles = team_profiles_from_snapshot(&settings)
        .expect("valid snapshot")
        .expect("profiles");
    assert_eq!(profiles.lead_balance, 5);

    for balance in [0, 6] {
        settings.lead_balance = Some(balance);
        let error = team_profiles_from_snapshot(&settings)
            .expect_err("out-of-range Lead balance should be rejected");
        assert_eq!(
            error,
            "thread team snapshot Lead balance must be between 1 and 5"
        );
    }
}

#[tokio::test]
async fn team_settings_snapshot_carries_lead_balance() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            team: Some(TeamToml {
                enabled: Some(false),
                lead: Some(TeamModelProfileToml {
                    model: Some("lead".to_string()),
                    reasoning_effort: Some(ReasoningEffortConfig::High),
                    balance: Some(4),
                    dynamic_handoff: None,
                    oversight_timeout_minutes: None,
                }),
                worker: Some(TeamWorkerProfileToml {
                    model: Some("worker".to_string()),
                    reasoning_effort: Some(ReasoningEffortConfig::Low),
                    max_concurrent: None,
                }),
            }),
            ..Default::default()
        },
        ConfigOverrides::default(),
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await?;

    let snapshot = config
        .team_settings_snapshot(Some(TeamRole::Lead))
        .expect("configured team should have a snapshot");
    assert_eq!(snapshot.lead_balance, Some(4));
    Ok(())
}
