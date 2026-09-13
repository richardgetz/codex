use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::ThreadActivityPauseResponse;
use codex_app_server_protocol::ThreadActivityReadResponse;
use codex_app_server_protocol::ThreadHandoffNodeState;
use codex_app_server_protocol::ThreadHandoffPrepareResponse;
use codex_app_server_protocol::ThreadHandoffRecoverResponse;
use codex_app_server_protocol::ThreadHandoffState;
use codex_app_server_protocol::ThreadPauseState;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::timeout;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::test]
async fn handoff_prepare_and_cold_recover_preserves_turn_and_pause_state() -> Result<()> {
    let (release_running_turn, running_turn_gate) = oneshot::channel();
    let (responses_server, _completions) = start_streaming_sse_server(vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("handoff-response")]),
            },
            StreamingSseChunk {
                gate: Some(running_turn_gate),
                body: responses::sse(vec![
                    responses::ev_assistant_message("handoff-message", "finished"),
                    responses::ev_completed("handoff-response"),
                ]),
            },
        ],
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("recovered-response"),
                responses::ev_assistant_message("recovered-message", "recovered"),
                responses::ev_completed("recovered-response"),
            ]),
        }],
    ])
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(responses_server.uri()).write(codex_home.path())?;
    let mut old_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;

    let paused_idle = old_server
        .start_thread(ThreadStartParams::default())
        .await?
        .thread;
    let pause_request = old_server
        .send_raw_request(
            "thread/activity/pause",
            Some(json!({"threadId": paused_idle.id})),
        )
        .await?;
    let _: ThreadActivityPauseResponse =
        timeout(REQUEST_TIMEOUT, old_server.read_response(pause_request)).await??;
    wait_for_pause_state(&mut old_server, &paused_idle.id).await?;

    let running = old_server
        .start_thread(ThreadStartParams::default())
        .await?
        .thread;
    let turn_request = old_server
        .send_turn_start_request(TurnStartParams {
            thread_id: running.id.clone(),
            input: vec![UserInput::Text {
                text: "preserve this turn exactly".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } =
        timeout(REQUEST_TIMEOUT, old_server.read_response(turn_request)).await??;
    timeout(
        REQUEST_TIMEOUT,
        old_server.read_stream_until_notification_message("turn/started"),
    )
    .await??;
    responses_server.wait_for_request_count(1).await;
    let prepare_request = old_server
        .send_raw_request("thread/handoff/prepare", None)
        .await?;
    let prepared: ThreadHandoffPrepareResponse =
        timeout(REQUEST_TIMEOUT, old_server.read_response(prepare_request)).await??;
    assert_eq!(prepared.receipt.state, ThreadHandoffState::Suspended);
    drop(release_running_turn);

    // A successful prepare retains the manager/root fences in the coordinator's active map. The
    // archive request now has a deterministic post-seal rejection before any store mutation.
    let fenced_archive_request = old_server
        .send_raw_request("thread/archive", Some(json!({"threadId": paused_idle.id})))
        .await?;
    let fenced_archive_error: JSONRPCError = timeout(
        REQUEST_TIMEOUT,
        old_server.read_response(fenced_archive_request),
    )
    .await??;
    assert_eq!(fenced_archive_error.error.code, -32600);
    assert!(fenced_archive_error.error.message.contains("handoff"));

    let prepared_running = prepared
        .receipt
        .nodes
        .iter()
        .find(|node| node.thread_id == running.id)
        .expect("running node should be recorded");
    assert_eq!(prepared_running.state, ThreadHandoffNodeState::Suspended);
    assert_eq!(prepared_running.turn_id.as_deref(), Some(turn.id.as_str()));

    let prepared_paused = prepared
        .receipt
        .nodes
        .iter()
        .find(|node| node.thread_id == paused_idle.id)
        .expect("paused idle node should be recorded");
    assert_eq!(prepared_paused.state, ThreadHandoffNodeState::NotActive);
    assert_eq!(prepared_paused.turn_id, None);

    timeout(REQUEST_TIMEOUT, old_server.shutdown_gracefully()).await??;

    let mut replacement = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;

    // Replacement startup remains fenced until every receipt node has been loaded and recovered.
    let blocked_start = replacement
        .send_raw_request("thread/start", Some(json!({})))
        .await?;
    let blocked_start_error: JSONRPCError = timeout(
        REQUEST_TIMEOUT,
        replacement.read_response(blocked_start),
    )
    .await??;
    assert_eq!(blocked_start_error.error.code, -32600);
    assert!(blocked_start_error
        .error
        .message
        .contains("recovery is pending"));

    let recover_request = replacement
        .send_raw_request(
            "thread/handoff/recover",
            Some(json!({"handoffId": prepared.receipt.handoff_id})),
        )
        .await?;
    let recovered: ThreadHandoffRecoverResponse =
        timeout(REQUEST_TIMEOUT, replacement.read_response(recover_request)).await??;
    assert_eq!(recovered.receipt.state, ThreadHandoffState::Completed);

    let recovered_running = recovered
        .receipt
        .nodes
        .iter()
        .find(|node| node.thread_id == running.id)
        .expect("recovered running node should be recorded");
    assert_eq!(recovered_running.state, ThreadHandoffNodeState::Restored);
    assert_eq!(recovered_running.turn_id.as_deref(), Some(turn.id.as_str()));

    let recovered_paused = recovered
        .receipt
        .nodes
        .iter()
        .find(|node| node.thread_id == paused_idle.id)
        .expect("recovered paused node should be recorded");
    assert_eq!(recovered_paused.state, ThreadHandoffNodeState::Paused);

    timeout(
        REQUEST_TIMEOUT,
        replacement.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    wait_for_pause_state(&mut replacement, &paused_idle.id).await?;
    assert_eq!(responses_server.requests().await.len(), 2);

    let _: ThreadStartResponse = replacement
        .start_thread(ThreadStartParams::default())
        .await?;
    replacement.shutdown_gracefully().await?;
    responses_server.shutdown().await;
    Ok(())
}

async fn wait_for_pause_state(app: &mut TestAppServer, thread_id: &str) -> Result<()> {
    timeout(REQUEST_TIMEOUT, async {
        loop {
            let request = app
                .send_raw_request("thread/activity/read", Some(json!({"threadId": thread_id})))
                .await?;
            let activity: ThreadActivityReadResponse = app.read_response(request).await?;
            if activity.activities.iter().any(|entry| {
                entry.thread_id == thread_id && entry.pause_state == ThreadPauseState::Paused
            }) {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}
