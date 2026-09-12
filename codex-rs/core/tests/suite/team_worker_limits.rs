use super::*;

use test_case::test_case;

const SHELL_LIMIT_PROMPT: &str = "start a worker before shell coverage";
const SHELL_LIMIT_TASK: &str = "hold the worker task before shell";
const SHELL_LIMIT_SPAWN_CALL_ID: &str = "worker-limit-shell-first";
const SHELL_LIMIT_SECOND_PROMPT: &str = "try another worker while shell runs";
const SHELL_LIMIT_SECOND_TASK: &str = "the second worker must stay rejected";
const SHELL_LIMIT_SECOND_SPAWN_CALL_ID: &str = "worker-limit-shell-second";

#[test_case(MultiAgentVersion::V1, MULTI_AGENT_V1_NAMESPACE; "legacy backend")]
#[test_case(MultiAgentVersion::V2, MULTI_AGENT_V2_NAMESPACE; "v2 backend")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_worker_limit_admits_shell_task_while_worker_idle(
    lead_multi_agent_version: MultiAgentVersion,
    tool_namespace: &str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let first_spawn_args = serde_json::to_string(&json!({
        "message": SHELL_LIMIT_TASK,
        "task_name": "worker_limit_shell_first",
        "fork_turns": "none",
    }))?;
    let second_spawn_args = serde_json::to_string(&json!({
        "message": SHELL_LIMIT_SECOND_TASK,
        "task_name": "worker_limit_shell_second",
        "fork_turns": "none",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, SHELL_LIMIT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, SHELL_LIMIT_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("worker-limit-shell-root-1"),
            ev_function_call_with_namespace(
                SHELL_LIMIT_SPAWN_CALL_ID,
                tool_namespace,
                "spawn_agent",
                &first_spawn_args,
            ),
            ev_completed("worker-limit-shell-root-1"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, SHELL_LIMIT_SPAWN_CALL_ID)
                && !request_has_function_call_output(request, SHELL_LIMIT_SECOND_SPAWN_CALL_ID)
                && !body_contains(request, "worker initial complete")
        },
        sse(vec![
            ev_response_created("worker-limit-shell-root-2"),
            ev_assistant_message("worker-limit-shell-root-message", "worker started"),
            ev_completed("worker-limit-shell-root-2"),
        ]),
    )
    .await;
    let worker_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, SHELL_LIMIT_TASK) && request_has_model(request, WORKER_MODEL)
        },
        sse(vec![
            ev_response_created("worker-limit-shell-worker"),
            ev_assistant_message(
                "worker-limit-shell-message",
                "worker initial complete",
            ),
            ev_completed("worker-limit-shell-worker"),
        ]),
    )
    .await;
    // The completion watcher wakes a parked Team Lead once the initial Worker
    // turn finishes; answer that review turn before exercising shell capacity.
    let root_after_worker_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, SHELL_LIMIT_SPAWN_CALL_ID)
                && !request_has_function_call_output(request, SHELL_LIMIT_SECOND_SPAWN_CALL_ID)
                && body_contains(request, "worker initial complete")
        },
        sse(vec![
            ev_response_created("worker-limit-shell-root-worker-completion"),
            ev_assistant_message(
                "worker-limit-shell-root-worker-completion-message",
                "worker completion reviewed",
            ),
            ev_completed("worker-limit-shell-root-worker-completion"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model_info_override(LEAD_MODEL, move |model_info| {
            model_info.multi_agent_version = Some(lead_multi_agent_version);
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.multi_agent_version = Some(MultiAgentVersion::V1);
        })
        .with_model(INITIAL_MODEL)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("Collab feature");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("MultiAgentV2 feature");
            configure_team(config, TeamMode::LeadWorker);
            config.team.worker_max_concurrent = Some(1);
        });
    let test = builder.build_with_auto_env(&server).await?;
    submit_turn(
        &test.codex,
        SHELL_LIMIT_PROMPT,
        ThreadSettingsOverrides::default(),
    )
    .await?;
    let worker_request = wait_for_captured_request(
        &worker_response,
        |request| {
            request.body_contains_text(SHELL_LIMIT_TASK)
                && response_request_has_model(request, WORKER_MODEL)
        },
        "shell test first worker",
    )
    .await;
    let worker_thread_id = worker_request
        .body_json()
        .get("client_metadata")
        .and_then(|metadata| metadata.get("thread_id"))
        .and_then(Value::as_str)
        .and_then(|id| ThreadId::from_string(id).ok())
        .unwrap_or_else(|| {
            panic!(
                "first worker request missing a valid thread ID: {:?}",
                worker_request.body_json()
            )
        });
    let worker = test.thread_manager.get_thread(worker_thread_id).await?;
    wait_for_event(worker.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let _root_after_worker_completion_request = wait_for_captured_request(
        &root_after_worker_completion,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, SHELL_LIMIT_SPAWN_CALL_ID)
                && request.body_contains_text("worker initial complete")
        },
        "shell test worker completion wake",
    )
    .await;

    let shell_command = match codex_core::shell::default_user_shell().name() {
        "powershell" => "Write-Output worker-limit-shell-ready; Start-Sleep -Seconds 60",
        "cmd" => "echo worker-limit-shell-ready & ping -n 61 127.0.0.1 > nul",
        _ => "printf 'worker-limit-shell-ready\\n'; exec sleep 60",
    };
    worker
        .submit(Op::RunUserShellCommand {
            command: shell_command.to_string(),
            timeout_ms: Some(28_800_000),
        })
        .await?;
    wait_for_event(worker.as_ref(), |event| {
        matches!(event, EventMsg::ExecCommandOutputDelta(delta)
            if String::from_utf8_lossy(&delta.chunk).contains("worker-limit-shell-ready"))
    })
    .await;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, SHELL_LIMIT_SECOND_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, SHELL_LIMIT_SECOND_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("worker-limit-shell-root-3"),
            ev_function_call_with_namespace(
                SHELL_LIMIT_SECOND_SPAWN_CALL_ID,
                tool_namespace,
                "spawn_agent",
                &second_spawn_args,
            ),
            ev_completed("worker-limit-shell-root-3"),
        ]),
    )
    .await;
    let root_after_second = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, SHELL_LIMIT_SECOND_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("worker-limit-shell-root-4"),
            ev_assistant_message("worker-limit-shell-root-final", "shell admission checked"),
            ev_completed("worker-limit-shell-root-4"),
        ]),
    )
    .await;
    submit_turn(
        &test.codex,
        SHELL_LIMIT_SECOND_PROMPT,
        ThreadSettingsOverrides::default(),
    )
    .await?;
    let second_request = wait_for_captured_request(
        &root_after_second,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(
                    request,
                    SHELL_LIMIT_SECOND_SPAWN_CALL_ID,
                )
        },
        "shell test second spawn",
    )
    .await;
    let second_output = second_request
        .function_call_output_text(SHELL_LIMIT_SECOND_SPAWN_CALL_ID)
        .expect("second worker spawn output");
    assert!(
        second_output.contains("agent thread limit reached"),
        "shell task must retain the direct Worker slot: {second_output}"
    );
    worker.submit(Op::Interrupt).await?;
    wait_for_event(worker.as_ref(), |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    Ok(())
}
