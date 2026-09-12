use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use app_test_support::create_mock_responses_server_sequence_with_delays_unchecked;
use codex_app_server_protocol::ThreadGoalGetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::sleep;
use tokio::time::timeout;
use wiremock::MockServer;

use super::connection_handling_websocket::DEFAULT_READ_TIMEOUT;

/// A running exec session is a legitimate external wait. Goal continuation
/// must remain parked until that exact process exits instead of issuing a
/// model polling turn immediately after the exec tool returns.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_waits_for_running_exec_before_continuation() -> Result<()> {
    let server = create_mock_responses_server_sequence_unchecked(vec![
        create_final_assistant_message_sse_response("initial-response")?,
        responses::sse(vec![
            responses::ev_response_created("goal-exec-response"),
            responses::ev_exec_command_call_with_args(
                "goal-exec-call",
                &serde_json::json!({
                    "cmd": "sleep 2",
                    "yield_time_ms": 250,
                }),
            ),
            responses::ev_completed("goal-exec-response"),
        ]),
        responses::sse(vec![
            responses::ev_response_created("goal-final-response"),
            responses::ev_assistant_message("goal-final-message", "goal resumed after exec"),
            responses::ev_completed("goal-final-response"),
        ]),
        create_final_assistant_message_sse_response("goal resumed after process exit")?,
    ])
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Goals)
        .with_sandbox_mode("danger-full-access")
        .with_root_config(&format!(r#"chatgpt_base_url = "{}""#, server.uri()))
        .write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?
        .thread;

    let initial_turn = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "materialize the goal thread".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: serde_json::Value = app_server.read_response(initial_turn).await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let goal_set = app_server
        .send_raw_request(
            "thread/goal/set",
            Some(serde_json::json!({
                "threadId": thread.id,
                "objective": "wait for the background command",
                "tokenBudget": 10_000,
            })),
        )
        .await?;
    let _: serde_json::Value = app_server.read_response(goal_set).await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("thread/goal/updated"),
    )
    .await??;

    wait_for_request_count(&server, /*count*/ 3).await?;
    sleep(Duration::from_millis(/*millis*/ 500)).await;
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("request recording enabled")
            .len(),
        3,
        "a running exec must not trigger an immediate Goal polling request"
    );

    wait_for_request_count(&server, /*count*/ 4).await?;
    let _ = app_server
        .read_stream_until_notification_message("turn/completed")
        .await?;
    Ok(())
}

/// Completion of a tracked process must not invalidate terminal handling for
/// the unchanged Goal while the same model turn is still in flight.
#[test_case::test_case("server_error", ThreadGoalStatus::Blocked; "turn_error")]
#[test_case::test_case("insufficient_quota", ThreadGoalStatus::UsageLimited; "usage_limit")]
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_wait_completion_preserves_terminal_error_for_active_turn(
    error_code: &str,
    expected_status: ThreadGoalStatus,
) -> Result<()> {
    let server = create_mock_responses_server_sequence_with_delays_unchecked(vec![
        (
            create_final_assistant_message_sse_response("initial-response")?,
            Duration::ZERO,
        ),
        (
            responses::sse(vec![
                responses::ev_response_created("goal-exec-response"),
                responses::ev_exec_command_call_with_args(
                    "goal-exec-call",
                    &serde_json::json!({
                        "cmd": "sleep 2",
                        "yield_time_ms": 250,
                    }),
                ),
                responses::ev_completed("goal-exec-response"),
            ]),
            Duration::ZERO,
        ),
        (
            responses::sse_failed(
                "goal-error-response",
                error_code,
                "terminal error after process completion",
            ),
            Duration::from_secs(3),
        ),
        (
            create_final_assistant_message_sse_response("unexpected polling response")?,
            Duration::ZERO,
        ),
    ])
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Goals)
        .with_sandbox_mode("danger-full-access")
        .with_root_config(&format!(r#"chatgpt_base_url = "{}""#, server.uri()))
        .write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?
        .thread;

    let initial_turn = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "materialize the goal thread".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: serde_json::Value = app_server.read_response(initial_turn).await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let goal_set = app_server
        .send_raw_request(
            "thread/goal/set",
            Some(serde_json::json!({
                "threadId": thread.id,
                "objective": "wait for the background command",
                "tokenBudget": 10_000,
            })),
        )
        .await?;
    let _: serde_json::Value = app_server.read_response(goal_set).await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("thread/goal/updated"),
    )
    .await??;

    // Request three is the delayed terminal error. The process from request two
    // is still running when this request is admitted.
    wait_for_request_count(&server, /*count*/ 3).await?;
    let goal_refresh = app_server
        .send_raw_request(
            "thread/goal/set",
            Some(serde_json::json!({
                "threadId": thread.id,
                "status": "active",
            })),
        )
        .await?;
    let _: serde_json::Value = app_server.read_response(goal_refresh).await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("thread/goal/updated"),
    )
    .await??;

    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    assert_eq!(
        3,
        server
            .received_requests()
            .await
            .expect("request recording enabled")
            .len(),
        "process completion must not trigger a polling turn after a terminal error"
    );

    let goal_get = app_server
        .send_raw_request(
            "thread/goal/get",
            Some(serde_json::json!({ "threadId": thread.id })),
        )
        .await?;
    let goal: ThreadGoalGetResponse = app_server.read_response(goal_get).await?;
    assert_eq!(expected_status, goal.goal.expect("goal exists").status);
    Ok(())
}

async fn wait_for_request_count(server: &MockServer, count: usize) -> Result<()> {
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            if server
                .received_requests()
                .await
                .is_some_and(|requests| requests.len() >= count)
            {
                return;
            }
            sleep(Duration::from_millis(/*millis*/ 25)).await;
        }
    })
    .await?;
    Ok(())
}
