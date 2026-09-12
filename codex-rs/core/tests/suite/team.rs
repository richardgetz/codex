use anyhow::Result;
use codex_config::TeamConfig;
use codex_config::TeamModelProfile;
use codex_config::TeamModelProfiles;
use codex_core::CodexThread;
use codex_core::ThreadConfigSnapshot;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ReviewTarget;
use codex_protocol::protocol::TeamMode;
use codex_protocol::protocol::TeamRole;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::ThreadTeamSettingsUpdate;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::submit_thread_settings;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;

const INITIAL_MODEL: &str = "gpt-5.4";
const DEFAULT_MODEL: &str = "gpt-6-astra";
const LEAD_MODEL: &str = "gpt-6-astra";
const WORKER_MODEL: &str = "gpt-5.6-luna";
const REQUESTED_MODEL: &str = "gpt-5.5";
const TEAM_TOGGLE_COMP_HASH: &str = "team-toggle-compatible";
const MULTI_AGENT_V1_NAMESPACE: &str = "multi_agent_v1";
const MULTI_AGENT_V2_NAMESPACE: &str = "collaboration";
const ROOT_PROMPT: &str = "delegate a team worker";
const CHILD_TASK: &str = "inspect the team implementation";
const GRANDCHILD_TASK: &str = "inspect the team test";
const SPAWN_CALL_ID: &str = "team-spawn";
const CHILD_SPAWN_CALL_ID: &str = "team-child-spawn";
const LIMIT_PROMPT: &str = "spawn two direct team workers";
const FIRST_DIRECT_TASK: &str = "inspect the first direct task";
const SECOND_DIRECT_TASK: &str = "inspect the second direct task";
const THIRD_DIRECT_TASK: &str = "inspect the replacement direct task";
const FOLLOWUP_TASK: &str = "continue after the worker slot is released";
const REPLACEMENT_PROMPT: &str = "spawn a replacement direct team worker";
const FIRST_DIRECT_CALL_ID: &str = "team-first-direct";
const SECOND_DIRECT_CALL_ID: &str = "team-second-direct";
const THIRD_DIRECT_CALL_ID: &str = "team-third-direct";
const FIRST_DIRECT_GATE_CALL_ID: &str = "team-first-direct-gate";
const ROOT_DIRECT_GATE_CALL_ID: &str = "team-root-direct-gate";

#[path = "team_activity.rs"]
mod team_activity;
#[path = "team_idle.rs"]
mod team_idle;
#[path = "team_usage.rs"]
mod team_usage;
#[path = "team_worker_limits.rs"]
mod worker_limits;

fn team_config(mode: TeamMode, lead_model: &str, worker_model: &str) -> TeamConfig {
    TeamConfig {
        enabled: mode == TeamMode::LeadWorker,
        profiles: Some(TeamModelProfiles {
            lead: TeamModelProfile {
                model: lead_model.to_string(),
                reasoning_effort: ReasoningEffort::Max,
            },
            worker: TeamModelProfile {
                model: worker_model.to_string(),
                reasoning_effort: ReasoningEffort::Low,
            },
            lead_dynamic_handoff: false,
            lead_balance: codex_config::DEFAULT_TEAM_LEAD_BALANCE,
            lead_oversight_timeout_minutes:
                codex_config::DEFAULT_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES,
        }),
        worker_max_concurrent: None,
        lead_show_idle_notifications: false,
    }
}

fn configure_team(config: &mut Config, mode: TeamMode) {
    config.team = team_config(mode, LEAD_MODEL, WORKER_MODEL);
    config.team_mode = mode;
}

fn team_mode_update(mode: TeamMode) -> ThreadSettingsOverrides {
    ThreadSettingsOverrides {
        team: Some(ThreadTeamSettingsUpdate {
            mode,
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn submit_turn(
    thread: &CodexThread,
    prompt: &str,
    thread_settings: ThreadSettingsOverrides,
) -> Result<()> {
    thread
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(thread_settings),
        )
        .await?;
    wait_for_event(thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(())
}

async fn submit_team_update_expect_error(thread: &CodexThread, mode: TeamMode) -> Result<String> {
    let submission_id = thread
        .submit(Op::ThreadSettings {
            thread_settings: team_mode_update(mode),
            usage_policy_update: None,
        })
        .await?;
    timeout(Duration::from_secs(10), async {
        loop {
            let event = thread.next_event().await?;
            if event.id != submission_id {
                continue;
            }
            match event.msg {
                EventMsg::Error(error) => return Ok::<_, anyhow::Error>(error.message),
                EventMsg::ThreadSettingsApplied(_) => {
                    anyhow::bail!("team settings update unexpectedly succeeded")
                }
                _ => {}
            }
        }
    })
    .await?
}

fn assert_incompatible_worker_error(error: &str) {
    let lowercase = error.to_ascii_lowercase();
    assert!(
        error.contains(WORKER_MODEL)
            && lowercase.contains("worker")
            && (lowercase.contains("multi-agent") || lowercase.contains("multi_agent")),
        "incompatible Worker backend error should identify the assignment: {error}"
    );
}

fn assert_assignment(snapshot: &ThreadConfigSnapshot, mode: TeamMode, role: TeamRole) {
    let team = snapshot.team.as_ref().expect("team snapshot");
    assert_eq!(
        (
            team.mode,
            team.role,
            team.lead_model.as_deref(),
            team.worker_model.as_deref()
        ),
        (mode, Some(role), Some(LEAD_MODEL), Some(WORKER_MODEL),)
    );
    assert_eq!(snapshot.model, LEAD_MODEL);
    assert_eq!(snapshot.reasoning_effort, Some(ReasoningEffort::Max));
}

fn assert_request_assignment(request: &ResponsesRequest, model: &str, effort: &str) {
    let body = request.body_json();
    assert_eq!(body["model"], json!(model));
    assert_eq!(body["reasoning"]["effort"], json!(effort));
}

async fn wait_for_captured_request(
    response: &ResponseMock,
    predicate: impl Fn(&ResponsesRequest) -> bool,
    label: &str,
) -> ResponsesRequest {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(request) = response
            .requests()
            .into_iter()
            .find(|request| predicate(request))
        {
            return request;
        }
        if Instant::now() >= deadline {
            panic!("{label} request was not captured");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn response_request_has_model(request: &ResponsesRequest, model: &str) -> bool {
    request.body_json()["model"].as_str() == Some(model)
}

fn response_request_has_function_call_output(request: &ResponsesRequest, call_id: &str) -> bool {
    request.input().iter().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("function_call_output")
            && item.get("call_id").and_then(Value::as_str) == Some(call_id)
    })
}

fn request_body(request: &wiremock::Request) -> Option<Value> {
    let body = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        })
        .then(|| zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok())
        .flatten()
        .unwrap_or_else(|| request.body.clone());
    serde_json::from_slice(&body).ok()
}

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    request_body(request).is_some_and(|body| body.to_string().contains(text))
}

fn request_has_model(request: &wiremock::Request, model: &str) -> bool {
    request_body(request)
        .and_then(|body| body.get("model").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|actual| actual == model)
}

fn request_has_function_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    request_body(request)
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some(call_id)
            })
        })
}

fn team_instruction_fragments(request: &ResponsesRequest) -> Vec<String> {
    request
        .message_input_texts("developer")
        .into_iter()
        .filter(|text| text.contains("<team_role_instructions>"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_toggle_pins_lead_and_restores_original_model() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        (1..=4)
            .map(|index| {
                sse(vec![
                    ev_response_created(&format!("team-resp-{index}")),
                    ev_completed(&format!("team-resp-{index}")),
                ])
            })
            .collect(),
    )
    .await;
    let mut expected_config = team_config(TeamMode::Off, LEAD_MODEL, WORKER_MODEL);
    expected_config
        .profiles
        .as_mut()
        .expect("team profiles")
        .lead_dynamic_handoff = true;
    let mut builder = test_codex()
        .with_model_info_override(INITIAL_MODEL, |model_info| {
            model_info.comp_hash = Some(TEAM_TOGGLE_COMP_HASH.to_string());
        })
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.comp_hash = Some(TEAM_TOGGLE_COMP_HASH.to_string());
        })
        .with_model(INITIAL_MODEL)
        .with_config({
            let expected_config = expected_config.clone();
            move |config| {
                config.model_reasoning_effort = Some(ReasoningEffort::Medium);
                config.team = expected_config;
                config.team_mode = TeamMode::Off;
            }
        });
    let test = builder.build_with_auto_env(&server).await?;

    submit_thread_settings(&test.codex, team_mode_update(TeamMode::LeadWorker)).await?;
    let enabled = test.codex.config_snapshot().await;
    assert_assignment(&enabled, TeamMode::LeadWorker, TeamRole::Lead);
    submit_turn(&test.codex, "lead turn", ThreadSettingsOverrides::default()).await?;

    submit_turn(
        &test.codex,
        "attempt to override lead",
        ThreadSettingsOverrides {
            model: Some(REQUESTED_MODEL.to_string()),
            effort: Some(Some(ReasoningEffort::Low)),
            ..Default::default()
        },
    )
    .await?;
    let still_enabled = test.codex.config_snapshot().await;
    assert_assignment(&still_enabled, TeamMode::LeadWorker, TeamRole::Lead);

    submit_thread_settings(&test.codex, team_mode_update(TeamMode::Off)).await?;
    let disabled = test.codex.config_snapshot().await;
    assert_eq!(disabled.model, INITIAL_MODEL);
    assert_eq!(disabled.reasoning_effort, Some(ReasoningEffort::Medium));
    assert_eq!(
        disabled.team.as_ref().map(|team| team.mode),
        Some(TeamMode::Off)
    );
    submit_thread_settings(&test.codex, team_mode_update(TeamMode::Off)).await?;
    let disabled_again = test.codex.config_snapshot().await;
    assert_eq!(
        (
            &disabled_again.model,
            &disabled_again.reasoning_effort,
            disabled_again.team.as_ref().map(|team| team.mode),
        ),
        (
            &disabled.model,
            &disabled.reasoning_effort,
            disabled.team.as_ref().map(|team| team.mode),
        )
    );
    submit_turn(
        &test.codex,
        "ordinary turn",
        ThreadSettingsOverrides::default(),
    )
    .await?;

    submit_thread_settings(&test.codex, team_mode_update(TeamMode::LeadWorker)).await?;
    let enabled_again = test.codex.config_snapshot().await;
    assert_assignment(&enabled_again, TeamMode::LeadWorker, TeamRole::Lead);
    submit_thread_settings(&test.codex, team_mode_update(TeamMode::LeadWorker)).await?;
    let enabled_idempotently = test.codex.config_snapshot().await;
    assert_eq!(
        (
            &enabled_idempotently.model,
            &enabled_idempotently.reasoning_effort,
            enabled_idempotently.team.as_ref().map(|team| team.mode),
        ),
        (
            &enabled_again.model,
            &enabled_again.reasoning_effort,
            enabled_again.team.as_ref().map(|team| team.mode),
        )
    );
    submit_turn(
        &test.codex,
        "lead again",
        ThreadSettingsOverrides::default(),
    )
    .await?;

    assert_eq!(test.config.team, expected_config);
    let requests = responses.requests();
    assert_request_assignment(&requests[0], LEAD_MODEL, "max");
    assert_request_assignment(&requests[1], LEAD_MODEL, "max");
    assert_request_assignment(&requests[2], INITIAL_MODEL, "medium");
    assert_request_assignment(&requests[3], LEAD_MODEL, "max");

    let active_fragments = team_instruction_fragments(&requests[0]);
    assert_eq!(active_fragments.len(), 1);
    assert!(active_fragments[0].contains("You are the Lead"));
    assert!(active_fragments[0].contains("Team On is the user's opt-in authorization"));
    assert!(active_fragments[0].contains("delegate substantive in-scope work"));
    assert!(active_fragments[0].contains("Dynamic lookup handoff is enabled"));
    assert_eq!(
        team_instruction_fragments(&requests[1]),
        active_fragments,
        "an unchanged active team state should not append another team fragment"
    );
    let disabled_fragments = team_instruction_fragments(&requests[2]);
    assert_eq!(disabled_fragments.len(), active_fragments.len() + 1);
    let disabled_fragment = disabled_fragments.last().expect("disabled team fragment");
    assert!(disabled_fragment.to_ascii_lowercase().contains("disabled"));
    assert!(!disabled_fragment.contains("Dynamic lookup handoff"));
    assert_ne!(
        disabled_fragment,
        active_fragments.last().expect("active team fragment")
    );
    let reenabled_fragments = team_instruction_fragments(&requests[3]);
    assert_eq!(reenabled_fragments.len(), disabled_fragments.len() + 1);
    assert!(
        reenabled_fragments
            .last()
            .is_some_and(|fragment| fragment.contains("You are the Lead"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_off_restores_provider_default_after_startup_without_model() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("team-default"),
            ev_completed("team-default"),
        ]),
    )
    .await;
    let mut builder = test_codex().with_config(|config| {
        config.model = None;
        config.model_reasoning_effort = Some(ReasoningEffort::High);
        configure_team(config, TeamMode::LeadWorker);
    });
    let test = builder.build_with_auto_env(&server).await?;
    let startup = test.codex.config_snapshot().await;
    assert_assignment(&startup, TeamMode::LeadWorker, TeamRole::Lead);

    submit_thread_settings(&test.codex, team_mode_update(TeamMode::Off)).await?;
    let disabled = test.codex.config_snapshot().await;
    assert_eq!(disabled.model, DEFAULT_MODEL);
    assert_eq!(disabled.reasoning_effort, Some(ReasoningEffort::High));
    submit_turn(
        &test.codex,
        "provider default",
        ThreadSettingsOverrides::default(),
    )
    .await?;

    assert_eq!(
        test.config.team,
        team_config(TeamMode::LeadWorker, LEAD_MODEL, WORKER_MODEL)
    );
    assert_request_assignment(&response.single_request(), DEFAULT_MODEL, "high");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_explicit_legacy_depth_fails_open_but_catalog_v2_allows_it() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut legacy_builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V1);
        })
        .with_config(|config| {
            config.agent_max_depth = 1;
            config.agent_max_depth_explicit = true;
            config
                .features
                .enable(Feature::Collab)
                .expect("Collab feature");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("MultiAgentV2 feature");
            configure_team(config, TeamMode::LeadWorker);
        });
    let legacy = legacy_builder.build_with_auto_env(&server).await?;
    let legacy_snapshot = legacy.codex.config_snapshot().await;
    assert_eq!(
        legacy_snapshot.team.as_ref().map(|team| team.mode),
        Some(TeamMode::Off)
    );
    assert_eq!(legacy_snapshot.model, LEAD_MODEL);
    let warning =
        wait_for_event(&legacy.codex, |event| matches!(event, EventMsg::Warning(_))).await;
    let EventMsg::Warning(warning) = warning else {
        unreachable!("warning predicate should only return warning events")
    };
    assert!(
        warning
            .message
            .contains("Team mode was disabled for this thread")
    );
    assert!(
        warning
            .message
            .contains("agents.max_depth must be at least 2")
    );

    let mut v2_builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_config(|config| {
            config.agent_max_depth = 1;
            config.agent_max_depth_explicit = true;
            config
                .features
                .enable(Feature::Collab)
                .expect("Collab feature");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("MultiAgentV2 feature");
            configure_team(config, TeamMode::LeadWorker);
        });
    let v2 = v2_builder.build_with_auto_env(&server).await?;
    let _v2_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("team-v2-resolution"),
            ev_completed("team-v2-resolution"),
        ]),
    )
    .await;
    submit_turn(
        &v2.codex,
        "resolve the catalog-selected team backend",
        ThreadSettingsOverrides::default(),
    )
    .await?;
    assert_eq!(v2.codex.multi_agent_version(), Some(MultiAgentVersion::V2));
    assert_assignment(
        &v2.codex.config_snapshot().await,
        TeamMode::LeadWorker,
        TeamRole::Lead,
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_fails_open_incompatible_worker_on_startup_but_rejects_toggle() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut startup_builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::Disabled);
        })
        .with_model(INITIAL_MODEL)
        .with_config(|config| configure_team(config, TeamMode::LeadWorker));
    let startup = startup_builder.build_with_auto_env(&server).await?;
    let startup_snapshot = startup.codex.config_snapshot().await;
    assert_eq!(startup_snapshot.model, INITIAL_MODEL);
    assert_eq!(
        startup_snapshot.team.as_ref().map(|team| team.mode),
        Some(TeamMode::Off)
    );
    let warning = wait_for_event(&startup.codex, |event| {
        matches!(event, EventMsg::Warning(_))
    })
    .await;
    let EventMsg::Warning(warning) = warning else {
        unreachable!("warning predicate should only return warning events")
    };
    assert!(
        warning
            .message
            .contains("Team mode was disabled for this thread")
    );
    assert_incompatible_worker_error(&warning.message);

    let mut toggle_builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::Disabled);
        })
        .with_config(|config| configure_team(config, TeamMode::Off));
    let test = toggle_builder.build_with_auto_env(&server).await?;
    assert_eq!(
        test.codex
            .config_snapshot()
            .await
            .team
            .as_ref()
            .map(|team| team.mode),
        Some(TeamMode::Off)
    );

    let error = submit_team_update_expect_error(&test.codex, TeamMode::LeadWorker).await?;
    assert_incompatible_worker_error(&error);
    assert_eq!(
        test.codex
            .config_snapshot()
            .await
            .team
            .as_ref()
            .map(|team| team.mode),
        Some(TeamMode::Off)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_fails_open_incompatible_worker_on_cold_resume() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("team-resume-before-invalid"),
            ev_completed("team-resume-before-invalid"),
        ]),
    )
    .await;
    let mut initial_builder = test_codex()
        .with_model(INITIAL_MODEL)
        .with_config(|config| {
            config.model_reasoning_effort = Some(ReasoningEffort::Medium);
            configure_team(config, TeamMode::LeadWorker);
        });
    let initial = initial_builder.build_with_auto_env(&server).await?;
    submit_turn(
        &initial.codex,
        "persist an active team before resume",
        ThreadSettingsOverrides::default(),
    )
    .await?;

    let mut resume_builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::Disabled);
        })
        .with_model(REQUESTED_MODEL)
        .with_config(|config| {
            config.model_reasoning_effort = Some(ReasoningEffort::Low);
            configure_team(config, TeamMode::LeadWorker);
        });
    let resumed = resume_builder.restart(&server, &initial).await?;
    let snapshot = resumed.codex.config_snapshot().await;
    assert_eq!(snapshot.model, INITIAL_MODEL);
    assert_eq!(snapshot.reasoning_effort, Some(ReasoningEffort::Medium));
    assert_eq!(
        snapshot.team.as_ref().map(|team| team.mode),
        Some(TeamMode::Off)
    );
    assert_eq!(
        resumed.config.team,
        team_config(TeamMode::LeadWorker, LEAD_MODEL, WORKER_MODEL)
    );
    let warning = wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::Warning(_))
    })
    .await;
    let EventMsg::Warning(warning) = warning else {
        unreachable!("warning predicate should only return warning events")
    };
    assert!(
        warning
            .message
            .contains("Team mode was disabled for this thread")
    );
    assert_incompatible_worker_error(&warning.message);
    insta::assert_snapshot!(warning.message, @r#"Team mode was disabled for this thread because its assignment is unavailable: invalid value for `team`: `Worker model `gpt-5.6-luna` is disabled for MultiAgentV2; choose a model that supports multi-agent delegation` is not in the allowed set configured Lead and Worker profiles (set by <unspecified>)"#);
    assert_request_assignment(&response.single_request(), LEAD_MODEL, "max");

    let mut compatible_resume_builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_model(REQUESTED_MODEL)
        .with_config(|config| {
            config.model_reasoning_effort = Some(ReasoningEffort::Low);
            configure_team(config, TeamMode::LeadWorker);
        });
    let resumed_again = compatible_resume_builder.restart(&server, &resumed).await?;
    let resumed_again_snapshot = resumed_again.codex.config_snapshot().await;
    assert_eq!(resumed_again_snapshot.model, INITIAL_MODEL);
    assert_eq!(
        resumed_again_snapshot.team.as_ref().map(|team| team.mode),
        Some(TeamMode::Off)
    );
    Ok(())
}

#[test_case(
    MultiAgentVersion::V1,
    MULTI_AGENT_V1_NAMESPACE;
    "legacy backend with nested worker"
)]
#[test_case(
    MultiAgentVersion::V2,
    MULTI_AGENT_V2_NAMESPACE;
    "v2 backend with nested v1 worker"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_spawn_uses_worker_despite_role_and_model_overrides(
    lead_multi_agent_version: MultiAgentVersion,
    tool_namespace: &str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_TASK,
        "task_name": "team_worker",
        "agent_type": "team-reviewer",
        "model": REQUESTED_MODEL,
        "reasoning_effort": "high",
        "fork_turns": "none",
    }))?;
    let grandchild_args = serde_json::to_string(&json!({
        "message": GRANDCHILD_TASK,
        "task_name": "team_review_worker",
        "agent_type": "team-reviewer",
        "model": REQUESTED_MODEL,
        "reasoning_effort": "high",
        "fork_turns": "none",
    }))?;
    let root_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, ROOT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-root-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                tool_namespace,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("team-root-1"),
        ]),
    )
    .await;
    let child_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, CHILD_TASK)
                && !body_contains(request, GRANDCHILD_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, CHILD_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-child-1"),
            ev_function_call_with_namespace(
                CHILD_SPAWN_CALL_ID,
                tool_namespace,
                "spawn_agent",
                &grandchild_args,
            ),
            ev_completed("team-child-1"),
        ]),
    )
    .await;
    let grandchild_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, GRANDCHILD_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, CHILD_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-grandchild"),
            ev_assistant_message("team-grandchild-message", "review complete"),
            ev_completed("team-grandchild"),
        ]),
    )
    .await;
    let child_completion_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, CHILD_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-child-2"),
            ev_assistant_message("team-child-message", "worker review complete"),
            ev_completed("team-child-2"),
        ]),
    )
    .await;
    let root_completion_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-root-2"),
            ev_assistant_message("team-root-message", "delegation complete"),
            ev_completed("team-root-2"),
        ]),
    )
    .await;

    let role_model = "gpt-5.2";
    let mut builder = test_codex()
        .with_model_info_override(LEAD_MODEL, move |model_info| {
            model_info.multi_agent_version = Some(lead_multi_agent_version);
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V1);
        })
        .with_model(INITIAL_MODEL)
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("Collab feature");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("MultiAgentV2 feature");
            configure_team(config, TeamMode::LeadWorker);
            config
                .team
                .profiles
                .as_mut()
                .expect("team profiles")
                .lead_dynamic_handoff = true;
            config.team.worker_max_concurrent = Some(1);
            let role_path = config.codex_home.join("team-reviewer.toml");
            std::fs::write(
                &role_path,
                format!("model = \"{role_model}\"\nmodel_reasoning_effort = \"high\"\n"),
            )
            .expect("write team role");
            config.agent_roles.insert(
                "team-reviewer".to_string(),
                codex_core::config::AgentRoleConfig {
                    description: Some("Team reviewer".to_string()),
                    config_file: Some(role_path.to_path_buf()),
                    nickname_candidates: None,
                },
            );
        });
    let test = builder.build_with_auto_env(&server).await?;
    submit_turn(&test.codex, ROOT_PROMPT, ThreadSettingsOverrides::default()).await?;
    assert_eq!(
        test.codex.multi_agent_version(),
        Some(lead_multi_agent_version)
    );

    let root_request = wait_for_captured_request(
        &root_response,
        |request| {
            request.body_contains_text(ROOT_PROMPT)
                && response_request_has_model(request, LEAD_MODEL)
                && !response_request_has_function_call_output(request, SPAWN_CALL_ID)
        },
        "root",
    )
    .await;
    let root_completion_request = wait_for_captured_request(
        &root_completion_response,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, SPAWN_CALL_ID)
        },
        "root completion",
    )
    .await;
    let (root_output, _) = root_completion_request
        .function_call_output_content_and_success(SPAWN_CALL_ID)
        .expect("root spawn call output");
    let root_output = root_output.expect("root spawn call output content");
    let root_result: Value = serde_json::from_str(&root_output).unwrap_or_else(|error| {
        panic!("root spawn output should be JSON ({error}); raw output: {root_output:?}");
    });
    if lead_multi_agent_version == MultiAgentVersion::V1 {
        let root_agent_id = root_result
            .get("agent_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            ThreadId::from_string(root_agent_id).is_ok(),
            "root spawn should return a valid agent_id; raw output: {root_output:?}"
        );
    } else {
        assert!(
            root_result
                .get("task_name")
                .and_then(Value::as_str)
                .is_some(),
            "v2 root spawn should return a task_name; raw output: {root_output:?}"
        );
    }
    let child_request = wait_for_captured_request(
        &child_response,
        |request| {
            request.body_contains_text(CHILD_TASK)
                && !request.body_contains_text(GRANDCHILD_TASK)
                && response_request_has_model(request, WORKER_MODEL)
                && !response_request_has_function_call_output(request, CHILD_SPAWN_CALL_ID)
        },
        "child",
    )
    .await;
    let grandchild_request = wait_for_captured_request(
        &grandchild_response,
        |request| {
            request.body_contains_text(GRANDCHILD_TASK)
                && response_request_has_model(request, WORKER_MODEL)
                && !response_request_has_function_call_output(request, CHILD_SPAWN_CALL_ID)
        },
        "grandchild",
    )
    .await;
    let child_completion_request = wait_for_captured_request(
        &child_completion_response,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(request, CHILD_SPAWN_CALL_ID)
        },
        "child completion",
    )
    .await;
    assert_request_assignment(&root_request, LEAD_MODEL, "max");
    assert_request_assignment(&child_request, WORKER_MODEL, "low");
    assert_request_assignment(&grandchild_request, WORKER_MODEL, "low");
    assert!(
        team_instruction_fragments(&root_request)
            .iter()
            .any(|fragment| fragment.contains("quick preflight judgment"))
    );
    assert!(
        team_instruction_fragments(&child_request)
            .iter()
            .any(|fragment| fragment.contains("filter irrelevant material"))
    );
    child_completion_request.function_call_output(CHILD_SPAWN_CALL_ID);

    let child_thread_id = child_request.body_json()["client_metadata"]["thread_id"]
        .as_str()
        .and_then(|thread_id| ThreadId::from_string(thread_id).ok())
        .expect("child thread ID");
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    let grandchild_thread_id = grandchild_request.body_json()["client_metadata"]["thread_id"]
        .as_str()
        .and_then(|thread_id| ThreadId::from_string(thread_id).ok())
        .expect("grandchild thread ID");
    let grandchild_thread = test.thread_manager.get_thread(grandchild_thread_id).await?;
    wait_for_event(grandchild_thread.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    wait_for_event(child_thread.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let child_snapshot = child_thread.config_snapshot().await;
    let child_team = child_snapshot.team.as_ref().expect("child team snapshot");
    assert_eq!(
        (
            child_team.mode,
            child_team.role,
            child_snapshot.model,
            child_snapshot.reasoning_effort,
        ),
        (
            TeamMode::LeadWorker,
            Some(TeamRole::Worker),
            WORKER_MODEL.to_string(),
            Some(ReasoningEffort::Low),
        )
    );
    let grandchild_snapshot = grandchild_thread.config_snapshot().await;
    let grandchild_team = grandchild_snapshot
        .team
        .as_ref()
        .expect("grandchild team snapshot");
    assert_eq!(
        (
            grandchild_team.mode,
            grandchild_team.role,
            grandchild_snapshot.model,
            grandchild_snapshot.reasoning_effort,
        ),
        (
            TeamMode::LeadWorker,
            Some(TeamRole::Worker),
            WORKER_MODEL.to_string(),
            Some(ReasoningEffort::Low),
        )
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_worker_limit_rejects_second_direct_spawn_and_reuses_completed_slot() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let first_args = serde_json::to_string(&json!({
        "message": FIRST_DIRECT_TASK,
        "task_name": "first_direct",
        "fork_turns": "none",
    }))?;
    let second_args = serde_json::to_string(&json!({
        "message": SECOND_DIRECT_TASK,
        "task_name": "second_direct",
        "fork_turns": "none",
    }))?;
    let third_args = serde_json::to_string(&json!({
        "message": THIRD_DIRECT_TASK,
        "task_name": "third_direct",
        "fork_turns": "none",
    }))?;
    let gate_args = serde_json::to_string(&json!({
        "barrier": {
            "id": "team-worker-limit-direct-completion",
            "participants": 2,
            "timeout_ms": 10_000,
        },
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, LIMIT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, FIRST_DIRECT_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-limit-root-1"),
            ev_function_call_with_namespace(
                FIRST_DIRECT_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &first_args,
            ),
            ev_completed("team-limit-root-1"),
        ]),
    )
    .await;
    let root_after_first = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, FIRST_DIRECT_CALL_ID)
                && !request_has_function_call_output(request, SECOND_DIRECT_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-limit-root-2"),
            ev_function_call_with_namespace(
                SECOND_DIRECT_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &second_args,
            ),
            ev_completed("team-limit-root-2"),
        ]),
    )
    .await;
    let root_after_second = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, SECOND_DIRECT_CALL_ID)
                && !body_contains(request, REPLACEMENT_PROMPT)
        },
        sse(vec![
            ev_response_created("team-limit-root-3"),
            ev_function_call(ROOT_DIRECT_GATE_CALL_ID, "test_sync_tool", &gate_args),
            ev_completed("team-limit-root-3"),
        ]),
    )
    .await;
    let root_after_gate = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, ROOT_DIRECT_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-limit-root-4"),
            ev_assistant_message("team-limit-root-message", "direct worker limit checked"),
            ev_completed("team-limit-root-4"),
        ]),
    )
    .await;

    let first_worker_response = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FIRST_DIRECT_TASK) && request_has_model(request, WORKER_MODEL)
        },
        sse_response(sse(vec![
            ev_response_created("team-limit-worker-1"),
            ev_function_call(FIRST_DIRECT_GATE_CALL_ID, "test_sync_tool", &gate_args),
            ev_completed("team-limit-worker-1"),
        ])),
    )
    .await;
    let first_worker_after_gate = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, FIRST_DIRECT_GATE_CALL_ID)
        },
        sse_response(sse(vec![
            ev_response_created("team-limit-worker-2"),
            ev_assistant_message("team-limit-worker-message", "first worker complete"),
            ev_completed("team-limit-worker-2"),
        ])),
    )
    .await;

    let mut builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V1);
            model_info
                .experimental_supported_tools
                .push("test_sync_tool".to_string());
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V1);
            model_info
                .experimental_supported_tools
                .push("test_sync_tool".to_string());
        })
        .with_model(INITIAL_MODEL)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("Collab feature");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("MultiAgentV2 feature");
            configure_team(config, TeamMode::LeadWorker);
            config.team.worker_max_concurrent = Some(1);
        });
    let test = builder.build_with_auto_env(&server).await?;
    submit_turn(
        &test.codex,
        LIMIT_PROMPT,
        ThreadSettingsOverrides::default(),
    )
    .await?;
    let _ = root_after_gate.single_request();

    let first_output = root_after_first
        .function_call_output_text(FIRST_DIRECT_CALL_ID)
        .expect("first direct spawn output");
    let first_result: Value = serde_json::from_str(&first_output)?;
    let first_worker_id = first_result
        .get("agent_id")
        .and_then(Value::as_str)
        .and_then(|id| ThreadId::from_string(id).ok())
        .expect("first direct spawn should return a valid agent_id");
    let second_output = root_after_second
        .function_call_output_text(SECOND_DIRECT_CALL_ID)
        .expect("second direct spawn output");
    assert!(
        second_output.contains("agent thread limit reached"),
        "second direct spawn should be rejected by the configured ceiling: {second_output}"
    );

    let first_worker_request = wait_for_captured_request(
        &first_worker_response,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && request.body_contains_text(FIRST_DIRECT_TASK)
        },
        "first direct worker",
    )
    .await;
    assert_request_assignment(&first_worker_request, WORKER_MODEL, "low");
    let first_worker = test.thread_manager.get_thread(first_worker_id).await?;
    wait_for_event(first_worker.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let _ = first_worker_after_gate.single_request();

    let followup_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL) && body_contains(request, FOLLOWUP_TASK)
        },
        sse(vec![
            ev_response_created("team-limit-worker-followup"),
            ev_assistant_message("team-limit-worker-followup-message", "followup complete"),
            ev_completed("team-limit-worker-followup"),
        ]),
    )
    .await;
    first_worker
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: FOLLOWUP_TASK.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(first_worker.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_request_assignment(&followup_response.single_request(), WORKER_MODEL, "low");

    let root_after_third = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && body_contains(request, REPLACEMENT_PROMPT)
                && !request_has_function_call_output(request, THIRD_DIRECT_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-limit-root-5"),
            ev_function_call_with_namespace(
                THIRD_DIRECT_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &third_args,
            ),
            ev_completed("team-limit-root-5"),
        ]),
    )
    .await;
    let root_after_third_output = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, THIRD_DIRECT_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-limit-root-6"),
            ev_assistant_message("team-limit-root-replacement", "replacement worker complete"),
            ev_completed("team-limit-root-6"),
        ]),
    )
    .await;
    let replacement_worker_response = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL) && body_contains(request, THIRD_DIRECT_TASK)
        },
        sse_response(sse(vec![
            ev_response_created("team-limit-worker-replacement"),
            ev_assistant_message(
                "team-limit-worker-replacement-message",
                "replacement worker complete",
            ),
            ev_completed("team-limit-worker-replacement"),
        ])),
    )
    .await;
    submit_turn(
        &test.codex,
        REPLACEMENT_PROMPT,
        ThreadSettingsOverrides::default(),
    )
    .await?;
    let third_output = root_after_third_output
        .function_call_output_text(THIRD_DIRECT_CALL_ID)
        .expect("replacement direct spawn output");
    let third_result: Value = serde_json::from_str(&third_output)?;
    let third_worker_id = third_result
        .get("agent_id")
        .and_then(Value::as_str)
        .and_then(|id| ThreadId::from_string(id).ok())
        .expect("replacement direct spawn should return a valid agent_id");
    assert_ne!(
        third_worker_id, first_worker_id,
        "capacity reuse must admit a different direct Worker thread"
    );
    let replacement_worker = test.thread_manager.get_thread(third_worker_id).await?;
    let replacement_request = wait_for_captured_request(
        &replacement_worker_response,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && request.body_contains_text(THIRD_DIRECT_TASK)
        },
        "replacement direct worker",
    )
    .await;
    assert_request_assignment(&replacement_request, WORKER_MODEL, "low");
    wait_for_event(replacement_worker.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let _ = root_after_third.single_request();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_snapshot_survives_cold_resume_and_profile_change() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let initial_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("team-before-resume"),
            ev_completed("team-before-resume"),
        ]),
    )
    .await;
    let mut initial_builder = test_codex()
        .with_model(INITIAL_MODEL)
        .with_config(|config| {
            configure_team(config, TeamMode::LeadWorker);
        });
    let initial = initial_builder.build_with_auto_env(&server).await?;
    submit_turn(
        &initial.codex,
        "before resume",
        ThreadSettingsOverrides::default(),
    )
    .await?;
    assert_assignment(
        &initial.codex.config_snapshot().await,
        TeamMode::LeadWorker,
        TeamRole::Lead,
    );
    submit_thread_settings(
        &initial.codex,
        ThreadSettingsOverrides {
            team: Some(ThreadTeamSettingsUpdate {
                mode: TeamMode::LeadWorker,
                role: Some(TeamRole::Lead),
                model: Some(WORKER_MODEL.to_string()),
                reasoning_effort: Some(ReasoningEffort::Low),
                lead_balance: Some(4),
            }),
            ..Default::default()
        },
    )
    .await?;
    let changed_snapshot = initial.codex.config_snapshot().await;
    let changed_team = changed_snapshot.team.as_ref().expect("team snapshot");
    assert_eq!(
        (
            changed_snapshot.model,
            changed_snapshot.reasoning_effort,
            changed_team.lead_model.as_deref(),
            changed_team.lead_reasoning_effort.clone(),
            changed_team.lead_balance,
            changed_team.worker_model.as_deref(),
        ),
        (
            WORKER_MODEL.to_string(),
            Some(ReasoningEffort::Low),
            Some(WORKER_MODEL),
            Some(ReasoningEffort::Low),
            Some(4),
            Some(WORKER_MODEL),
        )
    );

    let mut resume_builder = test_codex().with_config(|config| {
        config.model = Some("gpt-5.2".to_string());
        config.model_reasoning_effort = Some(ReasoningEffort::Low);
        config.team = team_config(TeamMode::LeadWorker, "gpt-5.5", "gpt-5.4");
        config.team_mode = TeamMode::LeadWorker;
    });
    let resumed = resume_builder.restart(&server, &initial).await?;
    let snapshot = resumed.codex.config_snapshot().await;
    assert_eq!(snapshot.model, WORKER_MODEL);
    assert_eq!(snapshot.reasoning_effort, Some(ReasoningEffort::Low));
    let team = snapshot.team.as_ref().expect("team snapshot");
    assert_eq!(
        (
            team.mode,
            team.role,
            team.lead_model.as_deref(),
            team.lead_reasoning_effort.clone(),
            team.lead_balance,
            team.worker_model.as_deref(),
        ),
        (
            TeamMode::LeadWorker,
            Some(TeamRole::Lead),
            Some(WORKER_MODEL),
            Some(ReasoningEffort::Low),
            Some(4),
            Some(WORKER_MODEL),
        )
    );
    let resumed_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("team-after-resume"),
            ev_completed("team-after-resume"),
        ]),
    )
    .await;
    submit_turn(
        &resumed.codex,
        "after resume",
        ThreadSettingsOverrides::default(),
    )
    .await?;

    assert_eq!(
        resumed.config.team,
        team_config(TeamMode::LeadWorker, "gpt-5.5", "gpt-5.4")
    );
    let initial_request = initial_response.single_request();
    let resumed_request = resumed_response.single_request();
    assert_request_assignment(&initial_request, LEAD_MODEL, "max");
    assert!(
        !team_instruction_fragments(&initial_request)
            .iter()
            .any(|fragment| fragment.contains("Lead usage/confidence balance"))
    );
    assert_request_assignment(&resumed_request, WORKER_MODEL, "low");
    assert!(
        team_instruction_fragments(&resumed_request)
            .iter()
            .any(|fragment| fragment.contains("Confidence focused"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_off_snapshot_survives_cold_resume_and_ignores_enabled_global_config() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let initial_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("team-off-before-resume"),
            ev_completed("team-off-before-resume"),
        ]),
    )
    .await;
    let mut initial_builder = test_codex()
        .with_model(INITIAL_MODEL)
        .with_config(|config| {
            config.model_reasoning_effort = Some(ReasoningEffort::Medium);
        });
    let initial = initial_builder.build_with_auto_env(&server).await?;
    assert!(initial.codex.config_snapshot().await.team.is_none());

    submit_thread_settings(&initial.codex, team_mode_update(TeamMode::Off)).await?;
    let initial_off = initial.codex.config_snapshot().await;
    assert_eq!(initial_off.model, INITIAL_MODEL);
    assert_eq!(initial_off.reasoning_effort, Some(ReasoningEffort::Medium));
    assert_eq!(
        initial_off.team.as_ref().map(|team| team.mode),
        Some(TeamMode::Off)
    );
    submit_turn(
        &initial.codex,
        "before team off resume",
        ThreadSettingsOverrides::default(),
    )
    .await?;

    let resumed_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("team-off-after-resume"),
            ev_completed("team-off-after-resume"),
        ]),
    )
    .await;
    let mut resume_builder = test_codex().with_config(|config| {
        config.model = Some(REQUESTED_MODEL.to_string());
        config.model_reasoning_effort = Some(ReasoningEffort::Low);
        configure_team(config, TeamMode::LeadWorker);
    });
    let resumed = resume_builder.restart(&server, &initial).await?;
    assert_eq!(
        resumed.config.team,
        team_config(TeamMode::LeadWorker, LEAD_MODEL, WORKER_MODEL)
    );
    let resumed_snapshot = resumed.codex.config_snapshot().await;
    assert_eq!(resumed_snapshot.model, INITIAL_MODEL);
    assert_eq!(
        resumed_snapshot.reasoning_effort,
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(
        resumed_snapshot.team.as_ref().map(|team| team.mode),
        Some(TeamMode::Off)
    );
    submit_turn(
        &resumed.codex,
        "after team off resume",
        ThreadSettingsOverrides::default(),
    )
    .await?;

    assert_request_assignment(&initial_response.single_request(), INITIAL_MODEL, "medium");
    assert_request_assignment(&resumed_response.single_request(), INITIAL_MODEL, "medium");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_review_uses_worker_assignment_over_review_model_override() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let active_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("team-review-active"),
            ev_completed("team-review-active"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_model(INITIAL_MODEL)
        .with_config(|config| {
            config.review_model = Some(REQUESTED_MODEL.to_string());
            config.model_reasoning_effort = Some(ReasoningEffort::Medium);
            configure_team(config, TeamMode::Off);
        });
    let test = builder.build_with_auto_env(&server).await?;
    submit_thread_settings(&test.codex, team_mode_update(TeamMode::LeadWorker)).await?;
    assert_assignment(
        &test.codex.config_snapshot().await,
        TeamMode::LeadWorker,
        TeamRole::Lead,
    );
    test.codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "review the active team assignment".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert_request_assignment(&active_response.single_request(), WORKER_MODEL, "low");

    submit_thread_settings(&test.codex, team_mode_update(TeamMode::Off)).await?;
    assert_eq!(
        test.codex
            .config_snapshot()
            .await
            .team
            .as_ref()
            .map(|team| team.mode),
        Some(TeamMode::Off)
    );
    assert_eq!(
        test.codex.config_snapshot().await.reasoning_effort,
        Some(ReasoningEffort::Medium)
    );
    let off_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("team-review-off"),
            ev_completed("team-review-off"),
        ]),
    )
    .await;
    test.codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "review after team mode is disabled".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert_request_assignment(&off_response.single_request(), REQUESTED_MODEL, "medium");
    Ok(())
}
