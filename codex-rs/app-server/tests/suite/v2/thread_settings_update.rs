use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use app_test_support::write_models_cache;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_app_server_protocol::ThreadSettingsUpdateResponse;
use codex_app_server_protocol::ThreadSettingsUpdatedNotification;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadTeamSettings;
use codex_app_server_protocol::ThreadTeamSettingsUpdate;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::ThreadUnsubscribeStatus;
use codex_app_server_protocol::ThreadUsagePolicy;
use codex_app_server_protocol::ThreadUsagePolicyParams;
use codex_app_server_protocol::ThreadUsageResumeResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_core::test_support::all_model_presets;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::Settings;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::TeamMode;
use codex_protocol::protocol::TeamRole;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn thread_settings_update_emits_notification_and_updates_future_turns() -> Result<()> {
    let server = create_mock_responses_server_sequence_unchecked(vec![
        create_final_assistant_message_sse_response("done")?,
    ])
    .await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    write_models_cache(codex_home.path())?;
    let (model_id, service_tier_id) = service_tier_model_and_tier_id()?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let started = start_thread(&mut mcp).await?;
    assert_eq!(started.usage_policy, ThreadUsagePolicy::default());
    let thread = started.thread;

    let usage_policy = ThreadUsagePolicy {
        auto_resume: true,
        minimum_remaining_percent: Some(20),
    };
    let usage_policy_params = ThreadUsagePolicyParams {
        auto_resume: Some(true),
        minimum_remaining_percent: Some(Some(20)),
    };
    send_thread_settings_update(
        &mut mcp,
        ThreadSettingsUpdateParams {
            thread_id: thread.id.clone(),
            collaboration_mode: Some(CollaborationMode {
                mode: ModeKind::Default,
                settings: Settings {
                    model: model_id.clone(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            }),
            service_tier: Some(Some(service_tier_id.clone())),
            usage_policy: Some(usage_policy_params),
            ..Default::default()
        },
    )
    .await?;
    assert!(
        received_response_bodies(&server).await?.is_empty(),
        "settings-only update should not start a model request"
    );

    start_text_turn(&mut mcp, thread.id.clone()).await?;

    let updated = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(updated.thread_id, thread.id);
    assert_eq!(updated.thread_settings.model, model_id);
    assert_eq!(
        updated.thread_settings.service_tier.as_deref(),
        Some(service_tier_id.as_str())
    );
    assert_eq!(updated.thread_settings.usage_policy, usage_policy);

    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    // Loaded metadata must come from live settings, even if stored metadata is stale.
    let state_db = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    let mut stored = state_db
        .get_thread(codex_protocol::ThreadId::from_string(&thread.id)?)
        .await?
        .expect("completed thread should be persisted");
    stored.model = Some("stored-model".to_string());
    stored.reasoning_effort = Some(ReasoningEffort::Low);
    state_db.upsert_thread(&stored).await?;

    let unsubscribe_id = mcp
        .send_thread_unsubscribe_request(ThreadUnsubscribeParams {
            thread_id: thread.id.clone(),
        })
        .await?;
    let unsubscribed: ThreadUnsubscribeResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(unsubscribe_id)).await??;
    assert_eq!(unsubscribed.status, ThreadUnsubscribeStatus::Unsubscribed);

    for include_turns in [false, true] {
        let read_id = mcp
            .send_thread_read_request(ThreadReadParams {
                thread_id: thread.id.clone(),
                include_turns,
            })
            .await?;
        let read: ThreadReadResponse =
            timeout(DEFAULT_TIMEOUT, mcp.read_response(read_id)).await??;
        assert_eq!(read.thread.turns.len(), usize::from(include_turns));
        assert_eq!(
            (read.thread.model.as_deref(), read.thread.reasoning_effort),
            (Some(model_id.as_str()), None)
        );
    }
    let list_id = mcp
        .send_raw_request("thread/list", Some(json!({ "useStateDbOnly": true })))
        .await?;
    let listed: ThreadListResponse = timeout(DEFAULT_TIMEOUT, mcp.read_response(list_id)).await??;
    let listed = listed
        .data
        .iter()
        .find(|listed| listed.id == thread.id)
        .expect("loaded thread should be listed");
    assert_eq!(
        (listed.model.as_deref(), listed.reasoning_effort.clone()),
        (Some(model_id.as_str()), None)
    );

    let resume_id = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let resumed: ThreadResumeResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(resume_id)).await??;
    assert_eq!(resumed.usage_policy, usage_policy);

    let unsubscribe_id = mcp
        .send_thread_unsubscribe_request(ThreadUnsubscribeParams {
            thread_id: thread.id.clone(),
        })
        .await?;
    let unsubscribed: ThreadUnsubscribeResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(unsubscribe_id)).await??;
    assert_eq!(unsubscribed.status, ThreadUnsubscribeStatus::Unsubscribed);

    let request_bodies = received_response_bodies(&server).await?;
    assert!(
        request_bodies.iter().any(|body| {
            body.get("model").and_then(Value::as_str) == Some(model_id.as_str())
                && body.get("service_tier").and_then(Value::as_str)
                    == Some(service_tier_id.as_str())
        }),
        "future turn did not use updated model/service tier: {request_bodies:#?}"
    );
    Ok(())
}

#[tokio::test]
async fn thread_settings_update_usage_policy_preserves_omitted_nested_fields() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let thread = start_thread(&mut mcp).await?.thread;

    send_thread_settings_update(
        &mut mcp,
        ThreadSettingsUpdateParams {
            thread_id: thread.id.clone(),
            usage_policy: Some(ThreadUsagePolicyParams {
                auto_resume: Some(true),
                minimum_remaining_percent: Some(Some(20)),
            }),
            ..Default::default()
        },
    )
    .await?;
    let first_update = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(
        first_update.thread_settings.usage_policy,
        ThreadUsagePolicy {
            auto_resume: true,
            minimum_remaining_percent: Some(20),
        }
    );

    send_thread_settings_update(
        &mut mcp,
        ThreadSettingsUpdateParams {
            thread_id: thread.id.clone(),
            usage_policy: Some(ThreadUsagePolicyParams {
                auto_resume: Some(false),
                minimum_remaining_percent: None,
            }),
            ..Default::default()
        },
    )
    .await?;
    let partial_update = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(
        partial_update.thread_settings.usage_policy,
        ThreadUsagePolicy {
            auto_resume: false,
            minimum_remaining_percent: Some(20),
        }
    );

    send_thread_settings_update(
        &mut mcp,
        ThreadSettingsUpdateParams {
            thread_id: thread.id,
            usage_policy: Some(ThreadUsagePolicyParams {
                auto_resume: None,
                minimum_remaining_percent: Some(None),
            }),
            ..Default::default()
        },
    )
    .await?;
    let cleared_update = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(
        cleared_update.thread_settings.usage_policy,
        ThreadUsagePolicy {
            auto_resume: false,
            minimum_remaining_percent: None,
        }
    );
    Ok(())
}

#[tokio::test]
async fn thread_usage_resume_is_wake_only() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let thread = start_thread(&mut mcp).await?.thread;

    let request_id = mcp
        .send_raw_request(
            "thread/usage/resume",
            Some(json!({ "threadId": thread.id })),
        )
        .await?;
    let response: ThreadUsageResumeResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;
    assert_eq!(response, ThreadUsageResumeResponse {});
    assert!(
        received_response_bodies(&server).await?.is_empty(),
        "usage resume must not enqueue a model turn"
    );
    Ok(())
}

#[tokio::test]
async fn thread_settings_update_team_mode_is_sparse_and_fresh_threads_keep_defaults() -> Result<()>
{
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    create_team_config_toml(codex_home.path(), &server.uri())?;
    write_models_cache(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let started = start_thread(&mut mcp).await?;
    let thread_id = started.thread.id.clone();
    let default_team = ThreadTeamSettings {
        mode: TeamMode::Off,
        role: Some(TeamRole::Lead),
        lead_model: Some("gpt-6-astra".to_string()),
        lead_reasoning_effort: Some(ReasoningEffort::High),
        worker_model: Some("gpt-5.6-luna".to_string()),
        worker_reasoning_effort: Some(ReasoningEffort::Max),
        previous_model: None,
        previous_reasoning_effort: None,
    };
    assert_eq!(started.model, "mock-model");
    assert_eq!(started.reasoning_effort, None);
    assert_eq!(started.team, Some(default_team.clone()));

    send_thread_settings_update(
        &mut mcp,
        ThreadSettingsUpdateParams {
            thread_id: thread_id.clone(),
            team: Some(ThreadTeamSettingsUpdate {
                mode: TeamMode::LeadWorker,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await?;
    let enabled = read_thread_settings_updated(&mut mcp).await?;
    let enabled_team = ThreadTeamSettings {
        mode: TeamMode::LeadWorker,
        role: Some(TeamRole::Lead),
        lead_model: Some("gpt-6-astra".to_string()),
        lead_reasoning_effort: Some(ReasoningEffort::High),
        worker_model: Some("gpt-5.6-luna".to_string()),
        worker_reasoning_effort: Some(ReasoningEffort::Max),
        previous_model: Some("mock-model".to_string()),
        previous_reasoning_effort: None,
    };
    assert_eq!(enabled.thread_settings.model, "gpt-6-astra");
    assert_eq!(enabled.thread_settings.effort, Some(ReasoningEffort::High));
    assert_eq!(enabled.thread_settings.team, Some(enabled_team.clone()));

    send_thread_settings_update(
        &mut mcp,
        ThreadSettingsUpdateParams {
            thread_id: thread_id.clone(),
            team: Some(ThreadTeamSettingsUpdate {
                mode: TeamMode::LeadWorker,
                role: Some(TeamRole::Lead),
                model: Some("gpt-5.6-sol".to_string()),
                reasoning_effort: Some(ReasoningEffort::Low),
            }),
            ..Default::default()
        },
    )
    .await?;
    let profiled = read_thread_settings_updated(&mut mcp).await?;
    let profiled_team = ThreadTeamSettings {
        mode: TeamMode::LeadWorker,
        role: Some(TeamRole::Lead),
        lead_model: Some("gpt-5.6-sol".to_string()),
        lead_reasoning_effort: Some(ReasoningEffort::Low),
        worker_model: Some("gpt-5.6-luna".to_string()),
        worker_reasoning_effort: Some(ReasoningEffort::Max),
        previous_model: Some("mock-model".to_string()),
        previous_reasoning_effort: None,
    };
    assert_eq!(profiled.thread_settings.model, "gpt-5.6-sol");
    assert_eq!(profiled.thread_settings.effort, Some(ReasoningEffort::Low));
    assert_eq!(profiled.thread_settings.team, Some(profiled_team.clone()));

    let unsubscribe_id = mcp
        .send_thread_unsubscribe_request(ThreadUnsubscribeParams {
            thread_id: thread_id.clone(),
        })
        .await?;
    let unsubscribed: ThreadUnsubscribeResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(unsubscribe_id)).await??;
    assert_eq!(unsubscribed.status, ThreadUnsubscribeStatus::Unsubscribed);
    let resume_id = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            ..Default::default()
        })
        .await?;
    let resumed: ThreadResumeResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(resume_id)).await??;
    assert_eq!(resumed.model, "gpt-5.6-sol");
    assert_eq!(resumed.reasoning_effort, Some(ReasoningEffort::Low));
    assert_eq!(resumed.team, Some(profiled_team));

    let fresh = start_thread(&mut mcp).await?;
    assert_eq!(fresh.model, "mock-model");
    assert_eq!(fresh.reasoning_effort, None);
    assert_eq!(fresh.team, Some(default_team.clone()));

    send_thread_settings_update(
        &mut mcp,
        ThreadSettingsUpdateParams {
            thread_id,
            team: Some(ThreadTeamSettingsUpdate {
                mode: TeamMode::Off,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await?;
    let disabled = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(disabled.thread_settings.model, "mock-model");
    assert_eq!(disabled.thread_settings.effort, None);
    assert_eq!(
        disabled.thread_settings.team,
        Some(ThreadTeamSettings {
            mode: TeamMode::Off,
            role: Some(TeamRole::Lead),
            lead_model: Some("gpt-5.6-sol".to_string()),
            lead_reasoning_effort: Some(ReasoningEffort::Low),
            worker_model: Some("gpt-5.6-luna".to_string()),
            worker_reasoning_effort: Some(ReasoningEffort::Max),
            previous_model: None,
            previous_reasoning_effort: None,
        })
    );
    Ok(())
}

#[tokio::test]
async fn thread_settings_update_cwd_retargets_default_environment() -> Result<()> {
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_assistant_message("msg-1", "done"),
        responses::ev_completed("resp-1"),
    ]);
    let response_mock = responses::mount_sse_once(&server, body).await;
    let codex_home = TempDir::new()?;
    let initial_workspace = TempDir::new()?;
    let workspace = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let request_id = mcp
        .send_thread_start_request(ThreadStartParams {
            cwd: Some(initial_workspace.path().to_string_lossy().into_owned()),
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;

    send_thread_settings_update(
        &mut mcp,
        ThreadSettingsUpdateParams {
            thread_id: thread.id.clone(),
            cwd: Some(workspace.path().to_path_buf()),
            ..Default::default()
        },
    )
    .await?;
    let updated = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(updated.thread_settings.cwd.as_path(), workspace.path());

    start_text_turn(&mut mcp, thread.id).await?;
    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let environment_context = response_mock
        .single_request()
        .message_input_texts("user")
        .into_iter()
        .find(|text| text.starts_with("<environment_context>"))
        .context("environment context should be model visible")?;
    assert!(
        environment_context.contains(&format!(
            "<cwd>{}</cwd>",
            workspace.path().to_string_lossy()
        )),
        "default environment should use the updated cwd: {environment_context}"
    );
    assert!(
        environment_context.contains(&format!(
            "<workspace_roots><root>{}</root></workspace_roots>",
            workspace.path().to_string_lossy()
        )),
        "default workspace root should use the updated cwd: {environment_context}"
    );

    Ok(())
}

#[tokio::test]
async fn thread_settings_update_while_turn_is_active_emits_notification() -> Result<()> {
    let server = responses::start_mock_server().await;
    let first_response =
        responses::sse_response(create_final_assistant_message_sse_response("first done")?)
            .set_delay(Duration::from_secs(2));
    let _requests = responses::mount_response_sequence(&server, vec![first_response]).await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let thread = start_thread(&mut mcp).await?.thread;
    start_text_turn(&mut mcp, thread.id.clone()).await?;
    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/started"),
    )
    .await??;

    send_thread_settings_update(
        &mut mcp,
        ThreadSettingsUpdateParams {
            thread_id: thread.id.clone(),
            model: Some("mock-model-4".to_string()),
            ..Default::default()
        },
    )
    .await?;

    let updated = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(updated.thread_id, thread.id);
    assert_eq!(updated.thread_settings.model, "mock-model-4");

    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    Ok(())
}

#[tokio::test]
async fn thread_settings_update_null_service_tier_uses_default() -> Result<()> {
    let server = create_mock_responses_server_sequence_unchecked(vec![
        create_final_assistant_message_sse_response("done")?,
    ])
    .await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    write_models_cache(codex_home.path())?;
    let (model_id, service_tier_id) = service_tier_model_and_tier_id()?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let thread = start_thread(&mut mcp).await?.thread;

    send_thread_settings_update(
        &mut mcp,
        ThreadSettingsUpdateParams {
            thread_id: thread.id.clone(),
            model: Some(model_id.clone()),
            service_tier: Some(Some(service_tier_id.clone())),
            ..Default::default()
        },
    )
    .await?;

    let set_updated = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(set_updated.thread_id, thread.id);
    assert_eq!(
        set_updated.thread_settings.service_tier.as_deref(),
        Some(service_tier_id.as_str())
    );

    send_thread_settings_update(
        &mut mcp,
        ThreadSettingsUpdateParams {
            thread_id: thread.id.clone(),
            service_tier: Some(None),
            ..Default::default()
        },
    )
    .await?;

    let clear_updated = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(clear_updated.thread_id, thread.id);
    assert_eq!(clear_updated.thread_settings.model, model_id);
    assert_eq!(
        clear_updated.thread_settings.service_tier.as_deref(),
        Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE)
    );

    start_text_turn(&mut mcp, thread.id).await?;
    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let request_bodies = received_response_bodies(&server).await?;
    assert!(
        request_bodies.iter().any(|body| {
            body.get("model").and_then(Value::as_str) == Some(model_id.as_str())
                && body
                    .as_object()
                    .is_some_and(|object| !object.contains_key("service_tier"))
        }),
        "future turn did not clear service tier: {request_bodies:#?}"
    );
    Ok(())
}

#[tokio::test]
async fn thread_settings_update_rejects_sandbox_policy_with_permissions() -> Result<()> {
    let server = create_mock_responses_server_sequence_unchecked(Vec::new()).await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let thread = start_thread(&mut mcp).await?.thread;

    let request_id = mcp
        .send_thread_settings_update_request(ThreadSettingsUpdateParams {
            thread_id: thread.id,
            sandbox_policy: Some(SandboxPolicy::DangerFullAccess),
            permissions: Some(":workspace".to_string()),
            ..Default::default()
        })
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(
        error.error.message,
        "`permissions` cannot be combined with `sandboxPolicy`"
    );
    Ok(())
}

#[tokio::test]
async fn turn_start_settings_override_emits_thread_settings_updated() -> Result<()> {
    let server = create_mock_responses_server_sequence_unchecked(vec![
        create_final_assistant_message_sse_response("done")?,
    ])
    .await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let thread = start_thread(&mut mcp).await?.thread;
    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_notification_message("thread/started"),
    )
    .await??;

    let turn_request_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }],
            model: Some("mock-model-3".to_string()),
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(turn_request_id)).await??;
    assert!(!turn.id.is_empty());

    let updated = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(updated.thread_id, thread.id);
    assert_eq!(updated.thread_settings.model, "mock-model-3");

    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    Ok(())
}

async fn send_thread_settings_update(
    mcp: &mut TestAppServer,
    params: ThreadSettingsUpdateParams,
) -> Result<()> {
    let request_id = mcp.send_thread_settings_update_request(params).await?;
    let _: ThreadSettingsUpdateResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;
    Ok(())
}

async fn start_text_turn(mcp: &mut TestAppServer, thread_id: String) -> Result<()> {
    let turn_request_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id,
            input: vec![V2UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(turn_request_id)).await??;
    assert!(!turn.id.is_empty());
    Ok(())
}

async fn start_thread(mcp: &mut TestAppServer) -> Result<ThreadStartResponse> {
    let request_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await?
}

async fn read_thread_settings_updated(
    mcp: &mut TestAppServer,
) -> Result<ThreadSettingsUpdatedNotification> {
    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_notification("thread/settings/updated"),
    )
    .await?
}

async fn received_response_bodies(server: &wiremock::MockServer) -> Result<Vec<Value>> {
    let requests = server
        .received_requests()
        .await
        .context("failed to fetch received requests")?;
    let mut bodies = Vec::new();
    for request in requests {
        if request.url.path().ends_with("/responses") {
            bodies.push(request.body_json::<Value>()?);
        }
    }
    Ok(bodies)
}

fn service_tier_model_and_tier_id() -> Result<(String, String)> {
    let model = all_model_presets()
        .iter()
        .find(|preset| preset.show_in_picker && !preset.service_tiers.is_empty())
        .context("bundled model catalog should include a picker model with service tiers")?;
    Ok((model.id.clone(), model.service_tiers[0].id.clone()))
}

fn create_config_toml(codex_home: &std::path::Path, server_uri: &str) -> std::io::Result<()> {
    MockResponsesConfig::new(server_uri)
        .with_root_config("compact_prompt = \"compact\"\nmodel_auto_compact_token_limit = 200000")
        .with_provider_config("supports_websockets = false")
        .write(codex_home)
}

fn create_team_config_toml(codex_home: &std::path::Path, server_uri: &str) -> std::io::Result<()> {
    MockResponsesConfig::new(server_uri)
        .with_root_config("compact_prompt = \"compact\"\nmodel_auto_compact_token_limit = 200000")
        .with_provider_config("supports_websockets = false")
        .with_extra_config(
            r#"[team]
enabled = false

[team.lead]
model = "gpt-6-astra"
reasoning_effort = "high"

[team.worker]
model = "gpt-5.6-luna"
reasoning_effort = "max"
"#,
        )
        .write(codex_home)
}
