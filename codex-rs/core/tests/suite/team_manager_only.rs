use super::*;
use codex_protocol::openai_models::ToolMode;

const MANAGER_ONLY_PROMPT: &str = "delegate this implementation task";
const MANAGER_ONLY_WORKER_TASK: &str = "implement the delegated feature";
const LEAD_EXEC_CALL_ID: &str = "manager-only-lead-exec";
const LEAD_SPAWN_CALL_ID: &str = "manager-only-lead-spawn";
const WORKER_EXEC_CALL_ID: &str = "manager-only-worker-exec";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manager_only_lead_delegates_while_worker_keeps_execution_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lead_exec_args = serde_json::to_string(&json!({"cmd": "pwd"}))?;
    let lead_spawn_args = serde_json::to_string(&json!({
        "task_name": "manager_only_worker",
        "message": MANAGER_ONLY_WORKER_TASK,
    }))?;
    let worker_exec_args = serde_json::to_string(&json!({"cmd": "pwd"}))?;

    let lead_denial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, MANAGER_ONLY_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, LEAD_EXEC_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-only-lead-1"),
            ev_function_call(LEAD_EXEC_CALL_ID, "exec_command", &lead_exec_args),
            ev_completed("manager-only-lead-1"),
        ]),
    )
    .await;
    let lead_spawn = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, LEAD_EXEC_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-only-lead-2"),
            ev_function_call_with_namespace(
                LEAD_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &lead_spawn_args,
            ),
            ev_completed("manager-only-lead-2"),
        ]),
    )
    .await;
    let worker_exec = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, MANAGER_ONLY_WORKER_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, WORKER_EXEC_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-only-worker-1"),
            ev_function_call(WORKER_EXEC_CALL_ID, "exec_command", &worker_exec_args),
            ev_completed("manager-only-worker-1"),
        ]),
    )
    .await;
    let worker_final = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, WORKER_EXEC_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-only-worker-2"),
            ev_assistant_message("manager-only-worker-message", "implementation complete"),
            ev_completed("manager-only-worker-2"),
        ]),
    )
    .await;
    let lead_final = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, LEAD_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-only-lead-3"),
            ev_assistant_message("manager-only-lead-message", "delegated to the Worker"),
            ev_completed("manager-only-lead-3"),
        ]),
    )
    .await;

    // These model catalog entries default to CodeModeOnly; exercise the direct tool gate here.
    let mut builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.tool_mode = Some(ToolMode::Direct);
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.tool_mode = Some(ToolMode::Direct);
            model_info.multi_agent_version = Some(MultiAgentVersion::V1);
        })
        .with_model(INITIAL_MODEL)
        .with_config(|config| {
            config
                .features
                .enable(Feature::UnifiedExec)
                .expect("UnifiedExec feature");
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
                .lead_work_policy = codex_config::TeamLeadWorkPolicy::ManagerOnly;
        });
    let test = builder.build_with_auto_env(&server).await?;
    submit_turn(
        &test.codex,
        MANAGER_ONLY_PROMPT,
        ThreadSettingsOverrides::default(),
    )
    .await?;

    let lead_request = wait_for_captured_request(
        &lead_denial,
        |request| {
            request.body_contains_text(MANAGER_ONLY_PROMPT)
                && response_request_has_model(request, LEAD_MODEL)
        },
        "manager-only Lead",
    )
    .await;
    let lead_tools = lead_request
        .inputs_of_type("additional_tools")
        .into_iter()
        .next()
        .expect("Lead Responses Lite tool definitions")["tools"]
        .to_string();
    assert!(
        !lead_tools.contains("exec_command"),
        "manager_only Lead should not receive execution tools: {lead_tools}"
    );
    assert!(
        lead_tools.contains(MULTI_AGENT_V2_NAMESPACE),
        "manager_only Lead should keep coordination tools: {lead_tools}"
    );

    let lead_spawn_request = wait_for_captured_request(
        &lead_spawn,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, LEAD_EXEC_CALL_ID)
        },
        "manager-only Lead after denied execution call",
    )
    .await;
    let denied_tool_output = lead_spawn_request
        .function_call_output_text(LEAD_EXEC_CALL_ID)
        .expect("Lead execution call response");
    assert!(
        denied_tool_output.contains("manager_only Team Lead"),
        "the runtime should reject an execution call even when the model emits one: {denied_tool_output}"
    );

    let worker_request = wait_for_captured_request(
        &worker_exec,
        |request| {
            request.body_contains_text(MANAGER_ONLY_WORKER_TASK)
                && response_request_has_model(request, WORKER_MODEL)
        },
        "Worker",
    )
    .await;
    let worker_tools = worker_request
        .inputs_of_type("additional_tools")
        .into_iter()
        .next()
        .expect("Worker Responses Lite tool definitions")["tools"]
        .to_string();
    assert!(
        worker_tools.contains("exec_command"),
        "Worker should retain execution tools: {worker_tools}"
    );
    let _worker_final_request = wait_for_captured_request(
        &worker_final,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(request, WORKER_EXEC_CALL_ID)
        },
        "Worker after execution",
    )
    .await;
    let child_thread_id = worker_request.body_json()["client_metadata"]["thread_id"]
        .as_str()
        .and_then(|thread_id| ThreadId::from_string(thread_id).ok())
        .expect("Worker thread ID");
    let worker_thread = test.thread_manager.get_thread(child_thread_id).await?;
    wait_for_event(worker_thread.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let _lead_final_request = wait_for_captured_request(
        &lead_final,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, LEAD_SPAWN_CALL_ID)
        },
        "manager-only Lead after Worker completion",
    )
    .await;
    Ok(())
}
