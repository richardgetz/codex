use super::*;
use codex_config::TeamLeadWorkPolicy;
use codex_protocol::items::TurnItem;
use pretty_assertions::assert_eq;

const IDLE_ROOT_PROMPT: &str = "park the lead while the worker runs";
const IDLE_CHILD_TASK: &str = "send routine progress while working";
const IDLE_PROGRESS_CALL_ID: &str = "team-idle-progress";
const IDLE_ACTION_CALL_ID: &str = "team-idle-action";
const IDLE_ACTION_GATE_CALL_ID: &str = "team-idle-action-gate";
const IDLE_COMPLETION_GATE_CALL_ID: &str = "team-idle-completion-gate";
const IDLE_HELPER_GATE_CALL_ID: &str = "team-idle-helper-gate";
const IDLE_HELPER_ACTION_GATE_CALL_ID: &str = "team-idle-helper-action-gate";
const IDLE_GATE_CALL_ID: &str = "team-idle-gate";
const IDLE_SPAWN_CALL_ID: &str = "team-idle-spawn";
const IDLE_WAIT_CALL_ID: &str = "team-idle-wait";
const IDLE_ROOT_GATE_CALL_ID: &str = "team-idle-root-gate";
const IDLE_BARRIER_ID: &str = "team-idle-root-parked";
const IDLE_COMPLETION_BARRIER_ID: &str = "team-idle-completion-ready";
const IDLE_ACTION_BARRIER_ID: &str = "team-idle-action-ready";
const IDLE_HELPER_PROMPT: &str = "release the worker completion gate";
const IDLE_HELPER_ACTION_PROMPT: &str = "hold the worker action completion gate";
const IDLE_ACTION_MESSAGE: &str = "the Worker needs immediate Lead attention";

const DEADLINE_ROOT_PROMPT: &str = "park the lead until its oversight deadline";
const DEADLINE_CHILD_TASK: &str = "keep the worker active through the oversight deadline";
const DEADLINE_SPAWN_CALL_ID: &str = "team-idle-deadline-spawn";
const DEADLINE_WAIT_CALL_ID: &str = "team-idle-deadline-wait";
const DEADLINE_SLEEP_CALL_ID: &str = "team-idle-deadline-sleep";

const HANDOFF_ROOT_PROMPT: &str = "wake the parent when the worker has nothing left to wait for";
const HANDOFF_CHILD_TASK: &str = "report routine progress then wait for parent follow-up";
const HANDOFF_SPAWN_CALL_ID: &str = "team-idle-handoff-spawn";
const HANDOFF_ROOT_WAIT_CALL_ID: &str = "team-idle-handoff-root-wait";
const HANDOFF_CHILD_PROGRESS_CALL_ID: &str = "team-idle-handoff-child-progress";
const HANDOFF_CHILD_WAIT_CALL_ID: &str = "team-idle-handoff-child-wait";
const HANDOFF_ROOT_ACTION_CALL_ID: &str = "team-idle-handoff-root-action";
const HANDOFF_CHILD_PATH: &str = "/root/handoff_worker";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_lead_ignores_routine_progress_until_worker_completion() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": IDLE_CHILD_TASK,
        "task_name": "idle_worker",
        "fork_turns": "none",
    }))?;
    let progress_args = serde_json::to_string(&json!({
        "target": "/root",
        "message": "routine progress should stay parked",
    }))?;
    let action_args = serde_json::to_string(&json!({
        "target": "/root",
        "message": IDLE_ACTION_MESSAGE,
    }))?;
    let gate_args = serde_json::to_string(&json!({
        "barrier": {
            "id": IDLE_BARRIER_ID,
            "participants": 2,
            "timeout_ms": 10_000,
        },
    }))?;
    let wait_args = serde_json::to_string(&json!({
        // A Lead with active Workers uses the configured oversight interval, even when the model
        // asks for a short wait. This value catches accidental 1ms polling in the integration
        // fixture without making the test wait for the configured 30-minute deadline.
        "timeout_ms": 1,
    }))?;
    let completion_gate_args = serde_json::to_string(&json!({
        "barrier": {
            "id": IDLE_COMPLETION_BARRIER_ID,
            "participants": 2,
            "timeout_ms": 10_000,
        },
    }))?;
    let action_gate_args = serde_json::to_string(&json!({
        "barrier": {
            "id": IDLE_ACTION_BARRIER_ID,
            "participants": 2,
            "timeout_ms": 10_000,
        },
    }))?;

    let root_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, IDLE_ROOT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, IDLE_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-root-1"),
            ev_function_call_with_namespace(
                IDLE_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("team-idle-root-1"),
        ]),
    )
    .await;
    let root_park = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, IDLE_SPAWN_CALL_ID)
                && !request_has_function_call_output(request, IDLE_WAIT_CALL_ID)
                && !body_contains(request, "worker finished")
        },
        sse(vec![
            ev_response_created("team-idle-root-2"),
            ev_function_call(IDLE_ROOT_GATE_CALL_ID, "test_sync_tool", &gate_args),
            ev_function_call_with_namespace(
                IDLE_WAIT_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                &wait_args,
            ),
            ev_completed("team-idle-root-2"),
        ]),
    )
    .await;
    let worker_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, IDLE_CHILD_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, IDLE_GATE_CALL_ID)
                && !request_has_function_call_output(request, IDLE_PROGRESS_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-worker-1"),
            ev_function_call(IDLE_GATE_CALL_ID, "test_sync_tool", &gate_args),
            ev_completed("team-idle-worker-1"),
        ]),
    )
    .await;
    let worker_gate = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, IDLE_GATE_CALL_ID)
                && !request_has_function_call_output(request, IDLE_PROGRESS_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-worker-2"),
            ev_function_call_with_namespace(
                IDLE_PROGRESS_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "send_message",
                &progress_args,
            ),
            ev_completed("team-idle-worker-2"),
        ]),
    )
    .await;
    let worker_progress = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, IDLE_GATE_CALL_ID)
                && request_has_function_call_output(request, IDLE_PROGRESS_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-worker-3"),
            ev_function_call(
                IDLE_COMPLETION_GATE_CALL_ID,
                "test_sync_tool",
                &completion_gate_args,
            ),
            ev_completed("team-idle-worker-3"),
        ]),
    )
    .await;
    let worker_action = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, IDLE_PROGRESS_CALL_ID)
                && request_has_function_call_output(request, IDLE_COMPLETION_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-worker-4"),
            ev_function_call(IDLE_ACTION_CALL_ID, "send_message_action", &action_args),
            ev_function_call(
                IDLE_ACTION_GATE_CALL_ID,
                "test_sync_tool",
                &action_gate_args,
            ),
            ev_completed("team-idle-worker-4"),
        ]),
    )
    .await;
    let worker_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, IDLE_ACTION_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-worker-5"),
            ev_assistant_message("team-idle-worker-message", "worker finished"),
            ev_completed("team-idle-worker-5"),
        ]),
    )
    .await;
    let helper_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, INITIAL_MODEL) && body_contains(request, IDLE_HELPER_PROMPT)
        },
        sse(vec![
            ev_response_created("team-idle-helper-1"),
            ev_function_call(
                IDLE_HELPER_GATE_CALL_ID,
                "test_sync_tool",
                &completion_gate_args,
            ),
            ev_completed("team-idle-helper-1"),
        ]),
    )
    .await;
    let helper_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, INITIAL_MODEL)
                && request_has_function_call_output(request, IDLE_HELPER_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-helper-2"),
            ev_assistant_message("team-idle-helper-message", "completion gate released"),
            ev_completed("team-idle-helper-2"),
        ]),
    )
    .await;
    let helper_action_gate = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, INITIAL_MODEL)
                && body_contains(request, IDLE_HELPER_ACTION_PROMPT)
        },
        sse(vec![
            ev_response_created("team-idle-helper-action-1"),
            ev_function_call(
                IDLE_HELPER_ACTION_GATE_CALL_ID,
                "test_sync_tool",
                &action_gate_args,
            ),
            ev_completed("team-idle-helper-action-1"),
        ]),
    )
    .await;
    let helper_action_done = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, INITIAL_MODEL)
                && request_has_function_call_output(request, IDLE_HELPER_ACTION_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-helper-action-2"),
            ev_assistant_message("team-idle-helper-action-message", "action gate released"),
            ev_completed("team-idle-helper-action-2"),
        ]),
    )
    .await;
    let root_after_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, IDLE_SPAWN_CALL_ID)
                && request_has_function_call_output(request, IDLE_ROOT_GATE_CALL_ID)
                && request_has_function_call_output(request, IDLE_WAIT_CALL_ID)
                && body_contains(request, "worker finished")
        },
        sse(vec![
            ev_response_created("team-idle-root-3"),
            ev_assistant_message("team-idle-root-message", "review resumed"),
            ev_completed("team-idle-root-3"),
        ]),
    )
    .await;
    let root_after_action = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, IDLE_SPAWN_CALL_ID)
                && request_has_function_call_output(request, IDLE_ROOT_GATE_CALL_ID)
                && request_has_function_call_output(request, IDLE_WAIT_CALL_ID)
                && body_contains(request, IDLE_ACTION_MESSAGE)
                && !body_contains(request, "worker finished")
        },
        sse(vec![
            ev_response_created("team-idle-root-action"),
            ev_assistant_message("team-idle-root-action-message", "action wake observed"),
            ev_completed("team-idle-root-action"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
            model_info
                .experimental_supported_tools
                .push("test_sync_tool".to_string());
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
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
                .enable(Feature::MultiAgentV2)
                .expect("MultiAgentV2 feature");
            configure_team(config, TeamMode::LeadWorker);
            // This scenario asserts the legacy passive parked notice as well as
            // the actionable wake paths, so opt into the debug-only notice.
            config.team.lead_show_idle_notifications = true;
        });
    let test = builder.build_with_auto_env(&server).await?;
    let mut helper_builder = test_codex()
        .with_model_info_override(INITIAL_MODEL, |model_info| {
            model_info
                .experimental_supported_tools
                .push("test_sync_tool".to_string());
        })
        .with_model(INITIAL_MODEL);
    let helper = helper_builder.build_with_auto_env(&server).await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: IDLE_ROOT_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let _root_park_request = wait_for_captured_request(
        &root_park,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, IDLE_SPAWN_CALL_ID)
                && !response_request_has_function_call_output(request, IDLE_WAIT_CALL_ID)
        },
        "Lead idle wait",
    )
    .await;
    assert_eq!(root_initial.requests().len(), 1);
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.contains("Lead wait parked"))
    })
    .await;
    let _worker_initial_request = wait_for_captured_request(
        &worker_initial,
        |request| {
            request.body_contains_text(IDLE_CHILD_TASK)
                && response_request_has_model(request, WORKER_MODEL)
        },
        "idle worker progress",
    )
    .await;
    let _worker_gate_request = wait_for_captured_request(
        &worker_gate,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(request, IDLE_GATE_CALL_ID)
        },
        "idle worker gate",
    )
    .await;

    let _worker_progress_request = wait_for_captured_request(
        &worker_progress,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(request, IDLE_PROGRESS_CALL_ID)
        },
        "routine Worker progress",
    )
    .await;
    let requests = server
        .received_requests()
        .await
        .expect("mock server should record requests");
    let lead_requests = requests
        .iter()
        .filter(|request| {
            request.url.path() == "/v1/responses" && request_has_model(request, LEAD_MODEL)
        })
        .count();
    assert_eq!(
        lead_requests, 2,
        "routine progress must not wake the parked Lead"
    );
    assert!(root_after_action.requests().is_empty());
    assert!(
        root_after_completion.requests().is_empty(),
        "the completion wake must wait for an actionable Worker result"
    );

    helper
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: IDLE_HELPER_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let _helper_initial_request = wait_for_captured_request(
        &helper_initial,
        |request| {
            response_request_has_model(request, INITIAL_MODEL)
                && request.body_contains_text(IDLE_HELPER_PROMPT)
        },
        "completion gate helper",
    )
    .await;
    let action_request = wait_for_captured_request(
        &root_after_action,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && request.body_contains_text(IDLE_ACTION_MESSAGE)
        },
        "Lead action wake",
    )
    .await;
    assert!(action_request.body_contains_text(IDLE_ACTION_MESSAGE));
    let _helper_completion_request = wait_for_captured_request(
        &helper_completion,
        |request| {
            response_request_has_model(request, INITIAL_MODEL)
                && response_request_has_function_call_output(request, IDLE_HELPER_GATE_CALL_ID)
        },
        "completion gate helper completion",
    )
    .await;
    wait_for_event(&helper.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    helper
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: IDLE_HELPER_ACTION_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let _helper_action_gate_request = wait_for_captured_request(
        &helper_action_gate,
        |request| {
            response_request_has_model(request, INITIAL_MODEL)
                && request.body_contains_text(IDLE_HELPER_ACTION_PROMPT)
        },
        "action gate helper",
    )
    .await;
    let _worker_action_request = wait_for_captured_request(
        &worker_action,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(request, IDLE_COMPLETION_GATE_CALL_ID)
        },
        "Worker action and completion gate",
    )
    .await;
    let _helper_action_done_request = wait_for_captured_request(
        &helper_action_done,
        |request| {
            response_request_has_model(request, INITIAL_MODEL)
                && response_request_has_function_call_output(
                    request,
                    IDLE_HELPER_ACTION_GATE_CALL_ID,
                )
        },
        "action gate helper completion",
    )
    .await;
    let _worker_completion_request = wait_for_captured_request(
        &worker_completion,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(request, IDLE_COMPLETION_GATE_CALL_ID)
        },
        "Worker completion after helper release",
    )
    .await;
    wait_for_event(&helper.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let root_request = wait_for_captured_request(
        &root_after_completion,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, IDLE_SPAWN_CALL_ID)
                && request.body_contains_text("worker finished")
        },
        "Lead completion wake",
    )
    .await;
    assert!(root_request.body_contains_text("worker finished"));
    let requests = server
        .received_requests()
        .await
        .expect("mock server should record requests");
    let lead_requests = requests
        .iter()
        .filter(|request| {
            request.url.path() == "/v1/responses" && request_has_model(request, LEAD_MODEL)
        })
        .count();
    assert_eq!(
        lead_requests, 4,
        "explicit action and Worker completion should each wake the Lead once"
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(())
}

const MANAGER_BATCH_ROOT_PROMPT: &str = "delegate two workers and batch their results";
const MANAGER_BATCH_FIRST_TASK: &str = "manager batch first worker task";
const MANAGER_BATCH_SECOND_TASK: &str = "manager batch second worker task";
const MANAGER_BATCH_FIRST_SPAWN_CALL_ID: &str = "manager-batch-first-spawn";
const MANAGER_BATCH_SECOND_SPAWN_CALL_ID: &str = "manager-batch-second-spawn";
const MANAGER_BATCH_FIRST_GATE_CALL_ID: &str = "manager-batch-first-gate";
const MANAGER_BATCH_SECOND_GATE_CALL_ID: &str = "manager-batch-second-gate";
const MANAGER_BATCH_ROOT_WAIT_CALL_ID: &str = "manager-batch-root-wait";
const MANAGER_BATCH_ACTION_CALL_ID: &str = "manager-batch-action";
const MANAGER_BATCH_GATE_ID: &str = "manager-batch-gate";
const MANAGER_BATCH_ACTION_MESSAGE: &str = "second worker asks for immediate review";
const MANAGER_BATCH_FIRST_RESULT: &str = "first manager batch result marker";
const MANAGER_BATCH_SECOND_RESULT: &str = "second manager batch result marker";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manager_only_batches_successful_worker_completions_and_wakes_for_action() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let first_spawn_args = serde_json::to_string(&json!({
        "message": MANAGER_BATCH_FIRST_TASK,
        "task_name": "manager_batch_first",
        "fork_turns": "none",
    }))?;
    let second_spawn_args = serde_json::to_string(&json!({
        "message": MANAGER_BATCH_SECOND_TASK,
        "task_name": "manager_batch_second",
        "fork_turns": "none",
    }))?;
    let gate_args = serde_json::to_string(&json!({
        "barrier": {
            "id": MANAGER_BATCH_GATE_ID,
            "participants": 2,
            "timeout_ms": 10_000,
        },
    }))?;
    let action_args = serde_json::to_string(&json!({
        "target": "/root",
        "message": MANAGER_BATCH_ACTION_MESSAGE,
    }))?;
    let root_wait_args = serde_json::to_string(&json!({ "timeout_ms": 1 }))?;

    let root_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, MANAGER_BATCH_ROOT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, MANAGER_BATCH_FIRST_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-batch-root-initial"),
            ev_function_call_with_namespace(
                MANAGER_BATCH_FIRST_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &first_spawn_args,
            ),
            ev_function_call_with_namespace(
                MANAGER_BATCH_SECOND_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &second_spawn_args,
            ),
            ev_completed("manager-batch-root-initial"),
        ]),
    )
    .await;
    let root_after_spawns = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(
                    request,
                    MANAGER_BATCH_FIRST_SPAWN_CALL_ID,
                )
                && request_has_function_call_output(
                    request,
                    MANAGER_BATCH_SECOND_SPAWN_CALL_ID,
                )
                && !request_has_function_call_output(
                    request,
                    MANAGER_BATCH_ROOT_WAIT_CALL_ID,
                )
        },
        sse(vec![
            ev_response_created("manager-batch-root-wait"),
            ev_function_call_with_namespace(
                MANAGER_BATCH_ROOT_WAIT_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                &root_wait_args,
            ),
            ev_completed("manager-batch-root-wait"),
        ]),
    )
    .await;
    let root_after_action = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, MANAGER_BATCH_ROOT_WAIT_CALL_ID)
                && body_contains(request, MANAGER_BATCH_ACTION_MESSAGE)
                && !body_contains(request, MANAGER_BATCH_FIRST_RESULT)
                && !body_contains(request, MANAGER_BATCH_SECOND_RESULT)
        },
        sse(vec![
            ev_response_created("manager-batch-root-action"),
            ev_assistant_message("manager-batch-root-action-message", "action wake observed"),
            ev_completed("manager-batch-root-action"),
        ]),
    )
    .await;
    let root_after_batch = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, MANAGER_BATCH_ROOT_WAIT_CALL_ID)
                && body_contains(request, MANAGER_BATCH_FIRST_RESULT)
                && body_contains(request, MANAGER_BATCH_SECOND_RESULT)
        },
        sse(vec![
            ev_response_created("manager-batch-root-completion"),
            ev_assistant_message(
                "manager-batch-root-completion-message",
                "both Worker results reviewed",
            ),
            ev_completed("manager-batch-root-completion"),
        ]),
    )
    .await;

    let _first_worker_gate = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, MANAGER_BATCH_FIRST_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, MANAGER_BATCH_FIRST_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-batch-first-worker-gate"),
            ev_function_call(
                MANAGER_BATCH_FIRST_GATE_CALL_ID,
                "test_sync_tool",
                &gate_args,
            ),
            ev_completed("manager-batch-first-worker-gate"),
        ]),
    )
    .await;
    let _first_worker_result = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, MANAGER_BATCH_FIRST_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-batch-first-worker-result"),
            ev_assistant_message("manager-batch-first-worker-message", MANAGER_BATCH_FIRST_RESULT),
            ev_completed("manager-batch-first-worker-result"),
        ]),
    )
    .await;
    let _second_worker_action = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, MANAGER_BATCH_SECOND_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, MANAGER_BATCH_ACTION_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-batch-second-worker-action"),
            ev_function_call(
                MANAGER_BATCH_ACTION_CALL_ID,
                "send_message_action",
                &action_args,
            ),
            ev_completed("manager-batch-second-worker-action"),
        ]),
    )
    .await;
    let _second_worker_gate = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, MANAGER_BATCH_ACTION_CALL_ID)
                && !request_has_function_call_output(request, MANAGER_BATCH_SECOND_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-batch-second-worker-gate"),
            ev_function_call(
                MANAGER_BATCH_SECOND_GATE_CALL_ID,
                "test_sync_tool",
                &gate_args,
            ),
            ev_completed("manager-batch-second-worker-gate"),
        ]),
    )
    .await;
    let _second_worker_result = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, MANAGER_BATCH_SECOND_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-batch-second-worker-result"),
            ev_assistant_message(
                "manager-batch-second-worker-message",
                MANAGER_BATCH_SECOND_RESULT,
            ),
            ev_completed("manager-batch-second-worker-result"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
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
                .enable(Feature::MultiAgentV2)
                .expect("MultiAgentV2 feature");
            configure_team(config, TeamMode::LeadWorker);
            config
                .team
                .profiles
                .as_mut()
                .expect("team profiles")
                .lead_work_policy = TeamLeadWorkPolicy::ManagerOnly;
        })
        .build_with_auto_env(&server)
        .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: MANAGER_BATCH_ROOT_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_captured_request(
        &root_initial,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && request.body_contains_text(MANAGER_BATCH_ROOT_PROMPT)
        },
        "manager-only Worker spawns",
    )
    .await;

    wait_for_captured_request(
        &root_after_spawns,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(
                    request,
                    MANAGER_BATCH_FIRST_SPAWN_CALL_ID,
                )
                && response_request_has_function_call_output(
                    request,
                    MANAGER_BATCH_SECOND_SPAWN_CALL_ID,
                )
                && !response_request_has_function_call_output(
                    request,
                    MANAGER_BATCH_ROOT_WAIT_CALL_ID,
                )
        },
        "normal manager-only Lead continuation after Worker spawns",
    )
    .await;

    wait_for_captured_request(
        &root_after_action,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, MANAGER_BATCH_ROOT_WAIT_CALL_ID)
                && request.body_contains_text(MANAGER_BATCH_ACTION_MESSAGE)
        },
        "immediate manager-only action wake",
    )
    .await;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    wait_for_captured_request(
        &root_after_batch,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && request.body_contains_text(MANAGER_BATCH_FIRST_RESULT)
                && request.body_contains_text(MANAGER_BATCH_SECOND_RESULT)
        },
        "manager-only Worker completion batch",
    )
    .await;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let lead_requests = server
        .received_requests()
        .await
        .expect("mock server should record requests")
        .into_iter()
        .filter(|request| {
            request.url.path() == "/v1/responses" && request_has_model(request, LEAD_MODEL)
        })
        .collect::<Vec<_>>();
    let action_wakes = lead_requests
        .iter()
        .filter(|request| {
            request_has_function_call_output(request, MANAGER_BATCH_ROOT_WAIT_CALL_ID)
                && body_contains(request, MANAGER_BATCH_ACTION_MESSAGE)
                && !body_contains(request, MANAGER_BATCH_FIRST_RESULT)
                && !body_contains(request, MANAGER_BATCH_SECOND_RESULT)
        })
        .count();
    assert_eq!(
        action_wakes, 1,
        "the Worker action should wake the waiting Lead immediately"
    );

    let completion_wakes = lead_requests
        .iter()
        .filter(|request| {
            body_contains(request, MANAGER_BATCH_FIRST_RESULT)
                || body_contains(request, MANAGER_BATCH_SECOND_RESULT)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        completion_wakes.len(),
        1,
        "both successful Worker completions should share one Lead wake"
    );
    assert!(body_contains(completion_wakes[0], MANAGER_BATCH_FIRST_RESULT));
    assert!(body_contains(completion_wakes[0], MANAGER_BATCH_SECOND_RESULT));
    Ok(())
}

const MANAGER_HANDOFF_ROOT_PROMPT: &str = "delegate one worker before handoff";
const MANAGER_HANDOFF_WORKER_TASK: &str = "manager handoff worker task";
const MANAGER_HANDOFF_SPAWN_CALL_ID: &str = "manager-handoff-spawn";
const MANAGER_HANDOFF_GATE_CALL_ID: &str = "manager-handoff-gate";
const MANAGER_HANDOFF_RESULT: &str = "manager handoff retry result marker";

#[tokio::test(flavor = "current_thread")]
async fn manager_only_completion_batch_retries_after_temporary_handoff_seal() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": MANAGER_HANDOFF_WORKER_TASK,
        "task_name": "manager_handoff_worker",
        "fork_turns": "none",
    }))?;
    let delay_args = serde_json::to_string(&json!({"sleep_before_ms": 2_000}))?;
    let root_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, MANAGER_HANDOFF_ROOT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
        },
        sse(vec![
            ev_response_created("manager-handoff-root-initial"),
            ev_function_call_with_namespace(
                MANAGER_HANDOFF_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("manager-handoff-root-initial"),
        ]),
    )
    .await;
    let worker_gate = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, MANAGER_HANDOFF_WORKER_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, MANAGER_HANDOFF_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-handoff-worker-gate"),
            ev_function_call(
                MANAGER_HANDOFF_GATE_CALL_ID,
                "test_sync_tool",
                &delay_args,
            ),
            ev_completed("manager-handoff-worker-gate"),
        ]),
    )
    .await;
    let worker_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, MANAGER_HANDOFF_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-handoff-worker-completion"),
            ev_assistant_message("manager-handoff-worker-message", MANAGER_HANDOFF_RESULT),
            ev_completed("manager-handoff-worker-completion"),
        ]),
    )
    .await;
    let root_after_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && body_contains(request, MANAGER_HANDOFF_RESULT)
        },
        sse(vec![
            ev_response_created("manager-handoff-root-completion"),
            ev_assistant_message(
                "manager-handoff-root-completion-message",
                "the Worker completion was reviewed after handoff reopened",
            ),
            ev_completed("manager-handoff-root-completion"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
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
                .enable(Feature::MultiAgentV2)
                .expect("MultiAgentV2 feature");
            configure_team(config, TeamMode::LeadWorker);
            config
                .team
                .profiles
                .as_mut()
                .expect("team profiles")
                .lead_work_policy = TeamLeadWorkPolicy::ManagerOnly;
        })
        .build_with_auto_env(&server)
        .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: MANAGER_HANDOFF_ROOT_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_captured_request(
        &root_initial,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && request.body_contains_text(MANAGER_HANDOFF_ROOT_PROMPT)
        },
        "manager-only Worker spawn before handoff",
    )
    .await;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    wait_for_captured_request(
        &worker_gate,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && request.body_contains_text(MANAGER_HANDOFF_WORKER_TASK)
        },
        "Worker active before handoff",
    )
    .await;

    // Hold the tree while the real Worker is delayed before its terminal report.
    let handoff = test.codex.begin_handoff()?;
    wait_for_captured_request(
        &worker_completion,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(request, MANAGER_HANDOFF_GATE_CALL_ID)
        },
        "Worker completion while handoff is sealed",
    )
    .await;

    let mailbox_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let preflight = test.codex.handoff_preflight().await;
        if preflight
            .blockers
            .contains(&codex_protocol::turn_input::HandoffBlocker::PendingMailbox)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < mailbox_deadline,
            "manager-only Worker completion should reach the Lead mailbox"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    tokio::time::sleep(Duration::from_millis(151)).await;
    assert!(test.codex.handoff_admission_sealed());
    assert!(test
        .codex
        .handoff_preflight()
        .await
        .blockers
        .contains(&codex_protocol::turn_input::HandoffBlocker::PendingMailbox));
    assert!(
        !root_after_completion.requests().iter().any(|request| {
            response_request_has_model(request, LEAD_MODEL)
                && request.body_contains_text(MANAGER_HANDOFF_RESULT)
        }),
        "the completion wake must remain queued while handoff admission is sealed"
    );

    let request_count_before_release = root_after_completion.requests().len();
    drop(handoff);

    let deadline = Instant::now() + Duration::from_secs(2);
    let root_request = loop {
        let requests = root_after_completion.requests();
        if let Some(request) = requests
            .iter()
            .skip(request_count_before_release)
            .find(|request| response_request_has_model(request, LEAD_MODEL))
        {
            break request.clone();
        }
        if Instant::now() >= deadline {
            let post_release_requests = requests
                .iter()
                .skip(request_count_before_release)
                .map(|request| {
                    json!({
                        "model": request.body_json()["model"],
                        "user_input": request.message_input_text_groups("user"),
                        "has_completion_marker":
                            request.body_contains_text(MANAGER_HANDOFF_RESULT),
                    })
                })
                .collect::<Vec<_>>();
            if post_release_requests.is_empty() {
                panic!(
                    "manager-only completion retry after handoff release produced no Responses request"
                );
            }
            panic!(
                "manager-only completion retry after handoff release produced no request using \
                 the expected Lead model {LEAD_MODEL}; captured post-release Responses requests \
                 (model, user input, marker): {post_release_requests:#?}"
            );
        }
        sleep(Duration::from_millis(10)).await;
    };
    let root_user_input = root_request.message_input_text_groups("user");
    assert!(
        root_request.body_contains_text(MANAGER_HANDOFF_RESULT),
        "the first post-release Lead request omitted the Worker result marker; user input: {root_user_input:#?}"
    );
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(())
}

#[tokio::test]
async fn team_lead_default_hides_passive_notice_but_wakes_at_oversight_deadline() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": DEADLINE_CHILD_TASK,
        "task_name": "deadline_worker",
        "fork_turns": "none",
    }))?;
    let wait_args = serde_json::to_string(&json!({ "timeout_ms": 1 }))?;
    let sleep_args = serde_json::to_string(&json!({ "sleep_after_ms": 120_000 }))?;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, DEADLINE_ROOT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, DEADLINE_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-deadline-root-1"),
            ev_function_call_with_namespace(
                DEADLINE_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("team-idle-deadline-root-1"),
        ]),
    )
    .await;
    let root_wait = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, DEADLINE_SPAWN_CALL_ID)
                && !request_has_function_call_output(request, DEADLINE_WAIT_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-deadline-root-2"),
            ev_function_call_with_namespace(
                DEADLINE_WAIT_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                &wait_args,
            ),
            ev_completed("team-idle-deadline-root-2"),
        ]),
    )
    .await;
    let root_after_deadline = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, DEADLINE_WAIT_CALL_ID)
                && body_contains(request, "oversight deadline has elapsed")
        },
        sse(vec![
            ev_response_created("team-idle-deadline-root-3"),
            ev_assistant_message("team-idle-deadline-root-message", "deadline wake observed"),
            ev_completed("team-idle-deadline-root-3"),
        ]),
    )
    .await;
    let worker_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, DEADLINE_CHILD_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, DEADLINE_SLEEP_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-deadline-worker-1"),
            ev_function_call(DEADLINE_SLEEP_CALL_ID, "test_sync_tool", &sleep_args),
            ev_completed("team-idle-deadline-worker-1"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
            model_info
                .experimental_supported_tools
                .push("test_sync_tool".to_string());
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
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
                .enable(Feature::MultiAgentV2)
                .expect("MultiAgentV2 feature");
            configure_team(config, TeamMode::LeadWorker);
            config
                .team
                .profiles
                .as_mut()
                .expect("team profiles")
                .lead_oversight_timeout_minutes = 1;
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: DEADLINE_ROOT_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let _ = wait_for_captured_request(
        &root_wait,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, DEADLINE_SPAWN_CALL_ID)
        },
        "Lead deadline wait",
    )
    .await;
    let _ = wait_for_captured_request(
        &worker_initial,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && request.body_contains_text(DEADLINE_CHILD_TASK)
        },
        "deadline worker",
    )
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::ItemStarted(item)
                if matches!(&item.item, TurnItem::CollabAgentToolCall(call) if call.id == DEADLINE_WAIT_CALL_ID)
        )
    })
    .await;

    // The wait handler emits the passive parked notice immediately after its lifecycle item.
    // Drain any queued events briefly and fail if that notice appears with the default setting.
    let passive_notice = tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            let event = test
                .codex
                .next_event()
                .await
                .expect("event stream should remain open while the Worker sleeps");
            if let EventMsg::Warning(warning) = event.msg
                && (warning.message.contains("Lead idle while")
                    || warning.message.contains("Lead wait parked while"))
            {
                return warning.message;
            }
        }
    })
    .await
    .ok();
    assert!(
        passive_notice.is_none(),
        "passive idle notice should be hidden by default: {passive_notice:?}"
    );

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::time::resume();
    let deadline_warning = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.contains("Lead oversight deadline reached"))
    })
    .await;
    let EventMsg::Warning(deadline_warning) = deadline_warning else {
        unreachable!("deadline warning matcher should only return warnings")
    };
    assert!(
        deadline_warning
            .message
            .contains("Lead oversight deadline reached")
    );
    let _ = wait_for_captured_request(
        &root_after_deadline,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, DEADLINE_WAIT_CALL_ID)
        },
        "Lead oversight deadline wake",
    )
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_free_worker_wait_hands_off_to_parent_and_parent_action_wakes_worker()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": HANDOFF_CHILD_TASK,
        "task_name": "handoff_worker",
        "fork_turns": "none",
    }))?;
    let progress_args = serde_json::to_string(&json!({
        "target": "/root",
        "message": "routine progress before dependency-free wait",
    }))?;
    let wait_args = serde_json::to_string(&json!({ "timeout_ms": 1 }))?;
    let action_args = serde_json::to_string(&json!({
        "target": HANDOFF_CHILD_PATH,
        "message": "parent follow-up is ready",
    }))?;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, HANDOFF_ROOT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, HANDOFF_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-handoff-root-1"),
            ev_function_call_with_namespace(
                HANDOFF_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("team-idle-handoff-root-1"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, HANDOFF_SPAWN_CALL_ID)
                && !request_has_function_call_output(request, HANDOFF_ROOT_WAIT_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-handoff-root-2"),
            ev_function_call_with_namespace(
                HANDOFF_ROOT_WAIT_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                &wait_args,
            ),
            ev_completed("team-idle-handoff-root-2"),
        ]),
    )
    .await;
    let root_handoff = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, HANDOFF_ROOT_WAIT_CALL_ID)
                && body_contains(request, "Routine Worker progress summary")
                && body_contains(request, "no active child dependency")
        },
        sse(vec![
            ev_response_created("team-idle-handoff-root-3"),
            ev_function_call(
                HANDOFF_ROOT_ACTION_CALL_ID,
                "send_message_action",
                &action_args,
            ),
            ev_completed("team-idle-handoff-root-3"),
        ]),
    )
    .await;
    let root_after_action = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, HANDOFF_ROOT_ACTION_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-handoff-root-3b"),
            ev_assistant_message(
                "team-idle-handoff-root-action-message",
                "parent action sent",
            ),
            ev_completed("team-idle-handoff-root-3b"),
        ]),
    )
    .await;
    let root_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && body_contains(request, "follow-up received")
                && !body_contains(request, "no active child dependency")
        },
        sse(vec![
            ev_response_created("team-idle-handoff-root-4"),
            ev_assistant_message(
                "team-idle-handoff-root-message",
                "handoff and completion reviewed",
            ),
            ev_completed("team-idle-handoff-root-4"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, HANDOFF_CHILD_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, HANDOFF_CHILD_WAIT_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-handoff-worker-1"),
            ev_function_call_with_namespace(
                HANDOFF_CHILD_PROGRESS_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "send_message",
                &progress_args,
            ),
            ev_function_call_with_namespace(
                HANDOFF_CHILD_WAIT_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                &wait_args,
            ),
            ev_completed("team-idle-handoff-worker-1"),
        ]),
    )
    .await;
    let worker_after_action = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, HANDOFF_CHILD_WAIT_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-idle-handoff-worker-2"),
            ev_assistant_message("team-idle-handoff-worker-message", "follow-up received"),
            ev_completed("team-idle-handoff-worker-2"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_model(INITIAL_MODEL)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("Collab feature");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("MultiAgentV2 feature");
            configure_team(config, TeamMode::LeadWorker);
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: HANDOFF_ROOT_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let handoff_request = wait_for_captured_request(
        &root_handoff,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, HANDOFF_ROOT_WAIT_CALL_ID)
        },
        "dependency-free Worker handoff",
    )
    .await;
    assert!(handoff_request.body_contains_text("no active child dependency"));
    assert!(handoff_request.body_contains_text(HANDOFF_CHILD_PATH));
    assert!(handoff_request.body_contains_text("Routine Worker progress summary"));
    let action_request = wait_for_captured_request(
        &root_after_action,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, HANDOFF_ROOT_ACTION_CALL_ID)
        },
        "parent action wake",
    )
    .await;
    assert!(action_request.body_contains_text("parent follow-up is ready"));
    let _ = wait_for_captured_request(
        &worker_after_action,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(request, HANDOFF_CHILD_WAIT_CALL_ID)
        },
        "Worker wait wake",
    )
    .await;
    let _ = wait_for_captured_request(
        &root_completion,
        |request| response_request_has_model(request, LEAD_MODEL),
        "Worker completion wake",
    )
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(())
}
