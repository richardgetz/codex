use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::write_models_cache;
use codex_app_server_protocol::CollabAgentToolCallStatus;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ThreadActivityContinueResponse;
use codex_app_server_protocol::ThreadActivityPauseResponse;
use codex_app_server_protocol::ThreadActivityReadResponse;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadPauseState;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use codex_state::StateRuntime;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use serde_json::json;
use std::collections::HashSet;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::timeout;

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[tokio::test]
async fn thread_activity_pause_continue_is_root_scoped_and_wake_only() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;

    let pause_request = app
        .send_raw_request(
            "thread/activity/pause",
            Some(serde_json::json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let pause_response: ThreadActivityPauseResponse =
        timeout(REQUEST_TIMEOUT, app.read_response(pause_request)).await??;
    assert_eq!(pause_response, ThreadActivityPauseResponse {});

    let read_request = app
        .send_raw_request(
            "thread/activity/read",
            Some(serde_json::json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let paused: ThreadActivityReadResponse =
        timeout(REQUEST_TIMEOUT, app.read_response(read_request)).await??;
    assert!(!paused.activities.is_empty());
    assert!(
        paused
            .activities
            .iter()
            .all(|activity| activity.pause_state == ThreadPauseState::Paused)
    );

    let continue_request = app
        .send_raw_request(
            "thread/activity/continue",
            Some(serde_json::json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let continue_response: ThreadActivityContinueResponse =
        timeout(REQUEST_TIMEOUT, app.read_response(continue_request)).await??;
    assert_eq!(continue_response, ThreadActivityContinueResponse {});

    let read_request = app
        .send_raw_request(
            "thread/activity/read",
            Some(serde_json::json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let resumed: ThreadActivityReadResponse =
        timeout(REQUEST_TIMEOUT, app.read_response(read_request)).await??;
    assert!(
        resumed
            .activities
            .iter()
            .all(|activity| activity.pause_state == ThreadPauseState::Running)
    );
    Ok(())
}

#[tokio::test]
async fn thread_activity_continue_without_marker_keeps_healthy_tree_running() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;

    let continue_request = app
        .send_raw_request(
            "thread/activity/continue",
            Some(json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let _: ThreadActivityContinueResponse =
        timeout(REQUEST_TIMEOUT, app.read_response(continue_request)).await??;

    let read_request = app
        .send_raw_request(
            "thread/activity/read",
            Some(json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let activity: ThreadActivityReadResponse =
        timeout(REQUEST_TIMEOUT, app.read_response(read_request)).await??;
    assert!(
        activity
            .activities
            .iter()
            .all(|entry| entry.pause_state == ThreadPauseState::Running)
    );
    app.shutdown_gracefully().await?;
    Ok(())
}

#[tokio::test]
async fn thread_activity_pause_survives_restart_with_idle_open_descendant() -> Result<()> {
    let responses_server = responses::start_mock_server().await;
    let _seed_response = responses::mount_sse_once(
        &responses_server,
        responses::sse(vec![
            responses::ev_response_created("pause-restart-seed"),
            responses::ev_assistant_message("pause-restart-seed-message", "seed history"),
            responses::ev_completed("pause-restart-seed"),
        ]),
    )
    .await;
    let _idle_child_response = responses::mount_sse_once(
        &responses_server,
        responses::sse(vec![
            responses::ev_response_created("pause-restart-idle-child"),
            responses::ev_assistant_message(
                "pause-restart-idle-child-message",
                "idle child history",
            ),
            responses::ev_completed("pause-restart-idle-child"),
        ]),
    )
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&responses_server.uri()).write(codex_home.path())?;
    let mut first = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;
    let thread = first
        .start_thread(ThreadStartParams::default())
        .await?
        .thread;
    let seed_turn_request = first
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "seed history".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse =
        timeout(REQUEST_TIMEOUT, first.read_response(seed_turn_request)).await??;
    timeout(
        REQUEST_TIMEOUT,
        first.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let idle_child = first
        .start_thread(ThreadStartParams::default())
        .await?
        .thread;
    let idle_child_turn_request = first
        .send_turn_start_request(TurnStartParams {
            thread_id: idle_child.id.clone(),
            input: vec![UserInput::Text {
                text: "finish before the parent pauses".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = timeout(
        REQUEST_TIMEOUT,
        first.read_response(idle_child_turn_request),
    )
    .await??;
    timeout(
        REQUEST_TIMEOUT,
        first.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    // Older Team sessions can retain open graph edges after a worker has finished. Keep the
    // terminal thread metadata untouched so cold recovery proves it does not try to reload idle
    // historical descendants before continuing the root.
    state_db
        .upsert_thread_spawn_edge(
            ThreadId::from_string(&thread.id)?,
            ThreadId::from_string(&idle_child.id)?,
            DirectionalThreadSpawnEdgeStatus::Open,
        )
        .await?;

    let pause_request = first
        .send_raw_request(
            "thread/activity/pause",
            Some(serde_json::json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let _: ThreadActivityPauseResponse =
        timeout(REQUEST_TIMEOUT, first.read_response(pause_request)).await??;
    timeout(REQUEST_TIMEOUT, first.shutdown_gracefully()).await??;
    drop(first);

    let mut resumed = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;
    let resume_request = resumed
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse =
        timeout(REQUEST_TIMEOUT, resumed.read_response(resume_request)).await??;

    let read_request = resumed
        .send_raw_request(
            "thread/activity/read",
            Some(serde_json::json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let paused: ThreadActivityReadResponse =
        timeout(REQUEST_TIMEOUT, resumed.read_response(read_request)).await??;
    assert!(!paused.activities.is_empty());
    assert!(
        paused
            .activities
            .iter()
            .all(|activity| activity.pause_state == ThreadPauseState::Paused)
    );

    for _ in 0..2 {
        let continue_request = resumed
            .send_raw_request(
                "thread/activity/continue",
                Some(serde_json::json!({"threadId": thread.id.clone()})),
            )
            .await?;
        let _: ThreadActivityContinueResponse =
            timeout(REQUEST_TIMEOUT, resumed.read_response(continue_request)).await??;

        let read_request = resumed
            .send_raw_request(
                "thread/activity/read",
                Some(serde_json::json!({"threadId": thread.id.clone()})),
            )
            .await?;
        let running: ThreadActivityReadResponse =
            timeout(REQUEST_TIMEOUT, resumed.read_response(read_request)).await??;
        assert!(
            running
                .activities
                .iter()
                .all(|activity| activity.pause_state == ThreadPauseState::Running)
        );

        let pause_request = resumed
            .send_raw_request(
                "thread/activity/pause",
                Some(serde_json::json!({"threadId": thread.id.clone()})),
            )
            .await?;
        let _: ThreadActivityPauseResponse =
            timeout(REQUEST_TIMEOUT, resumed.read_response(pause_request)).await??;
    }

    timeout(REQUEST_TIMEOUT, resumed.shutdown_gracefully()).await??;
    Ok(())
}

#[tokio::test]
async fn thread_activity_cold_resume_reconciles_direct_and_nested_workers() -> Result<()> {
    run_cold_resume_case(
        /*pause_before_exit*/ true,
        ThreadHistoryMode::Legacy,
        false,
        /*multi_agent_v2*/ false,
        /*include_historical_workers*/ true,
    )
    .await
}

#[tokio::test]
async fn thread_activity_cold_resume_without_marker_reconciles_direct_and_nested_workers()
-> Result<()> {
    run_cold_resume_case(
        /*pause_before_exit*/ false,
        ThreadHistoryMode::Paginated,
        true,
        /*multi_agent_v2*/ false,
        /*include_historical_workers*/ false,
    )
    .await
}

#[tokio::test]
async fn thread_activity_cold_resume_reconciles_v2_direct_and_nested_workers() -> Result<()> {
    run_cold_resume_case(
        /*pause_before_exit*/ true,
        ThreadHistoryMode::Paginated,
        false,
        /*multi_agent_v2*/ true,
        /*include_historical_workers*/ true,
    )
    .await
}

#[tokio::test]
async fn thread_activity_cold_resume_without_marker_reconciles_root_only_model_once() -> Result<()>
{
    let (release_pending_turn, pending_turn_gate) = oneshot::channel();
    let (responses_server, _completions) = start_streaming_sse_server(vec![
        gated_response(
            "root-only-pending",
            "root-only work remains pending",
            pending_turn_gate,
        ),
        completed_response("root-only-recovered"),
    ])
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(responses_server.uri()).write(codex_home.path())?;
    let mut old_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;
    let thread = old_server
        .start_thread(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?
        .thread;
    let turn_request = old_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "keep this root-only turn unfinished".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse =
        timeout(REQUEST_TIMEOUT, old_server.read_response(turn_request)).await??;
    responses_server.wait_for_request_count(1).await;

    // Teardown while the root model stream is still unfinished. No pause marker is written.
    drop(old_server);
    drop(release_pending_turn);

    let mut resumed = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;
    let resume_request = resumed
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse =
        timeout(REQUEST_TIMEOUT, resumed.read_response(resume_request)).await??;
    assert_eq!(responses_server.requests().await.len(), 1);

    let continue_request = resumed
        .send_raw_request(
            "thread/activity/continue",
            Some(json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let _: ThreadActivityContinueResponse =
        timeout(REQUEST_TIMEOUT, resumed.read_response(continue_request)).await??;
    responses_server.wait_for_request_count(2).await;
    assert_eq!(responses_server.requests().await.len(), 2);

    let repeat_continue_request = resumed
        .send_raw_request(
            "thread/activity/continue",
            Some(json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let _: ThreadActivityContinueResponse = timeout(
        REQUEST_TIMEOUT,
        resumed.read_response(repeat_continue_request),
    )
    .await??;
    assert_eq!(responses_server.requests().await.len(), 2);

    resumed.shutdown_gracefully().await?;
    responses_server.shutdown().await;
    Ok(())
}

async fn run_cold_resume_case(
    pause_before_exit: bool,
    history_mode: ThreadHistoryMode,
    continue_before_exit: bool,
    multi_agent_v2: bool,
    include_historical_workers: bool,
) -> Result<()> {
    const PARENT_PROMPT: &str = "spawn a direct worker and keep the team unfinished";
    const CHILD_PROMPT: &str = "spawn a nested worker and keep the team unfinished";
    const GRANDCHILD_PROMPT: &str = "hold this nested worker for recovery";
    const PARENT_SPAWN_CALL_ID: &str = "cold-team-parent-spawn";
    const CHILD_SPAWN_CALL_ID: &str = "cold-team-child-spawn";
    let parent_spawn_args = if multi_agent_v2 {
        serde_json::to_string(&json!({
            "message": CHILD_PROMPT,
            "task_name": "direct_worker",
            "fork_turns": "none",
        }))?
    } else {
        serde_json::to_string(&json!({
            "message": CHILD_PROMPT,
            "model": "gpt-5.4",
        }))?
    };
    let child_spawn_args = if multi_agent_v2 {
        serde_json::to_string(&json!({
            "message": GRANDCHILD_PROMPT,
            "task_name": "nested_worker",
            "fork_turns": "none",
        }))?
    } else {
        serde_json::to_string(&json!({
            "message": GRANDCHILD_PROMPT,
            "model": "gpt-5.4",
        }))?
    };
    let spawn_namespace = if multi_agent_v2 {
        "collaboration"
    } else {
        "multi_agent_v1"
    };
    let (release_a, gate_a) = oneshot::channel();
    let (release_b, gate_b) = oneshot::channel();
    let (release_c, gate_c) = oneshot::channel();
    let (historical_release, historical_gate) = if include_historical_workers {
        let (release, gate) = oneshot::channel();
        (Some(release), Some(gate))
    } else {
        (None, None)
    };
    let mut response_sequence = Vec::new();
    if include_historical_workers {
        response_sequence.push(completed_response("historical-completed"));
        response_sequence.push(gated_response(
            "historical-interrupted",
            "historical work is interrupted before the pause",
            historical_gate.expect("historical gate when enabled"),
        ));
    }
    response_sequence.extend([
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("cold-parent-spawn"),
                responses::ev_function_call_with_namespace(
                    PARENT_SPAWN_CALL_ID,
                    spawn_namespace,
                    "spawn_agent",
                    &parent_spawn_args,
                ),
                responses::ev_completed("cold-parent-spawn"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("cold-child-spawn"),
                responses::ev_function_call_with_namespace(
                    CHILD_SPAWN_CALL_ID,
                    spawn_namespace,
                    "spawn_agent",
                    &child_spawn_args,
                ),
                responses::ev_completed("cold-child-spawn"),
            ]),
        }],
        gated_response(
            "cold-pending-a",
            "direct or nested work remains pending",
            gate_a,
        ),
        gated_response(
            "cold-pending-b",
            "direct or nested work remains pending",
            gate_b,
        ),
        gated_response(
            "cold-pending-c",
            "direct or nested work remains pending",
            gate_c,
        ),
        completed_response("cold-recovered-a"),
        completed_response("cold-recovered-b"),
        completed_response("cold-recovered-c"),
    ]);
    let (responses_server, _completions) = start_streaming_sse_server(response_sequence).await;

    let codex_home = TempDir::new()?;
    let mock_config = MockResponsesConfig::new(responses_server.uri());
    let mock_config = mock_config.enable_feature(Feature::Collab);
    let mock_config = if multi_agent_v2 {
        mock_config.enable_feature(Feature::MultiAgentV2)
    } else {
        mock_config.disable_feature(Feature::MultiAgentV2)
    };
    mock_config.write(codex_home.path())?;
    if multi_agent_v2 {
        write_models_cache(codex_home.path())?;
    }
    let mut old_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;
    let ThreadStartResponse { thread: parent, .. } = old_server
        .start_thread(ThreadStartParams {
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?;
    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    let mut historical_thread_ids = Vec::new();
    if include_historical_workers {
        // These workers deliberately remain open in the persisted graph after their turns are
        // complete or interrupted. They model historical cleanup rows that the old recovery
        // scan would rediscover, while the root's direct and nested workers below remain active
        // at the pause boundary and are the only turns allowed to resume.
        let historical_completed = old_server
            .start_thread(ThreadStartParams::default())
            .await?
            .thread;
        let historical_completed_turn_request = old_server
            .send_turn_start_request(TurnStartParams {
                thread_id: historical_completed.id.clone(),
                input: vec![UserInput::Text {
                    text: "complete this historical worker before pause".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })
            .await?;
        let _: TurnStartResponse = timeout(
            REQUEST_TIMEOUT,
            old_server.read_response(historical_completed_turn_request),
        )
        .await??;
        loop {
            let completed: TurnCompletedNotification =
                old_server.read_notification("turn/completed").await?;
            if completed.thread_id == historical_completed.id {
                assert_eq!(
                    completed.turn.status,
                    codex_app_server_protocol::TurnStatus::Completed
                );
                break;
            }
        }

        let historical_interrupted = old_server
            .start_thread(ThreadStartParams::default())
            .await?
            .thread;
        let historical_interrupted_turn_request = old_server
            .send_turn_start_request(TurnStartParams {
                thread_id: historical_interrupted.id.clone(),
                input: vec![UserInput::Text {
                    text: "interrupt this historical worker before pause".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })
            .await?;
        let TurnStartResponse {
            turn: historical_interrupted_turn,
        } = timeout(
            REQUEST_TIMEOUT,
            old_server.read_response(historical_interrupted_turn_request),
        )
        .await??;
        responses_server.wait_for_request_count(2).await;
        let interrupt_request = old_server
            .send_turn_interrupt_request(TurnInterruptParams {
                thread_id: historical_interrupted.id.clone(),
                turn_id: historical_interrupted_turn.id.clone(),
            })
            .await?;
        let _: TurnInterruptResponse =
            timeout(REQUEST_TIMEOUT, old_server.read_response(interrupt_request)).await??;
        loop {
            let completed: TurnCompletedNotification =
                old_server.read_notification("turn/completed").await?;
            if completed.thread_id == historical_interrupted.id
                && completed.turn.id == historical_interrupted_turn.id
            {
                assert_eq!(
                    completed.turn.status,
                    codex_app_server_protocol::TurnStatus::Interrupted
                );
                break;
            }
        }
        drop(historical_release);

        historical_thread_ids.extend([
            historical_completed.id.clone(),
            historical_interrupted.id.clone(),
        ]);
    }
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
    let TurnStartResponse { .. } = timeout(
        REQUEST_TIMEOUT,
        old_server.read_response(parent_turn_request),
    )
    .await??;

    let (child_id, grandchild_id) = timeout(REQUEST_TIMEOUT, async {
        let mut child_id = None;
        let mut grandchild_id = None;
        while grandchild_id.is_none() {
            let completed: ItemCompletedNotification =
                old_server.read_notification("item/completed").await?;
            match completed.item {
                ThreadItem::CollabAgentToolCall {
                    id,
                    status: CollabAgentToolCallStatus::Completed,
                    receiver_thread_ids,
                    ..
                } if !multi_agent_v2 => {
                    if id == PARENT_SPAWN_CALL_ID {
                        child_id = receiver_thread_ids.into_iter().next();
                    } else if id == CHILD_SPAWN_CALL_ID {
                        grandchild_id = receiver_thread_ids.into_iter().next();
                    }
                }
                ThreadItem::SubAgentActivity {
                    id,
                    kind: codex_app_server_protocol::SubAgentActivityKind::Started,
                    agent_thread_id,
                    ..
                } if multi_agent_v2 => {
                    if id == PARENT_SPAWN_CALL_ID {
                        child_id = Some(agent_thread_id.to_string());
                    } else if id == CHILD_SPAWN_CALL_ID {
                        grandchild_id = Some(agent_thread_id.to_string());
                    }
                }
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>((
            child_id.expect("direct worker id"),
            grandchild_id.expect("nested worker id"),
        ))
    })
    .await??;

    let expected_threads =
        HashSet::from([parent.id.clone(), child_id.clone(), grandchild_id.clone()]);
    let initial_request_count = historical_thread_ids.len() + 5;
    let terminal_thread_id = ThreadId::new();
    state_db
        .upsert_thread_spawn_edge(
            ThreadId::from_string(&parent.id)?,
            terminal_thread_id,
            DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await?;
    let mut started_threads = HashSet::new();
    while started_threads.len() < expected_threads.len() {
        let started: TurnStartedNotification = old_server.read_notification("turn/started").await?;
        if expected_threads.contains(&started.thread_id) {
            started_threads.insert(started.thread_id);
        }
    }
    responses_server
        .wait_for_request_count(initial_request_count)
        .await;

    // Add the completed and interrupted workers to the persisted Team graph only after the
    // active direct/nested workers have spawned. That keeps the synthetic roots from affecting
    // spawn-path selection while still presenting the pause boundary with idle historical edges.
    for historical_thread_id in &historical_thread_ids {
        let thread_id = ThreadId::from_string(historical_thread_id)?;
        let mut metadata = state_db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("historical thread metadata missing"))?;
        metadata.source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::from_string(&parent.id)?,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        })
        .to_string();
        state_db.upsert_thread(&metadata).await?;
        state_db
            .upsert_thread_spawn_edge(
                ThreadId::from_string(&parent.id)?,
                thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await?;
    }

    if continue_before_exit {
        let continue_request = old_server
            .send_raw_request(
                "thread/activity/continue",
                Some(json!({"threadId": parent.id.clone()})),
            )
            .await?;
        let _: ThreadActivityContinueResponse =
            timeout(REQUEST_TIMEOUT, old_server.read_response(continue_request)).await??;
        assert_eq!(
            responses_server.requests().await.len(),
            initial_request_count
        );
        let read_request = old_server
            .send_raw_request(
                "thread/activity/read",
                Some(json!({"threadId": parent.id.clone()})),
            )
            .await?;
        let active: ThreadActivityReadResponse =
            timeout(REQUEST_TIMEOUT, old_server.read_response(read_request)).await??;
        assert!(
            active
                .activities
                .iter()
                .all(|entry| entry.pause_state == ThreadPauseState::Running)
        );
    }

    if pause_before_exit {
        let pause_request = old_server
            .send_raw_request(
                "thread/activity/pause",
                Some(json!({"threadId": parent.id.clone()})),
            )
            .await?;
        let _: ThreadActivityPauseResponse =
            timeout(REQUEST_TIMEOUT, old_server.read_response(pause_request)).await??;
        let captured = state_db
            .get_thread_activity_pause_snapshot(ThreadId::from_string(&parent.id)?)
            .await?
            .expect("pause snapshot");
        let captured_thread_ids = captured
            .into_iter()
            .map(|snapshot| snapshot.thread_id.to_string())
            .collect::<HashSet<_>>();
        assert_eq!(captured_thread_ids, expected_threads);
        let read_request = old_server
            .send_raw_request(
                "thread/activity/read",
                Some(json!({"threadId": parent.id.clone()})),
            )
            .await?;
        let paused: ThreadActivityReadResponse =
            timeout(REQUEST_TIMEOUT, old_server.read_response(read_request)).await??;
        assert!(paused.activities.len() >= 3);
        assert!(paused.activities.iter().all(|entry| {
            expected_threads.contains(&entry.thread_id.to_string())
                && matches!(
                    entry.pause_state,
                    ThreadPauseState::Pausing | ThreadPauseState::Paused
                )
        }));
        assert!(
            paused
                .activities
                .iter()
                .any(|entry| entry.pause_state == ThreadPauseState::Pausing)
        );
    }

    // Teardown while the three model streams remain unfinished. With an explicit pause, the
    // durable marker and graph are already committed; without one, the graph is the recovery
    // authority and `/continue` must establish the marker before admitting retained work.
    drop(old_server);
    drop((release_a, release_b, release_c));

    let mut resumed = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;
    let resume_request = resumed
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: parent.id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse =
        timeout(REQUEST_TIMEOUT, resumed.read_response(resume_request)).await??;
    assert_eq!(
        responses_server.requests().await.len(),
        initial_request_count
    );
    if !pause_before_exit {
        let read_request = resumed
            .send_raw_request(
                "thread/activity/read",
                Some(json!({"threadId": parent.id.clone()})),
            )
            .await?;
        let activity: ThreadActivityReadResponse =
            timeout(REQUEST_TIMEOUT, resumed.read_response(read_request)).await??;
        assert!(
            activity
                .activities
                .iter()
                .all(|entry| entry.pause_state != ThreadPauseState::Paused)
        );
    }

    let continue_request = resumed
        .send_raw_request(
            "thread/activity/continue",
            Some(json!({"threadId": parent.id.clone()})),
        )
        .await?;
    let _: ThreadActivityContinueResponse =
        timeout(REQUEST_TIMEOUT, resumed.read_response(continue_request)).await??;
    responses_server
        .wait_for_request_count(initial_request_count + 3)
        .await;
    let recovered_requests = responses_server.requests().await;
    for thread_id in &expected_threads {
        let count = recovered_requests[initial_request_count..]
            .iter()
            .filter(|request| request_thread_id(request).as_deref() == Some(thread_id.as_str()))
            .count();
        assert_eq!(count, 1, "worker {thread_id} should resume exactly once");
    }
    let terminal_thread_id = terminal_thread_id.to_string();
    assert!(
        recovered_requests[initial_request_count..]
            .iter()
            .all(|request| request_thread_id(request).as_deref()
                != Some(terminal_thread_id.as_str()))
    );
    for thread_id in &historical_thread_ids {
        assert!(
            recovered_requests[initial_request_count..]
                .iter()
                .all(|request| request_thread_id(request).as_deref() != Some(thread_id.as_str())),
            "historical worker {thread_id} must not resume"
        );
    }

    let request_count = recovered_requests.len();
    let repeat_continue_request = resumed
        .send_raw_request(
            "thread/activity/continue",
            Some(json!({"threadId": parent.id.clone()})),
        )
        .await?;
    let _: ThreadActivityContinueResponse = timeout(
        REQUEST_TIMEOUT,
        resumed.read_response(repeat_continue_request),
    )
    .await??;
    assert_eq!(responses_server.requests().await.len(), request_count);
    resumed.shutdown_gracefully().await?;
    responses_server.shutdown().await;
    Ok(())
}

fn gated_response(
    response_id: &'static str,
    message: &'static str,
    gate: oneshot::Receiver<()>,
) -> Vec<StreamingSseChunk> {
    vec![
        StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![responses::ev_response_created(response_id)]),
        },
        StreamingSseChunk {
            gate: Some(gate),
            body: responses::sse(vec![
                responses::ev_assistant_message(response_id, message),
                responses::ev_completed(response_id),
            ]),
        },
    ]
}

fn completed_response(response_id: &'static str) -> Vec<StreamingSseChunk> {
    vec![StreamingSseChunk {
        gate: None,
        body: responses::sse(vec![
            responses::ev_response_created(response_id),
            responses::ev_assistant_message(response_id, "recovered once"),
            responses::ev_completed(response_id),
        ]),
    }]
}

fn request_thread_id(request: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(request)
        .ok()
        .and_then(|body| {
            body["client_metadata"]["thread_id"]
                .as_str()
                .map(str::to_owned)
        })
}
