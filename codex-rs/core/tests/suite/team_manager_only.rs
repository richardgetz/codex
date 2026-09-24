use super::*;
use codex_config::TeamLeadWorkPolicy;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::ThreadTeamSettingsUpdate;
use codex_protocol::protocol::SandboxPolicy;
use core_test_support::responses::ev_custom_tool_call;

const MANAGER_ONLY_PROMPT: &str = "delegate this implementation task";
const MANAGER_ONLY_WORKER_TASK: &str = "implement the delegated feature";
const LEAD_EXEC_CALL_ID: &str = "manager-only-lead-exec";
const LEAD_SPAWN_CALL_ID: &str = "manager-only-lead-spawn";
const WORKER_EXEC_CALL_ID: &str = "manager-only-worker-exec";
const CODE_MODE_CALL_ID: &str = "manager-only-code-mode";

fn lead_work_policy_update(policy: TeamLeadWorkPolicy) -> ThreadSettingsOverrides {
    ThreadSettingsOverrides {
        team: Some(ThreadTeamSettingsUpdate {
            mode: TeamMode::LeadWorker,
            lead_work_policy: Some(policy),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn request_has_custom_tool_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    request_body(request)
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("custom_tool_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some(call_id)
            })
        })
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lead_work_policy_changes_next_turn_and_survives_resume() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        (1..=4)
            .map(|index| {
                sse(vec![
                    ev_response_created(&format!("lead-work-policy-{index}")),
                    ev_completed(&format!("lead-work-policy-{index}")),
                ])
            })
            .collect(),
    )
    .await;
    let mut builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.tool_mode = Some(ToolMode::Direct);
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
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
        });
    let test = builder.build_with_auto_env(&server).await?;

    submit_turn(
        &test.codex,
        "prompt guided before policy change",
        ThreadSettingsOverrides::default(),
    )
    .await?;
    let prompt_guided = wait_for_captured_request(
        &responses,
        |request| request.body_contains_text("prompt guided before policy change"),
        "prompt-guided Lead",
    )
    .await;
    let prompt_guided_tools = prompt_guided
        .inputs_of_type("additional_tools")
        .into_iter()
        .next()
        .expect("prompt-guided Lead tool definitions")["tools"]
        .to_string();
    assert!(
        prompt_guided_tools.contains("exec_command"),
        "prompt_guided Lead should retain execution tools: {prompt_guided_tools}"
    );

    submit_thread_settings(
        &test.codex,
        lead_work_policy_update(TeamLeadWorkPolicy::ManagerOnly),
    )
    .await?;
    let manager_only_snapshot = test.codex.config_snapshot().await;
    let manager_only_team = manager_only_snapshot.team.as_ref().expect("team snapshot");
    assert_eq!(manager_only_team.mode, TeamMode::LeadWorker);
    assert_eq!(manager_only_team.role, Some(TeamRole::Lead));
    assert_eq!(
        manager_only_team.lead_work_policy,
        Some(TeamLeadWorkPolicy::ManagerOnly)
    );
    submit_turn(
        &test.codex,
        "manager only after policy change",
        ThreadSettingsOverrides::default(),
    )
    .await?;
    let manager_only = wait_for_captured_request(
        &responses,
        |request| request.body_contains_text("manager only after policy change"),
        "manager-only Lead",
    )
    .await;
    let manager_only_tools = manager_only
        .inputs_of_type("additional_tools")
        .into_iter()
        .next()
        .expect("manager-only Lead tool definitions")["tools"]
        .to_string();
    assert!(
        !manager_only_tools.contains("exec_command"),
        "manager_only Lead should not receive execution tools: {manager_only_tools}"
    );

    submit_thread_settings(
        &test.codex,
        lead_work_policy_update(TeamLeadWorkPolicy::PromptGuided),
    )
    .await?;
    submit_turn(
        &test.codex,
        "prompt guided after reverting policy",
        ThreadSettingsOverrides::default(),
    )
    .await?;
    let prompt_guided_again = wait_for_captured_request(
        &responses,
        |request| request.body_contains_text("prompt guided after reverting policy"),
        "reverted prompt-guided Lead",
    )
    .await;
    let prompt_guided_again_tools = prompt_guided_again
        .inputs_of_type("additional_tools")
        .into_iter()
        .next()
        .expect("reverted prompt-guided Lead tool definitions")["tools"]
        .to_string();
    assert!(
        prompt_guided_again_tools.contains("exec_command"),
        "prompt_guided Lead should regain execution tools on the next turn: {prompt_guided_again_tools}"
    );

    submit_thread_settings(
        &test.codex,
        lead_work_policy_update(TeamLeadWorkPolicy::ManagerOnly),
    )
    .await?;
    let rollout_path = test
        .session_configured
        .rollout_path
        .clone()
        .expect("thread rollout path");
    test.codex.shutdown_and_wait().await?;
    let mut resumed_builder = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.tool_mode = Some(ToolMode::Direct);
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
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
        });
    let resumed = resumed_builder
        .resume(&server, std::sync::Arc::clone(&test.home), rollout_path)
        .await?;
    let resumed_snapshot = resumed.codex.config_snapshot().await;
    assert_eq!(
        resumed_snapshot
            .team
            .as_ref()
            .and_then(|team| team.lead_work_policy),
        Some(TeamLeadWorkPolicy::ManagerOnly)
    );
    submit_turn(
        &resumed.codex,
        "manager only after resume",
        ThreadSettingsOverrides::default(),
    )
    .await?;
    let resumed_manager_only = wait_for_captured_request(
        &responses,
        |request| request.body_contains_text("manager only after resume"),
        "resumed manager-only Lead",
    )
    .await;
    let resumed_tools = resumed_manager_only
        .inputs_of_type("additional_tools")
        .into_iter()
        .next()
        .expect("resumed manager-only Lead tool definitions")["tools"]
        .to_string();
    assert!(
        !resumed_tools.contains("exec_command"),
        "resumed manager_only Lead should retain its restriction: {resumed_tools}"
    );
    Ok(())
}

#[test_case::test_case("gpt-6-sol"; "GPT-6 Sol")]
#[test_case::test_case("gpt-6-astra"; "GPT-6 Astra")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manager_only_code_mode_keeps_coordination_and_rejects_lead_execution(
    lead_model: &'static str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lead_spawn_args = serde_json::to_string(&json!({
        "task_name": "manager_only_code_mode_worker",
        "message": MANAGER_ONLY_WORKER_TASK,
    }))?;
    let lead_code = format!(
        "const worker = await tools.collaboration__spawn_agent({lead_spawn_args});\ntext(JSON.stringify(worker));"
    );
    let lead_exec_args = serde_json::to_string(&json!({ "cmd": "echo lead_execution_marker" }))?;
    let worker_exec_args =
        serde_json::to_string(&json!({ "cmd": "echo worker_execution_marker" }))?;

    let lead_code_mode = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, MANAGER_ONLY_PROMPT)
                && request_has_model(request, lead_model)
                && !request_has_custom_tool_call_output(request, CODE_MODE_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-only-code-mode-lead-1"),
            ev_custom_tool_call(CODE_MODE_CALL_ID, "exec", &lead_code),
            ev_completed("manager-only-code-mode-lead-1"),
        ]),
    )
    .await;
    // Simulate an out-of-contract direct shell call after the nested Code Mode delegation.
    let lead_denial = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            request_has_model(request, lead_model)
                && request_has_custom_tool_call_output(request, CODE_MODE_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-only-code-mode-lead-2"),
            ev_function_call(LEAD_EXEC_CALL_ID, "exec_command", &lead_exec_args),
            ev_completed("manager-only-code-mode-lead-2"),
        ]),
    )
    .await;
    let lead_final = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            request_has_model(request, lead_model)
                && request_has_function_call_output(request, LEAD_EXEC_CALL_ID)
        },
        sse(vec![
            ev_response_created("manager-only-code-mode-lead-3"),
            ev_assistant_message("manager-only-code-mode-lead-message", "delegated"),
            ev_completed("manager-only-code-mode-lead-3"),
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
            ev_response_created("manager-only-code-mode-worker-1"),
            ev_function_call(WORKER_EXEC_CALL_ID, "exec_command", &worker_exec_args),
            ev_completed("manager-only-code-mode-worker-1"),
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
            ev_response_created("manager-only-code-mode-worker-2"),
            ev_assistant_message("manager-only-code-mode-worker-message", "complete"),
            ev_completed("manager-only-code-mode-worker-2"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.tool_mode = Some(ToolMode::Direct);
        })
        .with_model(INITIAL_MODEL)
        .with_config(move |config| {
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
            // This fixture only runs harmless echo commands; avoid nesting Seatbelt under
            // a Seatbelt test runner so the Worker can exercise its execution tool locally.
            config
                .set_legacy_sandbox_policy(SandboxPolicy::DangerFullAccess)
                .expect("DangerFullAccess test policy");
            let profiles = config.team.profiles.as_mut().expect("team profiles");
            profiles.lead.model = lead_model.to_string();
            profiles.lead_work_policy = codex_config::TeamLeadWorkPolicy::ManagerOnly;
        });
    let test = builder.build_with_auto_env(&server).await?;
    submit_turn(
        &test.codex,
        MANAGER_ONLY_PROMPT,
        ThreadSettingsOverrides::default(),
    )
    .await?;

    let lead_request = wait_for_captured_request(
        &lead_code_mode,
        |request| {
            request.body_contains_text(MANAGER_ONLY_PROMPT)
                && response_request_has_model(request, lead_model)
        },
        "manager-only Code Mode Lead",
    )
    .await;
    let additional_tools = lead_request
        .inputs_of_type("additional_tools")
        .into_iter()
        .next()
        .expect("Lead Responses Lite tool definitions");
    let tools = additional_tools["tools"]
        .as_array()
        .expect("additional_tools should contain an array");
    let functions = tools
        .iter()
        .find(|tool| tool["type"] == "namespace" && tool["name"] == "functions")
        .expect("Code Mode should remain in the functions namespace");
    let function_tools = functions["tools"]
        .as_array()
        .expect("functions namespace should contain tools");
    let exec = function_tools
        .iter()
        .find(|tool| tool["name"] == "exec")
        .expect("manager-only Lead should retain functions.exec");
    assert!(
        exec["description"]
            .as_str()
            .is_some_and(|description| description.contains("collaboration__spawn_agent")),
        "functions.exec should retain nested Team coordination: {exec}"
    );
    assert!(
        !function_tools
            .iter()
            .any(|tool| tool["name"] == "exec_command"),
        "Code Mode Lead should not receive direct execution tools: {function_tools:?}"
    );

    let _denial_request = wait_for_captured_request(
        &lead_denial,
        |request| {
            response_request_has_model(request, lead_model)
                && request
                    .inputs_of_type("custom_tool_call_output")
                    .iter()
                    .any(|item| item["call_id"] == CODE_MODE_CALL_ID)
        },
        "manager-only Lead after nested Code Mode delegation",
    )
    .await;
    let denied_tool_response = wait_for_captured_request(
        &lead_final,
        |request| {
            response_request_has_model(request, lead_model)
                && response_request_has_function_call_output(request, LEAD_EXEC_CALL_ID)
        },
        "manager-only Lead after denied execution",
    )
    .await;
    let denied_tool_output = denied_tool_response
        .function_call_output_text(LEAD_EXEC_CALL_ID)
        .expect("Lead execution call response");
    assert!(
        denied_tool_output.contains("manager_only Team Lead"),
        "manager-only must reject an execution call emitted by the Lead: {denied_tool_output}"
    );

    let worker_request = wait_for_captured_request(
        &worker_exec,
        |request| {
            request.body_contains_text(MANAGER_ONLY_WORKER_TASK)
                && response_request_has_model(request, WORKER_MODEL)
        },
        "Worker spawned by the Code Mode Lead",
    )
    .await;
    let worker_additional_tools = worker_request
        .inputs_of_type("additional_tools")
        .into_iter()
        .next()
        .expect("Worker Responses Lite tool definitions");
    assert!(
        worker_additional_tools["tools"]
            .to_string()
            .contains("exec_command"),
        "Worker should retain normal execution tools: {}",
        worker_additional_tools["tools"]
    );
    let worker_final_request = wait_for_captured_request(
        &worker_final,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(request, WORKER_EXEC_CALL_ID)
        },
        "Worker after execution",
    )
    .await;
    let worker_exec_output = worker_final_request
        .function_call_output_text(WORKER_EXEC_CALL_ID)
        .expect("Worker execution call response");
    assert!(
        worker_exec_output.contains("worker_execution_marker"),
        "Worker execution should run successfully: {worker_exec_output}"
    );
    let child_thread_id = worker_request.body_json()["client_metadata"]["thread_id"]
        .as_str()
        .and_then(|thread_id| ThreadId::from_string(thread_id).ok())
        .expect("Worker thread ID");
    let worker_thread = test.thread_manager.get_thread(child_thread_id).await?;
    wait_for_event(worker_thread.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}
