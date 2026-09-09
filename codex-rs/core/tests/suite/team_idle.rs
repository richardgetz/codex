use super::*;
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
