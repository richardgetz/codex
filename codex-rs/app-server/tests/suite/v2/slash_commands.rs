use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::TestAppServer;
use app_test_support::write_chatgpt_auth;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::SlashCommandExecuteParams;
use codex_app_server_protocol::SlashCommandExecuteResponse;
use codex_app_server_protocol::SlashCommandListParams;
use codex_app_server_protocol::SlashCommandListResponse;
use codex_app_server_protocol::SlashCommandResultKind;
use codex_app_server_protocol::SlashCommandResultNotification;
use codex_config::types::AuthCredentialsStoreMode;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[tokio::test]
async fn slash_command_list_exposes_available_and_tui_only_commands() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;

    let response: SlashCommandListResponse = app
        .request(|request_id| ClientRequest::SlashCommandList {
            request_id,
            params: SlashCommandListParams {
                thread_id: None,
                side_conversation: false,
            },
        })
        .await?;

    assert!(response.capabilities.status);
    assert!(response.capabilities.spend);
    assert!(response.capabilities.reload);
    let status = response
        .commands
        .iter()
        .find(|command| command.name == "status")
        .expect("status should be listed");
    assert!(status.available);
    let plan = response
        .commands
        .iter()
        .find(|command| command.name == "plan")
        .expect("plan should be listed");
    assert!(!plan.available);
    assert_eq!(
        plan.unavailable_reason.as_deref(),
        Some("This command is currently TUI-only.")
    );
    Ok(())
}

#[tokio::test]
async fn slash_reload_reports_unavailable_without_replacing_the_server() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;

    let response: SlashCommandExecuteResponse = app
        .request(|request_id| ClientRequest::SlashCommandExecute {
            request_id,
            params: SlashCommandExecuteParams {
                thread_id: "test-thread".to_string(),
                command: "/reload".to_string(),
                args: String::new(),
            },
        })
        .await?;
    assert!(!response.ok);
    let reload = response.reload.expect("reload result");
    assert!(!reload.eligible);
    assert_eq!(reload.state, "unavailable");
    assert!(response.output.text.contains("unavailable"));

    let notification: SlashCommandResultNotification = timeout(
        READ_TIMEOUT,
        app.read_notification("slashCommand/result"),
    )
    .await??;
    assert_eq!(notification.command, "reload");
    assert_eq!(notification.result.command, "reload");
    assert_eq!(notification.result.ok, response.ok);
    assert_eq!(notification.result.output, response.output);
    Ok(())
}

#[tokio::test]
async fn slash_status_and_spend_use_the_configured_default_account() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!("chatgpt_base_url = \"{}\"\n", server.uri()),
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("default-token").account_id("default-account"),
        AuthCredentialsStoreMode::File,
    )?;

    Mock::given(method("GET"))
        .and(path("/api/codex/profiles/me"))
        .and(header("authorization", "Bearer default-token"))
        .and(header("chatgpt-account-id", "default-account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "stats": {
                "lifetime_tokens": 1234,
                "peak_daily_tokens": 500,
                "longest_running_turn_sec": 22,
                "current_streak_days": 3,
                "longest_streak_days": 9,
                "daily_usage_buckets": [{"start_date": "2026-09-20", "tokens": 321}]
            }
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .and(header("authorization", "Bearer default-token"))
        .and(header("chatgpt-account-id", "default-account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 12,
                    "limit_window_seconds": 3600,
                    "reset_after_seconds": 600,
                    "reset_at": 1789924200
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;

    let status: SlashCommandExecuteResponse = app
        .request(|request_id| ClientRequest::SlashCommandExecute {
            request_id,
            params: SlashCommandExecuteParams {
                thread_id: "test-thread".to_string(),
                command: "status".to_string(),
                args: String::new(),
            },
        })
        .await?;
    assert!(status.ok);
    assert_eq!(status.result_kind, SlashCommandResultKind::Status);
    assert!(status.output.text.contains("1234"));
    assert!(status.output.text.contains("Rate limits"));
    let _: SlashCommandResultNotification =
        timeout(READ_TIMEOUT, app.read_notification("slashCommand/result")).await??;

    let spend: SlashCommandExecuteResponse = app
        .request(|request_id| ClientRequest::SlashCommandExecute {
            request_id,
            params: SlashCommandExecuteParams {
                thread_id: "test-thread".to_string(),
                command: "spend".to_string(),
                args: String::new(),
            },
        })
        .await?;
    assert!(spend.ok);
    assert_eq!(spend.result_kind, SlashCommandResultKind::Spend);
    assert!(spend.output.text.contains("321"));
    Ok(())
}
