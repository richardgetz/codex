use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::CollabAgentToolCallStatus;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ThreadActivityPauseResponse;
use codex_app_server_protocol::ThreadActivityReadResponse;
use codex_app_server_protocol::ThreadHandoffNodeState;
use codex_app_server_protocol::ThreadHandoffPrepareResponse;
use codex_app_server_protocol::ThreadHandoffRecoverResponse;
use codex_app_server_protocol::ThreadHandoffState;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadPauseState;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_core::RolloutRecorder;
use codex_features::Feature;
use codex_protocol::models::{ContentItem, ResponseItem};
use codex_rollout::RolloutItem;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::timeout;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A V1 child shutdown is an internal handoff event, not a completed child result. Exercise the
/// public API with a live parent-child tree so recovery must retain both the unfinished child turn
/// ID and the root pause state without injecting a synthetic parent completion marker.
#[tokio::test]
async fn v1_parent_child_handoff_recovery_preserves_unfinished_turn_and_pause() -> Result<()> {
    const PARENT_PROMPT: &str = "spawn a child and keep the tree unfinished";
    const CHILD_PROMPT: &str = "hold this child turn for handoff";
    const SPAWN_CALL_ID: &str = "handoff-tree-spawn";

    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "model": "gpt-5.4",
    }))?;
    let (release_pending_turn_a, pending_turn_a_gate) = oneshot::channel();
    let (release_pending_turn_b, pending_turn_b_gate) = oneshot::channel();
    let (responses_server, _completions) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("parent-response"),
                responses::ev_function_call_with_namespace(
                    SPAWN_CALL_ID,
                    "multi_agent_v1",
                    "spawn_agent",
                    &spawn_args,
                ),
                responses::ev_completed("parent-response"),
            ]),
        }],
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("pending-response-a")]),
            },
            StreamingSseChunk {
                gate: Some(pending_turn_a_gate),
                body: responses::sse(vec![
                    responses::ev_assistant_message("pending-message-a", "turn remains gated"),
                    responses::ev_completed("pending-response-a"),
                ]),
            },
        ],
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("pending-response-b")]),
            },
            StreamingSseChunk {
                gate: Some(pending_turn_b_gate),
                body: responses::sse(vec![
                    responses::ev_assistant_message("pending-message-b", "turn remains gated"),
                    responses::ev_completed("pending-response-b"),
                ]),
            },
        ],
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("recovered-parent-response"),
                responses::ev_assistant_message("recovered-parent-message", "parent recovered"),
                responses::ev_completed("recovered-parent-response"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("recovered-child-response"),
                responses::ev_assistant_message("recovered-child-message", "child recovered"),
                responses::ev_completed("recovered-child-response"),
            ]),
        }],
    ])
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(responses_server.uri())
        .disable_feature(Feature::MultiAgentV2)
        .enable_feature(Feature::Collab)
        .write(codex_home.path())?;
    let mut old_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;

    let ThreadStartResponse { thread: parent, .. } = old_server
        .start_thread(ThreadStartParams {
            experimental_raw_events: true,
            ..Default::default()
        })
        .await?;
    let parent_turn_request = old_server
        .send_turn_start_request(TurnStartParams {
            thread_id: parent.id.clone(),
            input: vec![UserInput::Text {
                text: PARENT_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn: parent_turn } = timeout(
        REQUEST_TIMEOUT,
        old_server.read_response(parent_turn_request),
    )
    .await??;

    let child_id = timeout(REQUEST_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                old_server.read_notification("item/completed").await?;
            if let ThreadItem::CollabAgentToolCall {
                id,
                status: CollabAgentToolCallStatus::Completed,
                receiver_thread_ids,
                ..
            } = completed.item
                && id == SPAWN_CALL_ID
                && let Some(child_thread_id) = receiver_thread_ids.into_iter().next()
            {
                return Ok::<String, anyhow::Error>(child_thread_id);
            }
        }
    })
    .await??;
    // The child turn-start notification is emitted when the scheduler admits the child, before
    // its Responses API request. Use this public lifecycle event as the barrier; the two queued
    // streams remain gated so any later model request stays unfinished regardless of queue order.
    let child_turn = timeout(REQUEST_TIMEOUT, async {
        loop {
            let started: TurnStartedNotification =
                old_server.read_notification("turn/started").await?;
            if started.thread_id == child_id {
                return Ok::<_, anyhow::Error>(started.turn);
            }
        }
    })
    .await??;
    assert_ne!(parent_turn.id, child_turn.id);

    let interrupt_request = old_server
        .send_turn_interrupt_request(TurnInterruptParams {
            thread_id: parent.id.clone(),
            turn_id: parent_turn.id.clone(),
        })
        .await?;
    let _: TurnInterruptResponse =
        timeout(REQUEST_TIMEOUT, old_server.read_response(interrupt_request)).await??;
    let interrupted_parent = timeout(REQUEST_TIMEOUT, async {
        loop {
            let completed: TurnCompletedNotification =
                old_server.read_notification("turn/completed").await?;
            if completed.thread_id == parent.id && completed.turn.id == parent_turn.id {
                return Ok::<TurnCompletedNotification, anyhow::Error>(completed);
            }
        }
    })
    .await??;
    assert_eq!(interrupted_parent.turn.status, TurnStatus::Interrupted);

    let pause_request = old_server
        .send_raw_request(
            "thread/activity/pause",
            Some(json!({"threadId": parent.id.clone()})),
        )
        .await?;
    let _: ThreadActivityPauseResponse =
        timeout(REQUEST_TIMEOUT, old_server.read_response(pause_request)).await??;
    let activity_request = old_server
        .send_raw_request(
            "thread/activity/read",
            Some(json!({"threadId": parent.id.clone()})),
        )
        .await?;
    let activity: ThreadActivityReadResponse =
        timeout(REQUEST_TIMEOUT, old_server.read_response(activity_request)).await??;
    assert!(activity.activities.len() >= 2);
    assert!(
        activity
            .activities
            .iter()
            .all(|entry| entry.pause_state == ThreadPauseState::Paused)
    );

    let prepare_request = old_server
        .send_raw_request("thread/handoff/prepare", None)
        .await?;
    let prepared: ThreadHandoffPrepareResponse =
        timeout(REQUEST_TIMEOUT, old_server.read_response(prepare_request)).await??;
    assert_eq!(prepared.receipt.state, ThreadHandoffState::Suspended);
    let prepared_parent = prepared
        .receipt
        .nodes
        .iter()
        .find(|node| node.thread_id == parent.id)
        .expect("parent node should be recorded");
    assert!(prepared_parent.was_paused);
    let prepared_child = prepared
        .receipt
        .nodes
        .iter()
        .find(|node| node.thread_id == child_id)
        .expect("child node should be recorded");
    assert_eq!(
        prepared_child.parent_thread_id.as_deref(),
        Some(parent.id.as_str())
    );
    assert!(prepared_child.was_running);
    assert!(prepared_child.was_paused);
    assert_eq!(prepared_child.state, ThreadHandoffNodeState::Suspended);
    assert_eq!(
        prepared_child.turn_id.as_deref(),
        Some(child_turn.id.as_str())
    );

    // `prepare` waits for the V1 completion watcher. Inspect the durable raw rollout after that
    // barrier so an asynchronously delivered synthetic parent marker cannot escape a projected
    // history or notification-buffer assertion.
    let parent_rollout_path = parent.path.as_ref().expect("parent rollout path");
    let (rollout_items, _, parse_errors) =
        RolloutRecorder::load_rollout_items(parent_rollout_path).await?;
    assert_eq!(parse_errors, 0, "parent rollout should parse cleanly");
    assert!(
        rollout_items
            .iter()
            .any(|item| matches!(item, RolloutItem::ResponseItem(_)))
    );
    let synthetic_parent_completion = rollout_items.iter().any(|item| {
        let RolloutItem::ResponseItem(envelope) = item else {
            return false;
        };
        let ResponseItem::Message { role, content, .. } = &envelope.item else {
            return false;
        };
        role == "user"
            && content.iter().any(|item| match item {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    text.contains("<subagent_notification>")
                }
                ContentItem::InputImage { .. }
                | ContentItem::InputAudio { .. }
                | ContentItem::EncryptedContent { .. } => false,
            })
    });
    assert!(!synthetic_parent_completion);

    drop((release_pending_turn_a, release_pending_turn_b));
    timeout(REQUEST_TIMEOUT, old_server.shutdown_gracefully()).await??;

    let mut replacement = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;
    let recover_request = replacement
        .send_raw_request(
            "thread/handoff/recover",
            Some(json!({"handoffId": prepared.receipt.handoff_id})),
        )
        .await?;
    let recovered: ThreadHandoffRecoverResponse =
        timeout(REQUEST_TIMEOUT, replacement.read_response(recover_request)).await??;
    assert_eq!(recovered.receipt.state, ThreadHandoffState::Completed);
    let recovered_parent = recovered
        .receipt
        .nodes
        .iter()
        .find(|node| node.thread_id == parent.id)
        .expect("recovered parent node should be recorded");
    assert!(recovered_parent.was_paused);
    let recovered_child = recovered
        .receipt
        .nodes
        .iter()
        .find(|node| node.thread_id == child_id)
        .expect("recovered child node should be recorded");
    assert_eq!(
        recovered_child.parent_thread_id.as_deref(),
        Some(parent.id.as_str())
    );
    assert_eq!(
        recovered_child.turn_id.as_deref(),
        Some(child_turn.id.as_str())
    );
    assert!(recovered_child.was_paused);
    assert!(matches!(
        recovered_child.state,
        ThreadHandoffNodeState::Paused | ThreadHandoffNodeState::Restored
    ));

    let recovered_activity_request = replacement
        .send_raw_request(
            "thread/activity/read",
            Some(json!({"threadId": parent.id.clone()})),
        )
        .await?;
    let recovered_activity: ThreadActivityReadResponse = timeout(
        REQUEST_TIMEOUT,
        replacement.read_response(recovered_activity_request),
    )
    .await??;
    assert!(recovered_activity.activities.len() >= 2);
    assert!(
        recovered_activity
            .activities
            .iter()
            .all(|entry| entry.pause_state == ThreadPauseState::Paused)
    );

    replacement.shutdown_gracefully().await?;
    responses_server.shutdown().await;
    Ok(())
}
