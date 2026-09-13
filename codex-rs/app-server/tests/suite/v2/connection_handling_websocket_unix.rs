//! Signal-driven shutdown tests for Unix websocket servers.

use super::connection_handling_websocket::DEFAULT_READ_TIMEOUT;
use super::connection_handling_websocket::WsClient;
use super::connection_handling_websocket::connect_websocket;
use super::connection_handling_websocket::create_config_toml;
use super::connection_handling_websocket::read_response_for_id;
use super::connection_handling_websocket::read_error_for_id;
use super::connection_handling_websocket::read_notification_for_method;
use super::connection_handling_websocket::send_request;
use super::connection_handling_websocket::spawn_websocket_server;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::to_response;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerLifecyclePhase;
use codex_app_server_protocol::ServerLifecycleReadResponse;
use codex_app_server_protocol::ServerLifecycleUpdatedNotification;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput as V2UserInput;
use core_test_support::responses;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
#[cfg(unix)]
use std::process::Command as StdCommand;
use tempfile::TempDir;
use tokio::process::Child;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use wiremock::Mock;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

#[cfg(unix)]
#[tokio::test]
async fn websocket_transport_ctrl_c_waits_for_running_turn_before_exit() -> Result<()> {
    let GracefulCtrlCFixture {
        _codex_home,
        _server,
        mut process,
        mut ws,
        ..
    } = start_ctrl_c_restart_fixture(Duration::from_secs(3)).await?;

    send_sigint(&process)?;
    assert_process_does_not_exit_within(&mut process, Duration::from_millis(300)).await?;

    let status = wait_for_process_exit_within(
        &mut process,
        Duration::from_secs(10),
        "timed out waiting for graceful Ctrl-C restart shutdown",
    )
    .await?;
    assert!(status.success(), "expected graceful exit, got {status}");

    expect_websocket_disconnect(&mut ws).await?;

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn websocket_transport_second_ctrl_c_forces_exit_while_turn_running() -> Result<()> {
    let GracefulCtrlCFixture {
        _codex_home,
        _server,
        mut process,
        mut ws,
        ..
    } = start_ctrl_c_restart_fixture(Duration::from_secs(3)).await?;

    send_sigint(&process)?;
    assert_process_does_not_exit_within(&mut process, Duration::from_millis(300)).await?;

    send_sigint(&process)?;
    let status = wait_for_process_exit_within(
        &mut process,
        Duration::from_secs(2),
        "timed out waiting for forced Ctrl-C restart shutdown",
    )
    .await?;
    assert!(status.success(), "expected graceful exit, got {status}");

    expect_websocket_disconnect(&mut ws).await?;

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn websocket_transport_sigterm_waits_for_running_turn_before_exit() -> Result<()> {
    let GracefulCtrlCFixture {
        _codex_home,
        _server,
        mut process,
        mut ws,
        ..
    } = start_ctrl_c_restart_fixture(Duration::from_secs(3)).await?;

    send_sigterm(&process, _codex_home.path())?;
    assert_process_does_not_exit_within(&mut process, Duration::from_millis(300)).await?;

    let status = wait_for_process_exit_within(
        &mut process,
        Duration::from_secs(10),
        "timed out waiting for graceful SIGTERM restart shutdown",
    )
    .await?;
    assert!(status.success(), "expected graceful exit, got {status}");

    expect_websocket_disconnect(&mut ws).await?;

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn websocket_transport_second_sigterm_forces_exit_while_turn_running() -> Result<()> {
    let GracefulCtrlCFixture {
        _codex_home,
        _server,
        mut process,
        mut ws,
        ..
    } = start_ctrl_c_restart_fixture(Duration::from_secs(3)).await?;

    send_sigterm(&process, _codex_home.path())?;
    assert_process_does_not_exit_within(&mut process, Duration::from_millis(300)).await?;

    send_sigterm(&process, _codex_home.path())?;
    let status = wait_for_process_exit_within(
        &mut process,
        Duration::from_secs(2),
        "timed out waiting for forced SIGTERM restart shutdown",
    )
    .await?;
    assert!(status.success(), "expected graceful exit, got {status}");

    expect_websocket_disconnect(&mut ws).await?;

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn websocket_transport_repeated_sighup_keeps_waiting_for_running_turn() -> Result<()> {
    let GracefulCtrlCFixture {
        _codex_home,
        _server,
        mut process,
        mut ws,
        ..
    } = start_ctrl_c_restart_fixture(Duration::from_secs(3)).await?;

    send_sighup(&process)?;
    assert_process_does_not_exit_within(&mut process, Duration::from_millis(300)).await?;

    send_sighup(&process)?;
    assert_process_does_not_exit_within(&mut process, Duration::from_millis(300)).await?;

    let status = wait_for_process_exit_within(
        &mut process,
        Duration::from_secs(10),
        "timed out waiting for graceful repeated SIGHUP restart shutdown",
    )
    .await?;
    assert!(status.success(), "expected graceful exit, got {status}");

    expect_websocket_disconnect(&mut ws).await?;

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn websocket_transport_delivers_lifecycle_before_idle_disconnect() -> Result<()> {
    let IdleCtrlCFixture {
        _codex_home,
        _server,
        mut process,
        mut ws,
    } = start_idle_ctrl_c_restart_fixture().await?;

    send_sighup(&process)?;
    let notification = read_notification_for_method(&mut ws, "server/lifecycle/updated").await?;
    let update: ServerLifecycleUpdatedNotification = serde_json::from_value(
        notification
            .params
            .context("lifecycle notification should include params")?,
    )?;
    assert_eq!(update.phase, ServerLifecyclePhase::Draining);
    assert!(update.transition_id.is_some());
    assert_eq!(update.running_assistant_turns, 0);

    let status = wait_for_process_exit_within(
        &mut process,
        Duration::from_secs(2),
        "timed out waiting for idle graceful restart shutdown",
    )
    .await?;
    assert!(status.success(), "expected graceful exit, got {status}");
    expect_websocket_disconnect(&mut ws).await?;

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn websocket_transport_delivers_forced_lifecycle_before_disconnect() -> Result<()> {
    let GracefulCtrlCFixture {
        _codex_home,
        _server,
        mut process,
        mut ws,
        ..
    } = start_ctrl_c_restart_fixture(Duration::from_secs(30)).await?;

    send_sigint(&process)?;
    let draining_notification =
        read_notification_for_method(&mut ws, "server/lifecycle/updated").await?;
    let draining: ServerLifecycleUpdatedNotification = serde_json::from_value(
        draining_notification
            .params
            .context("draining lifecycle notification should include params")?,
    )?;

    send_sigint(&process)?;
    let forced_notification =
        read_notification_for_method(&mut ws, "server/lifecycle/updated").await?;
    let forced: ServerLifecycleUpdatedNotification = serde_json::from_value(
        forced_notification
            .params
            .context("forced lifecycle notification should include params")?,
    )?;
    assert_eq!(forced.phase, ServerLifecyclePhase::Forced);
    assert_eq!(forced.daemon_instance_id, draining.daemon_instance_id);
    assert_eq!(forced.transition_id, draining.transition_id);
    assert_eq!(forced.running_assistant_turns, 1);

    let status = wait_for_process_exit_within(
        &mut process,
        Duration::from_secs(2),
        "timed out waiting for forced graceful restart shutdown",
    )
    .await?;
    assert!(status.success(), "expected graceful exit, got {status}");
    expect_websocket_disconnect(&mut ws).await?;

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn websocket_transport_reports_lifecycle_and_preserves_read_and_interrupt_during_drain(
) -> Result<()> {
    let GracefulCtrlCFixture {
        _codex_home,
        _server,
        mut process,
        mut ws,
        thread_id,
        turn_id,
    } = start_ctrl_c_restart_fixture(Duration::from_secs(30)).await?;

    send_request(
        &mut ws,
        "server/lifecycle/read",
        /*id*/ 4,
        Some(serde_json::json!({})),
    )
    .await?;
    let initial_response = read_response_for_id(&mut ws, /*id*/ 4).await?;
    let initial: ServerLifecycleReadResponse = to_response(initial_response)?;
    assert_eq!(initial.phase, ServerLifecyclePhase::Ready);
    assert!(initial.transition_id.is_none());
    assert_eq!(initial.running_assistant_turns, 1);
    assert!(!initial.daemon_instance_id.is_empty());

    send_sigint(&process)?;
    let notification = read_notification_for_method(&mut ws, "server/lifecycle/updated").await?;
    let update: ServerLifecycleUpdatedNotification = serde_json::from_value(
        notification
            .params
            .context("lifecycle notification should include params")?,
    )?;
    assert_eq!(update.daemon_instance_id, initial.daemon_instance_id);
    assert_eq!(update.phase, ServerLifecyclePhase::Draining);
    assert!(update.transition_id.is_some());
    assert_eq!(update.running_assistant_turns, 1);

    send_request(
        &mut ws,
        "turn/start",
        /*id*/ 5,
        Some(serde_json::json!({
            "threadId": thread_id,
            "input": [{
                "type": "text",
                "text": "should be rejected during drain",
                "textElements": []
            }]
        })),
    )
    .await?;
    let rejected = read_error_for_id(&mut ws, /*id*/ 5).await?;
    assert!(rejected.error.message.contains("draining"));

    send_request(
        &mut ws,
        "thread/usage/resume",
        /*id*/ 15,
        Some(serde_json::json!({"threadId": thread_id})),
    )
    .await?;
    let rejected_usage_resume = read_error_for_id(&mut ws, /*id*/ 15).await?;
    assert!(rejected_usage_resume.error.message.contains("draining"));

    send_request(
        &mut ws,
        "thread/goal/set",
        /*id*/ 9,
        Some(serde_json::json!({
            "threadId": thread_id,
            "objective": "should be rejected during drain"
        })),
    )
    .await?;
    let rejected_goal = read_error_for_id(&mut ws, /*id*/ 9).await?;
    assert!(rejected_goal.error.message.contains("draining"));

    send_request(
        &mut ws,
        "thread/activity/continue",
        /*id*/ 10,
        Some(serde_json::json!({"threadId": thread_id})),
    )
    .await?;
    let rejected_continue = read_error_for_id(&mut ws, /*id*/ 10).await?;
    assert!(rejected_continue.error.message.contains("draining"));

    send_request(
        &mut ws,
        "thread/inject_items",
        /*id*/ 12,
        Some(serde_json::json!({
            "threadId": thread_id,
            "items": []
        })),
    )
    .await?;
    let rejected_injection = read_error_for_id(&mut ws, /*id*/ 12).await?;
    assert!(rejected_injection.error.message.contains("draining"));

    send_request(
        &mut ws,
        "thread/shellCommand",
        /*id*/ 11,
        Some(serde_json::json!({
            "threadId": thread_id,
            "command": "echo should be rejected during drain"
        })),
    )
    .await?;
    let rejected_shell = read_error_for_id(&mut ws, /*id*/ 11).await?;
    assert!(rejected_shell.error.message.contains("draining"));

    send_request(
        &mut ws,
        "thread/realtime/appendText",
        /*id*/ 13,
        Some(serde_json::json!({
            "threadId": thread_id,
            "text": "should be rejected during drain"
        })),
    )
    .await?;
    let rejected_realtime = read_error_for_id(&mut ws, /*id*/ 13).await?;
    assert!(rejected_realtime.error.message.contains("draining"));

    send_request(
        &mut ws,
        "mcpServer/tool/call",
        /*id*/ 14,
        Some(serde_json::json!({
            "threadId": thread_id,
            "server": "should-be-rejected",
            "tool": "should-be-rejected"
        })),
    )
    .await?;
    let rejected_mcp = read_error_for_id(&mut ws, /*id*/ 14).await?;
    assert!(rejected_mcp.error.message.contains("draining"));

    send_request(
        &mut ws,
        "mcpServer/event/stream/start",
        /*id*/ 16,
        Some(serde_json::json!({
            "threadId": thread_id,
            "server": "should-be-rejected",
            "subscriptionId": "should-be-rejected",
            "name": "should-be-rejected",
            "arguments": {}
        })),
    )
    .await?;
    let rejected_mcp_stream = read_error_for_id(&mut ws, /*id*/ 16).await?;
    assert!(rejected_mcp_stream.error.message.contains("draining"));

    send_request(
        &mut ws,
        "server/lifecycle/read",
        /*id*/ 6,
        Some(serde_json::json!({})),
    )
    .await?;
    let draining_response = read_response_for_id(&mut ws, /*id*/ 6).await?;
    let draining: ServerLifecycleReadResponse = to_response(draining_response)?;
    assert_eq!(draining.phase, ServerLifecyclePhase::Draining);
    assert_eq!(draining.daemon_instance_id, initial.daemon_instance_id);
    assert_eq!(draining.transition_id, update.transition_id);

    send_request(
        &mut ws,
        "thread/read",
        /*id*/ 7,
        Some(serde_json::json!({
            "threadId": thread_id,
            "includeTurns": false
        })),
    )
    .await?;
    let read_response = read_response_for_id(&mut ws, /*id*/ 7).await?;
    assert_eq!(read_response.result["thread"]["id"], thread_id);

    send_request(
        &mut ws,
        "turn/interrupt",
        /*id*/ 8,
        Some(serde_json::json!({
            "threadId": thread_id,
            "turnId": turn_id
        })),
    )
    .await?;
    let interrupt_response = read_response_for_id(&mut ws, /*id*/ 8).await?;
    assert_eq!(interrupt_response.result, serde_json::json!({}));

    process
        .kill()
        .await
        .context("failed to stop lifecycle websocket app-server process")?;
    Ok(())
}

struct IdleCtrlCFixture {
    _codex_home: TempDir,
    _server: wiremock::MockServer,
    process: Child,
    ws: WsClient,
}

struct GracefulCtrlCFixture {
    _codex_home: TempDir,
    _server: wiremock::MockServer,
    process: Child,
    ws: WsClient,
    thread_id: String,
    turn_id: String,
}

async fn start_idle_ctrl_c_restart_fixture() -> Result<IdleCtrlCFixture> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri(), "never")?;

    let (process, bind_addr) = spawn_websocket_server(codex_home.path()).await?;
    let mut ws = connect_websocket(bind_addr).await?;
    send_experimental_initialize_request(&mut ws, /*id*/ 1, "ws_idle_shutdown").await?;
    let init_response = read_response_for_id(&mut ws, /*id*/ 1).await?;
    assert_eq!(init_response.id, RequestId::Integer(1));

    Ok(IdleCtrlCFixture {
        _codex_home: codex_home,
        _server: server,
        process,
        ws,
    })
}

async fn start_ctrl_c_restart_fixture(turn_delay: Duration) -> Result<GracefulCtrlCFixture> {
    let server = responses::start_mock_server().await;
    let delayed_turn_response = create_final_assistant_message_sse_response("Done")?;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(responses::sse_response(delayed_turn_response).set_delay(turn_delay))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri(), "never")?;

    let (process, bind_addr) = spawn_websocket_server(codex_home.path()).await?;
    let mut ws = connect_websocket(bind_addr).await?;

    send_experimental_initialize_request(&mut ws, /*id*/ 1, "ws_graceful_shutdown").await?;
    let init_response = read_response_for_id(&mut ws, /*id*/ 1).await?;
    assert_eq!(init_response.id, RequestId::Integer(1));

    send_thread_start_request(&mut ws, /*id*/ 2).await?;
    let thread_start_response = read_response_for_id(&mut ws, /*id*/ 2).await?;
    let ThreadStartResponse { thread, .. } = to_response(thread_start_response)?;

    send_turn_start_request(&mut ws, /*id*/ 3, &thread.id).await?;
    let turn_start_response = read_response_for_id(&mut ws, /*id*/ 3).await?;
    assert_eq!(turn_start_response.id, RequestId::Integer(3));
    let turn_id = to_response::<codex_app_server_protocol::TurnStartResponse>(turn_start_response)?
        .turn
        .id;

    wait_for_responses_post(&server, Duration::from_secs(5)).await?;

    Ok(GracefulCtrlCFixture {
        _codex_home: codex_home,
        _server: server,
        process,
        ws,
        thread_id: thread.id,
        turn_id,
    })
}

async fn send_experimental_initialize_request(
    stream: &mut WsClient,
    id: i64,
    client_name: &str,
) -> Result<()> {
    send_request(
        stream,
        "initialize",
        id,
        Some(serde_json::json!({
            "clientInfo": {
                "name": client_name,
                "title": "WebSocket Test Client",
                "version": "0.1.0"
            },
            "capabilities": {
                "experimentalApi": true
            }
        })),
    )
    .await
}

async fn send_thread_start_request(stream: &mut WsClient, id: i64) -> Result<()> {
    send_request(
        stream,
        "thread/start",
        id,
        Some(serde_json::to_value(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })?),
    )
    .await
}

async fn send_turn_start_request(stream: &mut WsClient, id: i64, thread_id: &str) -> Result<()> {
    send_request(
        stream,
        "turn/start",
        id,
        Some(serde_json::to_value(TurnStartParams {
            thread_id: thread_id.to_string(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "Hello".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })?),
    )
    .await
}

async fn wait_for_responses_post(server: &wiremock::MockServer, wait_for: Duration) -> Result<()> {
    let deadline = Instant::now() + wait_for;
    loop {
        let requests = server
            .received_requests()
            .await
            .context("failed to read mock server requests")?;
        if requests
            .iter()
            .any(|request| request.method == "POST" && request.url.path().ends_with("/responses"))
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for /responses request");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
fn send_sigint(process: &Child) -> Result<()> {
    send_signal(process, "-INT")
}

#[cfg(unix)]
fn send_sigterm(process: &Child, _home: &std::path::Path) -> Result<()> {
    send_signal(process, "-TERM")
}

#[cfg(unix)]
fn send_sighup(process: &Child) -> Result<()> {
    send_signal(process, "-HUP")
}

#[cfg(unix)]
fn send_signal(process: &Child, signal: &str) -> Result<()> {
    let pid = process
        .id()
        .context("websocket app-server process has no pid")?;
    let status = StdCommand::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()
        .with_context(|| format!("failed to invoke kill {signal}"))?;
    if !status.success() {
        bail!("kill {signal} exited with {status}");
    }
    Ok(())
}

async fn assert_process_does_not_exit_within(process: &mut Child, window: Duration) -> Result<()> {
    match timeout(window, process.wait()).await {
        Err(_) => Ok(()),
        Ok(Ok(status)) => bail!("process exited too early during graceful drain: {status}"),
        Ok(Err(err)) => Err(err).context("failed waiting for process"),
    }
}

async fn wait_for_process_exit_within(
    process: &mut Child,
    window: Duration,
    timeout_context: &'static str,
) -> Result<std::process::ExitStatus> {
    timeout(window, process.wait())
        .await
        .context(timeout_context)?
        .context("failed waiting for websocket app-server process exit")
}

async fn expect_websocket_disconnect(stream: &mut WsClient) -> Result<()> {
    loop {
        let frame = timeout(DEFAULT_READ_TIMEOUT, stream.next())
            .await
            .context("timed out waiting for websocket disconnect")?;
        match frame {
            None => return Ok(()),
            Some(Ok(WebSocketMessage::Close(_))) => return Ok(()),
            Some(Ok(WebSocketMessage::Ping(payload))) => {
                stream
                    .send(WebSocketMessage::Pong(payload))
                    .await
                    .context("failed to reply to ping while waiting for disconnect")?;
            }
            Some(Ok(WebSocketMessage::Pong(_))) => {}
            Some(Ok(WebSocketMessage::Frame(_))) => {}
            Some(Ok(WebSocketMessage::Text(_))) => {}
            Some(Ok(WebSocketMessage::Binary(_))) => {}
            Some(Err(_)) => return Ok(()),
        }
    }
}
