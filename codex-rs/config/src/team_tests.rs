use super::*;
use codex_protocol::openai_models::ReasoningEffort;

fn profile(model: &str, reasoning_effort: ReasoningEffort) -> TeamModelProfileToml {
    TeamModelProfileToml {
        model: Some(model.to_string()),
        reasoning_effort: Some(reasoning_effort),
    }
}

fn worker_profile(
    model: &str,
    reasoning_effort: ReasoningEffort,
    max_concurrent: Option<usize>,
) -> TeamWorkerProfileToml {
    TeamWorkerProfileToml {
        model: Some(model.to_string()),
        reasoning_effort: Some(reasoning_effort),
        max_concurrent,
    }
}

#[test]
fn team_config_requires_exactly_two_complete_profiles() {
    let config = TeamConfig::try_from(TeamToml {
        enabled: Some(true),
        lead: Some(profile(" gpt-lead ", ReasoningEffort::High)),
        worker: Some(worker_profile("gpt-worker", ReasoningEffort::Max, Some(3))),
    })
    .expect("valid team config");

    assert!(config.enabled);
    assert_eq!(
        config
            .profiles
            .as_ref()
            .map(|profiles| &profiles.lead.model),
        Some(&"gpt-lead".to_string())
    );
    assert_eq!(
        config
            .profiles
            .as_ref()
            .map(|profiles| &profiles.worker.reasoning_effort),
        Some(&ReasoningEffort::Max)
    );
}

#[test]
fn team_config_is_available_but_disabled_by_default() {
    let config = TeamConfig::try_from(TeamToml {
        lead: Some(profile("gpt-lead", ReasoningEffort::High)),
        worker: Some(worker_profile("gpt-worker", ReasoningEffort::Max, None)),
        ..Default::default()
    })
    .expect("valid team config");

    assert!(!config.enabled);
    assert!(config.profiles.is_some());
}

#[test]
fn team_config_rejects_partial_profiles() {
    let error = TeamConfig::try_from(TeamToml {
        lead: Some(profile("gpt-lead", ReasoningEffort::High)),
        ..Default::default()
    })
    .expect_err("partial team config should be rejected");

    assert_eq!(
        error,
        "team.worker must be configured when team.lead is configured"
    );
}

#[test]
fn enabled_team_config_without_profiles_is_rejected() {
    let error = TeamConfig::try_from(TeamToml {
        enabled: Some(true),
        ..Default::default()
    })
    .expect_err("enabled team config should require profiles");

    assert_eq!(
        error,
        "team.enabled requires both team.lead and team.worker profiles"
    );
}

#[test]
fn team_config_rejects_zero_worker_concurrency() {
    let error = TeamConfig::try_from(TeamToml {
        lead: Some(profile("gpt-lead", ReasoningEffort::High)),
        worker: Some(worker_profile("gpt-worker", ReasoningEffort::Max, Some(0))),
        ..Default::default()
    })
    .expect_err("zero Worker concurrency should be rejected");

    assert_eq!(error, "team.worker.max_concurrent must be at least 1");
}

#[test]
fn team_worker_concurrency_is_worker_only() {
    let error = toml::from_str::<TeamToml>(
        "[lead]\nmodel = \"gpt-lead\"\nreasoning_effort = \"high\"\nmax_concurrent = 3\n",
    )
    .expect_err("Lead must not accept the Worker-only concurrency setting");

    assert!(error.to_string().contains("unknown field"));
}
