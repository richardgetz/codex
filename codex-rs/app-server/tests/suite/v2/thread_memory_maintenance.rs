use anyhow::Context;
use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadMemoryPolicySetResponse;
use codex_app_server_protocol::ThreadOrchestratorMemoryConsolidateResponse;
use codex_app_server_protocol::ThreadOrchestratorMemoryForgetResponse;
use codex_app_server_protocol::ThreadSettingsUpdatedNotification;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadUserPreferencesMemoryMigrateResponse;
use codex_app_server_protocol::ThreadUserPreferencesMemoryPolicySetResponse;
use codex_protocol::config_types::MemoryAccessPolicy;
use codex_protocol::config_types::UserPreferencesMemoryBucket;
use codex_protocol::config_types::UserPreferencesMemoryBucketPolicy;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::sleep;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn thread_memory_maintenance_endpoints_route_to_loaded_thread() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

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

    let consolidate_id = mcp
        .send_raw_request(
            "thread/orchestratorMemory/consolidate",
            Some(json!({ "threadId": thread.id })),
        )
        .await?;
    let consolidate_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(consolidate_id)),
    )
    .await??;
    let _: ThreadOrchestratorMemoryConsolidateResponse =
        to_response::<ThreadOrchestratorMemoryConsolidateResponse>(consolidate_resp)?;

    let forget_id = mcp
        .send_raw_request(
            "thread/orchestratorMemory/forget",
            Some(json!({
                "threadId": thread.id,
                "needle": "stale preference",
            })),
        )
        .await?;
    let forget_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(forget_id)),
    )
    .await??;
    let _: ThreadOrchestratorMemoryForgetResponse =
        to_response::<ThreadOrchestratorMemoryForgetResponse>(forget_resp)?;

    let migrate_id = mcp
        .send_raw_request(
            "thread/userPreferencesMemory/migrate",
            Some(json!({ "threadId": thread.id })),
        )
        .await?;
    let migrate_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(migrate_id)),
    )
    .await??;
    let _: ThreadUserPreferencesMemoryMigrateResponse =
        to_response::<ThreadUserPreferencesMemoryMigrateResponse>(migrate_resp)?;

    assert_eq!(thread.status, ThreadStatus::Idle);
    Ok(())
}

#[tokio::test]
async fn thread_memory_maintenance_endpoints_reject_disallowed_policy() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let start_id = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            memory_policy: Some(MemoryAccessPolicy::new(
                /*read*/ true, /*write*/ true,
            )),
            user_preferences_memory_policy: Some(UserPreferencesMemoryBucketPolicy {
                read_buckets: UserPreferencesMemoryBucket::all().to_vec(),
                write_buckets: vec![UserPreferencesMemoryBucket::DurablePreference],
            }),
            ..Default::default()
        })
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(start_resp)?;

    let consolidate_id = mcp
        .send_raw_request(
            "thread/orchestratorMemory/consolidate",
            Some(json!({ "threadId": thread.id })),
        )
        .await?;
    let consolidate_err = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(consolidate_id)),
    )
    .await??;
    assert!(
        consolidate_err
            .error
            .message
            .contains("requires write access to all user-preferences memory buckets")
    );

    let forget_id = mcp
        .send_raw_request(
            "thread/orchestratorMemory/forget",
            Some(json!({
                "threadId": thread.id,
                "needle": "stale preference",
            })),
        )
        .await?;
    let forget_err = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(forget_id)),
    )
    .await??;
    assert!(
        forget_err
            .error
            .message
            .contains("requires write access to all user-preferences memory buckets")
    );

    let policy_id = mcp
        .send_raw_request(
            "thread/memoryPolicy/set",
            Some(json!({
                "threadId": thread.id,
                "policy": { "read": false, "write": false },
            })),
        )
        .await?;
    let policy_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(policy_id)),
    )
    .await??;
    let _: ThreadMemoryPolicySetResponse =
        to_response::<ThreadMemoryPolicySetResponse>(policy_resp)?;

    let disabled_id = mcp
        .send_raw_request(
            "thread/orchestratorMemory/consolidate",
            Some(json!({ "threadId": thread.id })),
        )
        .await?;
    let disabled_err = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(disabled_id)),
    )
    .await??;
    assert!(
        disabled_err
            .error
            .message
            .contains("memory writes are disabled")
    );

    Ok(())
}

#[tokio::test]
async fn live_memory_policy_updates_emit_thread_settings_notification() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

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

    let disabled_memory_policy = MemoryAccessPolicy::new(/*read*/ false, /*write*/ false);
    let memory_policy_id = mcp
        .send_raw_request(
            "thread/memoryPolicy/set",
            Some(json!({
                "threadId": thread.id,
                "policy": { "read": false, "write": false },
            })),
        )
        .await?;
    let memory_settings = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(memory_settings.thread_id, thread.id);
    assert_eq!(
        memory_settings.thread_settings.memory_policy,
        disabled_memory_policy
    );

    let memory_policy_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(memory_policy_id)),
    )
    .await??;
    let _: ThreadMemoryPolicySetResponse =
        to_response::<ThreadMemoryPolicySetResponse>(memory_policy_resp)?;

    let bucket_policy = UserPreferencesMemoryBucketPolicy {
        read_buckets: vec![UserPreferencesMemoryBucket::OperatorPlaybook],
        write_buckets: vec![UserPreferencesMemoryBucket::OperatorPlaybook],
    };
    let bucket_policy_id = mcp
        .send_raw_request(
            "thread/userPreferencesMemoryPolicy/set",
            Some(json!({
                "threadId": thread.id,
                "policy": {
                    "readBuckets": ["operator_playbook"],
                    "writeBuckets": ["operator_playbook"],
                },
            })),
        )
        .await?;
    let bucket_settings = read_thread_settings_updated(&mut mcp).await?;
    assert_eq!(bucket_settings.thread_id, thread.id);
    assert_eq!(
        bucket_settings.thread_settings.memory_policy,
        disabled_memory_policy
    );
    assert_eq!(
        bucket_settings
            .thread_settings
            .user_preferences_memory_policy,
        bucket_policy
    );

    let bucket_policy_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(bucket_policy_id)),
    )
    .await??;
    let _: ThreadUserPreferencesMemoryPolicySetResponse =
        to_response::<ThreadUserPreferencesMemoryPolicySetResponse>(bucket_policy_resp)?;

    Ok(())
}

#[tokio::test]
async fn thread_memory_consolidate_uses_live_memory_policy_after_start() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let user_preferences_dir = codex_home
        .path()
        .join("memories")
        .join("extensions")
        .join("user_preferences");
    std::fs::create_dir_all(&user_preferences_dir)?;
    std::fs::write(
        user_preferences_dir.join("preferences.jsonl"),
        "{\"observed_at\":\"2026-04-25T00:00:00Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\",\"bucket\":\"durable_preference\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"concise updates\",\"candidate\":\"Prefer concise implementation updates\",\"source_excerpt\":\"be concise\",\"confidence\":0.8}\n",
    )?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let start_id = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            memory_policy: Some(MemoryAccessPolicy::new(
                /*read*/ false, /*write*/ false,
            )),
            ..Default::default()
        })
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(start_resp)?;

    let policy_id = mcp
        .send_raw_request(
            "thread/memoryPolicy/set",
            Some(json!({
                "threadId": thread.id,
                "policy": { "read": true, "write": true },
            })),
        )
        .await?;
    let policy_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(policy_id)),
    )
    .await??;
    let _: ThreadMemoryPolicySetResponse =
        to_response::<ThreadMemoryPolicySetResponse>(policy_resp)?;

    let consolidate_id = mcp
        .send_raw_request(
            "thread/orchestratorMemory/consolidate",
            Some(json!({ "threadId": thread.id })),
        )
        .await?;
    let consolidate_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(consolidate_id)),
    )
    .await??;
    let _: ThreadOrchestratorMemoryConsolidateResponse =
        to_response::<ThreadOrchestratorMemoryConsolidateResponse>(consolidate_resp)?;

    let summary_path = user_preferences_dir.join("summary.md");
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            if let Ok(summary) = tokio::fs::read_to_string(&summary_path).await
                && summary.contains("Prefer concise implementation updates")
            {
                return;
            }
            sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await?;

    Ok(())
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

async fn read_thread_settings_updated(
    mcp: &mut McpProcess,
) -> Result<ThreadSettingsUpdatedNotification> {
    let notification: JSONRPCNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("thread/settings/updated"),
    )
    .await??;
    let params = notification
        .params
        .context("thread/settings/updated should include params")?;
    Ok(serde_json::from_value(params)?)
}
