use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::create_fake_rollout;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadControlMode;
use codex_app_server_protocol::ThreadControlReadParams;
use codex_app_server_protocol::ThreadControlReadResponse;
use codex_app_server_protocol::ThreadControlReleaseParams;
use codex_app_server_protocol::ThreadControlReleaseResponse;
use codex_app_server_protocol::ThreadControlSetParams;
use codex_app_server_protocol::ThreadControlSetResponse;
use codex_app_server_protocol::ThreadScratchpadContinuousPolicySetParams;
use codex_app_server_protocol::ThreadScratchpadContinuousPolicySetResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn thread_control_set_read_and_release_round_trip() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let state_db = init_state_db(codex_home.path()).await?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let start_id = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(start_resp)?;

    let set_id = mcp
        .send_thread_control_set_request(ThreadControlSetParams {
            thread_id: thread.id.clone(),
            mode: ThreadControlMode::Orchestrator,
            reason: "Keep supervising the spawned worker threads".to_string(),
            release_channel: Some("imessage".to_string()),
            watch_interval_seconds: Some(30),
            target_thread_ids: Some(vec![
                "00000000-0000-0000-0000-000000000010".to_string(),
                "00000000-0000-0000-0000-000000000011".to_string(),
            ]),
        })
        .await?;
    let set_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(set_id)),
    )
    .await??;
    let ThreadControlSetResponse { control } = to_response::<ThreadControlSetResponse>(set_resp)?;
    assert_eq!(control.mode, ThreadControlMode::Orchestrator);
    assert_eq!(
        control.reason,
        "Keep supervising the spawned worker threads"
    );
    assert_eq!(control.release_channel.as_deref(), Some("imessage"));
    assert_eq!(control.watch_interval_seconds, Some(30));
    assert_eq!(
        control.target_thread_ids,
        vec![
            "00000000-0000-0000-0000-000000000010".to_string(),
            "00000000-0000-0000-0000-000000000011".to_string()
        ]
    );
    assert_eq!(control.released_at, None);

    let read_id = mcp
        .send_thread_control_read_request(ThreadControlReadParams {
            thread_id: thread.id.clone(),
        })
        .await?;
    let read_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(read_id)),
    )
    .await??;
    let ThreadControlReadResponse { control } =
        to_response::<ThreadControlReadResponse>(read_resp)?;
    let control = control.expect("control");
    assert_eq!(control.mode, ThreadControlMode::Orchestrator);
    assert_eq!(
        control.reason,
        "Keep supervising the spawned worker threads"
    );

    let release_id = mcp
        .send_thread_control_release_request(ThreadControlReleaseParams {
            thread_id: thread.id.clone(),
        })
        .await?;
    let release_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(release_id)),
    )
    .await??;
    let ThreadControlReleaseResponse { control } =
        to_response::<ThreadControlReleaseResponse>(release_resp)?;
    let control = control.expect("released control");
    assert!(control.released_at.is_some());

    let read_after_release_id = mcp
        .send_thread_control_read_request(ThreadControlReadParams {
            thread_id: thread.id.clone(),
        })
        .await?;
    let read_after_release_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(read_after_release_id)),
    )
    .await??;
    let ThreadControlReadResponse { control } =
        to_response::<ThreadControlReadResponse>(read_after_release_resp)?;
    assert_eq!(control, None);

    let stored_control = state_db
        .get_thread_control(codex_protocol::ThreadId::from_string(&thread.id)?)
        .await?
        .expect("stored control");
    assert!(stored_control.released_at.is_some());
    Ok(())
}

#[tokio::test]
async fn thread_control_set_rejects_orchestrator_mode_for_stored_thread() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let _state_db = init_state_db(codex_home.path()).await?;

    let thread_id = create_fake_rollout(
        codex_home.path(),
        "2025-01-06T08-30-00",
        "2025-01-06T08:30:00Z",
        "Stored thread preview",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let set_id = mcp
        .send_thread_control_set_request(ThreadControlSetParams {
            thread_id,
            mode: ThreadControlMode::Orchestrator,
            reason: "Keep supervising the spawned worker threads".to_string(),
            release_channel: Some("imessage".to_string()),
            watch_interval_seconds: Some(30),
            target_thread_ids: Some(vec!["00000000-0000-0000-0000-000000000010".to_string()]),
        })
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(set_id)),
    )
    .await??;
    assert_eq!(
        error.error.message,
        "orchestrator mode currently requires a loaded thread"
    );
    Ok(())
}

#[tokio::test]
async fn thread_scratchpad_continuous_policy_set_round_trip() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let _state_db = init_state_db(codex_home.path()).await?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let start_id = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(start_resp)?;

    let set_id = mcp
        .send_thread_scratchpad_continuous_policy_set_request(
            ThreadScratchpadContinuousPolicySetParams {
                thread_id: thread.id.clone(),
                enabled: true,
            },
        )
        .await?;
    let set_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(set_id)),
    )
    .await??;
    let ThreadScratchpadContinuousPolicySetResponse {} =
        to_response::<ThreadScratchpadContinuousPolicySetResponse>(set_resp)?;

    let scratchpad_path = codex_home
        .path()
        .join("scratchpad")
        .join("entries")
        .join(format!("{}.json", thread.id));
    let value = wait_for_scratchpad_json(&scratchpad_path).await?;
    assert_eq!(
        value["run_policy"]["continuous"]["enabled"],
        serde_json::json!(true)
    );
    assert_eq!(value["origin_thread_id"], serde_json::json!(thread.id));
    Ok(())
}

async fn init_state_db(codex_home: &Path) -> Result<Arc<StateRuntime>> {
    let state_db = StateRuntime::init(codex_home.to_path_buf(), "mock_provider".into()).await?;
    state_db
        .mark_backfill_complete(/*last_watermark*/ None)
        .await?;
    Ok(state_db)
}

async fn wait_for_scratchpad_json(path: &Path) -> Result<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + DEFAULT_READ_TIMEOUT;
    loop {
        match tokio::fs::read_to_string(path).await {
            Ok(text) => return Ok(serde_json::from_str(&text)?),
            Err(err)
                if err.kind() == std::io::ErrorKind::NotFound
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn create_config_toml(codex_home: &Path, server_uri: &str) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"

model_provider = "mock_provider"
suppress_unstable_features_warning = true

[features]
sqlite = true

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}
