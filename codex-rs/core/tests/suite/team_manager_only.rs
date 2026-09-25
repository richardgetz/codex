use super::*;
use codex_config::TeamLeadWorkPolicy;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::ThreadTeamSettingsUpdate;
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

async fn wait_for_completed_agent_message(thread: &codex_core::CodexThread, expected: &str) {
    let expected_status =
        codex_protocol::protocol::AgentStatus::Completed(Some(expected.to_string()));
    let status = tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 10), async {
        loop {
            let status = thread.agent_status().await;
            if status == expected_status {
                break status;
            }
            match &status {
                codex_protocol::protocol::AgentStatus::Errored(_)
                | codex_protocol::protocol::AgentStatus::Shutdown
                | codex_protocol::protocol::AgentStatus::NotFound
                | codex_protocol::protocol::AgentStatus::Completed(_) => break status,
                codex_protocol::protocol::AgentStatus::PendingInit
                | codex_protocol::protocol::AgentStatus::Running
                | codex_protocol::protocol::AgentStatus::Interrupted => {
                    tokio::time::sleep(std::time::Duration::from_millis(/*millis*/ 10)).await;
                }
            }
        }
    })
    .await
    .expect("agent should reach a terminal status");
    pretty_assertions::assert_eq!(
        status,
        expected_status,
        "agent should complete with its expected final assistant message"
    );
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
    assert!(
        lead_request.body_contains_text("changes execution ownership, not tool access")
    );
    assert!(lead_request.body_contains_text("reuse a suitable available Worker"));
    assert!(lead_request
        .body_contains_text("delegate in parallel within the configured runtime limit"));
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
    pretty_assertions::assert_eq!(
        manager_only_team.mode,
        codex_protocol::protocol::TeamMode::LeadWorker
    );
    pretty_assertions::assert_eq!(
        manager_only_team.role,
        Some(codex_protocol::protocol::TeamRole::Lead)
    );
    pretty_assertions::assert_eq!(
        manager_only_team.lead_work_policy,
        Some(codex_protocol::protocol::TeamLeadWorkPolicy::ManagerOnly)
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
    pretty_assertions::assert_eq!(
        resumed_snapshot
            .team
            .as_ref()
            .and_then(|team| team.lead_work_policy),
        Some(codex_protocol::protocol::TeamLeadWorkPolicy::ManagerOnly)
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

const POLICY_SWITCH_ROOT_PROMPT: &str = "delegate both workers before changing Lead policy";
const POLICY_SWITCH_FIRST_TASK: &str = "policy switch first worker task";
const POLICY_SWITCH_SECOND_TASK: &str = "policy switch second worker task";
const POLICY_SWITCH_WORKERS_BARRIER_ID: &str = "policy-switch-workers-active";
const POLICY_SWITCH_FIRST_SPAWN_CALL_ID: &str = "policy-switch-first-spawn";
const POLICY_SWITCH_SECOND_SPAWN_CALL_ID: &str = "policy-switch-second-spawn";
const POLICY_SWITCH_FIRST_GATE_CALL_ID: &str = "policy-switch-first-gate";
const POLICY_SWITCH_SECOND_GATE_CALL_ID: &str = "policy-switch-second-gate";
const POLICY_SWITCH_SECOND_HOLD_CALL_ID: &str = "policy-switch-second-hold";
const POLICY_SWITCH_RELEASE_SECOND_CALL_ID: &str = "policy-switch-release-second";
const POLICY_SWITCH_FIRST_RESULT: &str = "policy switch first completion marker";
const POLICY_SWITCH_SECOND_RESULT: &str = "policy switch second completion marker";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_to_prompt_guided_releases_buffered_worker_completion() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let first_spawn_args = serde_json::to_string(&json!({
        "message": POLICY_SWITCH_FIRST_TASK,
        "task_name": "policy_switch_first_worker",
        "fork_turns": "none",
    }))?;
    let second_spawn_args = serde_json::to_string(&json!({
        "message": POLICY_SWITCH_SECOND_TASK,
        "task_name": "policy_switch_second_worker",
        "fork_turns": "none",
    }))?;
    let worker_barrier_args = json!({
        "id": POLICY_SWITCH_WORKERS_BARRIER_ID,
        "participants": 2,
        "timeout_ms": 60_000,
    });
    let first_gate_args = serde_json::to_string(&json!({"barrier": worker_barrier_args.clone()}))?;
    let second_gate_args = serde_json::to_string(&json!({"barrier": worker_barrier_args}))?;
    let second_worker_hold_barrier = json!({
        "id": "policy-switch-second-worker-hold",
        "participants": 2,
        "timeout_ms": 30_000,
    });
    let second_worker_hold_args =
        serde_json::to_string(&json!({"barrier": second_worker_hold_barrier.clone()}))?;
    let lead_release_second_args =
        serde_json::to_string(&json!({"barrier": second_worker_hold_barrier}))?;

    let root_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, POLICY_SWITCH_ROOT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, POLICY_SWITCH_FIRST_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("policy-switch-root-initial"),
            ev_function_call_with_namespace(
                POLICY_SWITCH_FIRST_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &first_spawn_args,
            ),
            ev_function_call_with_namespace(
                POLICY_SWITCH_SECOND_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &second_spawn_args,
            ),
            ev_completed("policy-switch-root-initial"),
        ]),
    )
    .await;
    let root_after_spawns = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, POLICY_SWITCH_FIRST_SPAWN_CALL_ID)
                && request_has_function_call_output(request, POLICY_SWITCH_SECOND_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("policy-switch-root-after-spawns"),
            ev_assistant_message("policy-switch-root-waiting", "both Workers are running"),
            ev_completed("policy-switch-root-after-spawns"),
        ]),
    )
    .await;
    let root_after_policy_switch = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && body_contains(request, POLICY_SWITCH_FIRST_RESULT)
        },
        sse(vec![
            ev_response_created("policy-switch-root-after-policy"),
            ev_function_call(
                POLICY_SWITCH_RELEASE_SECOND_CALL_ID,
                "test_sync_tool",
                &lead_release_second_args,
            ),
            ev_completed("policy-switch-root-after-policy"),
        ]),
    )
    .await;
    let root_after_policy_switch_release = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, POLICY_SWITCH_RELEASE_SECOND_CALL_ID)
        },
        sse(vec![
            ev_response_created("policy-switch-root-after-worker-release"),
            ev_assistant_message(
                "policy-switch-root-after-worker-release-message",
                "the held Worker was released after the policy switch",
            ),
            ev_completed("policy-switch-root-after-worker-release"),
        ]),
    )
    .await;
    let root_after_second_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && body_contains(request, POLICY_SWITCH_SECOND_RESULT)
        },
        sse(vec![
            ev_response_created("policy-switch-root-after-second-completion"),
            ev_assistant_message(
                "policy-switch-root-after-second-completion-message",
                "the later prompt-guided completion also woke the Lead",
            ),
            ev_completed("policy-switch-root-after-second-completion"),
        ]),
    )
    .await;
    let first_worker = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, POLICY_SWITCH_FIRST_TASK)
                && request_has_model(request, WORKER_MODEL)
        },
        sse(vec![
            ev_response_created("policy-switch-first-worker"),
            ev_function_call(
                POLICY_SWITCH_FIRST_GATE_CALL_ID,
                "test_sync_tool",
                &first_gate_args,
            ),
            ev_completed("policy-switch-first-worker"),
        ]),
    )
    .await;
    let first_worker_after_gate = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, POLICY_SWITCH_FIRST_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("policy-switch-first-worker-result"),
            ev_assistant_message(
                "policy-switch-first-worker-result-message",
                POLICY_SWITCH_FIRST_RESULT,
            ),
            ev_completed("policy-switch-first-worker-result"),
        ]),
    )
    .await;
    let second_worker_gate = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, POLICY_SWITCH_SECOND_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, POLICY_SWITCH_SECOND_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("policy-switch-second-worker-gate"),
            ev_function_call(
                POLICY_SWITCH_SECOND_GATE_CALL_ID,
                "test_sync_tool",
                &second_gate_args,
            ),
            ev_completed("policy-switch-second-worker-gate"),
        ]),
    )
    .await;
    let second_worker_hold = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, POLICY_SWITCH_SECOND_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("policy-switch-second-worker-result"),
            ev_function_call(
                POLICY_SWITCH_SECOND_HOLD_CALL_ID,
                "test_sync_tool",
                &second_worker_hold_args,
            ),
            ev_completed("policy-switch-second-worker-result"),
        ]),
    )
    .await;
    let second_worker_result = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, POLICY_SWITCH_SECOND_HOLD_CALL_ID)
        },
        sse(vec![
            ev_response_created("policy-switch-second-worker-final-result"),
            ev_assistant_message(
                "policy-switch-second-worker-final-message",
                POLICY_SWITCH_SECOND_RESULT,
            ),
            ev_completed("policy-switch-second-worker-final-result"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_model_info_override(LEAD_MODEL, |model_info| {
            model_info.tool_mode = Some(ToolMode::Direct);
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
            model_info
                .experimental_supported_tools
                .push("test_sync_tool".to_string());
        })
        .with_model_info_override(WORKER_MODEL, |model_info| {
            model_info.tool_mode = Some(ToolMode::Direct);
            model_info.multi_agent_version = Some(MultiAgentVersion::V2);
            model_info
                .experimental_supported_tools
                .push("test_sync_tool".to_string());
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
                .lead_work_policy = TeamLeadWorkPolicy::ManagerOnly;
        })
        .build_with_auto_env(&server)
        .await?;

    submit_turn(
        &test.codex,
        POLICY_SWITCH_ROOT_PROMPT,
        ThreadSettingsOverrides::default(),
    )
    .await?;
    wait_for_captured_request(
        &root_initial,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && request.body_contains_text(POLICY_SWITCH_ROOT_PROMPT)
        },
        "Lead Worker delegation",
    )
    .await;
    wait_for_captured_request(
        &root_after_spawns,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(
                    request,
                    POLICY_SWITCH_FIRST_SPAWN_CALL_ID,
                )
                && response_request_has_function_call_output(
                    request,
                    POLICY_SWITCH_SECOND_SPAWN_CALL_ID,
                )
        },
        "Lead continuation after both Worker spawns",
    )
    .await;
    let first_worker_request = wait_for_captured_request(
        &first_worker,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && request.body_contains_text(POLICY_SWITCH_FIRST_TASK)
        },
        "first Worker completion",
    )
    .await;
    let first_worker_id = first_worker_request.body_json()["client_metadata"]["thread_id"]
        .as_str()
        .and_then(|thread_id| ThreadId::from_string(thread_id).ok())
        .expect("first Worker thread ID");
    let first_worker_thread = test.thread_manager.get_thread(first_worker_id).await?;

    let second_worker_request = wait_for_captured_request(
        &second_worker_gate,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && request.body_contains_text(POLICY_SWITCH_SECOND_TASK)
        },
        "second Worker held active in its tool call",
    )
    .await;
    let second_worker_id = second_worker_request.body_json()["client_metadata"]["thread_id"]
        .as_str()
        .and_then(|thread_id| ThreadId::from_string(thread_id).ok())
        .expect("second Worker thread ID");
    let second_worker_thread = test.thread_manager.get_thread(second_worker_id).await?;
    let second_worker_tools = second_worker_request
        .inputs_of_type("additional_tools")
        .into_iter()
        .next()
        .expect("Worker Responses Lite tool definitions")["tools"]
        .to_string();
    assert!(
        second_worker_tools.contains("exec_command"),
        "Workers should retain normal execution tools after the Lead policy change: {second_worker_tools}"
    );
    assert!(
        second_worker_tools.contains("test_sync_tool"),
        "Worker should have the test-only hold tool needed by this fixture: {second_worker_tools}"
    );

    // Worker 2 enters a second test barrier after the shared start barrier. The post-switch Lead
    // turn is the second participant, so Worker 2 stays Running until the policy change releases it.
    let _second_worker_hold_request = wait_for_captured_request(
        &second_worker_hold,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(
                    request,
                    POLICY_SWITCH_SECOND_GATE_CALL_ID,
                )
        },
        "second Worker waiting at the policy-switch release barrier",
    )
    .await;
    let second_worker_status_before_first_completion = second_worker_thread.agent_status().await;
    pretty_assertions::assert_eq!(
        second_worker_status_before_first_completion,
        codex_protocol::protocol::AgentStatus::Running,
        "second Worker should remain Running while it waits at the policy-switch barrier"
    );
    let second_worker_subtree_before_first_completion = test
        .thread_manager
        .list_open_agent_subtree_thread_ids(test.codex.id())
        .await?;
    assert!(
        second_worker_subtree_before_first_completion.contains(&second_worker_id),
        "second Worker should remain in the Lead's open subtree while its terminal response is delayed: {second_worker_subtree_before_first_completion:?}"
    );

    let _first_worker_result_request = wait_for_captured_request(
        &first_worker_after_gate,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(
                    request,
                    POLICY_SWITCH_FIRST_GATE_CALL_ID,
                )
        },
        "first Worker completion after both Workers reach the barrier",
    )
    .await;
    let second_worker_status_after_barrier = second_worker_thread.agent_status().await;
    let open_subtree_after_barrier = test
        .thread_manager
        .list_open_agent_subtree_thread_ids(test.codex.id())
        .await?;
    wait_for_completed_agent_message(&first_worker_thread, POLICY_SWITCH_FIRST_RESULT).await;
    let second_worker_status_after_first_completion = second_worker_thread.agent_status().await;

    // Let the quiet-window callback observe the second Worker while it waits at the test barrier.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let requests_before_policy_switch = root_after_policy_switch.requests();
    let second_worker_status_at_checkpoint = second_worker_thread.agent_status().await;
    let open_subtree_at_checkpoint = test
        .thread_manager
        .list_open_agent_subtree_thread_ids(test.codex.id())
        .await?;
    let premature_lead_requests = requests_before_policy_switch
        .iter()
        .filter(|request| {
            response_request_has_model(request, LEAD_MODEL)
                && request.body_contains_text(POLICY_SWITCH_FIRST_RESULT)
        })
        .map(|request| {
            let matching_inputs = request
                .input()
                .into_iter()
                .filter(|item| item.to_string().contains(POLICY_SWITCH_FIRST_RESULT))
                .collect::<Vec<_>>();
            json!({
                "thread_id": request.body_json()["client_metadata"]["thread_id"],
                "model": request.body_json()["model"],
                "matching_inputs": matching_inputs,
            })
        })
        .collect::<Vec<_>>();
    pretty_assertions::assert_eq!(
        second_worker_status_at_checkpoint,
        codex_protocol::protocol::AgentStatus::Running,
        "second Worker should still be Running at the manager-only batching checkpoint"
    );
    assert!(
        open_subtree_at_checkpoint.contains(&second_worker_id),
        "second Worker should remain in the Lead's open subtree at the manager-only batching checkpoint: {open_subtree_at_checkpoint:?}"
    );
    assert!(
        premature_lead_requests.is_empty(),
        "ManagerOnly should keep the first Worker completion buffered while the second Worker is active; second_worker_id={second_worker_id}, status_before_first_completion={second_worker_status_before_first_completion:?}, subtree_before_first_completion={second_worker_subtree_before_first_completion:?}, status_after_barrier={second_worker_status_after_barrier:?}, status_after_first_completion={second_worker_status_after_first_completion:?}, status_at_checkpoint={second_worker_status_at_checkpoint:?}, open_subtree_after_barrier={open_subtree_after_barrier:?}, open_subtree_at_checkpoint={open_subtree_at_checkpoint:?}, premature_lead_requests={premature_lead_requests:#?}"
    );
    let request_count_before_policy_switch = requests_before_policy_switch.len();

    submit_thread_settings(
        &test.codex,
        lead_work_policy_update(TeamLeadWorkPolicy::PromptGuided),
    )
    .await?;
    let applied_snapshot = test.codex.config_snapshot().await;
    let applied_team = applied_snapshot.team.as_ref().expect("team snapshot");
    pretty_assertions::assert_eq!(
        applied_team.lead_work_policy,
        Some(codex_protocol::protocol::TeamLeadWorkPolicy::PromptGuided)
    );
    let released_request = wait_for_captured_request(
        &root_after_policy_switch,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && request.body_contains_text(POLICY_SWITCH_FIRST_RESULT)
        },
        "buffered Worker completion released by the policy switch",
    )
    .await;
    let requests_after_policy_switch = root_after_policy_switch.requests();
    assert!(
        requests_after_policy_switch
            .iter()
            .skip(request_count_before_policy_switch)
            .any(|request| {
                response_request_has_model(request, LEAD_MODEL)
                    && request.body_contains_text(POLICY_SWITCH_FIRST_RESULT)
            }),
        "buffered completion request should be captured after the policy-switch baseline"
    );
    let _root_after_release_request = wait_for_captured_request(
        &root_after_policy_switch_release,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(
                    request,
                    POLICY_SWITCH_RELEASE_SECOND_CALL_ID,
                )
        },
        "Lead continuation after releasing the held Worker",
    )
    .await;
    let _second_worker_result_request = wait_for_captured_request(
        &second_worker_result,
        |request| {
            response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(
                    request,
                    POLICY_SWITCH_SECOND_HOLD_CALL_ID,
                )
        },
        "second Worker result after the policy-switch release barrier",
    )
    .await;
    wait_for_completed_agent_message(&second_worker_thread, POLICY_SWITCH_SECOND_RESULT).await;
    let lead_tools = released_request
        .inputs_of_type("additional_tools")
        .into_iter()
        .next()
        .expect("prompt-guided Lead tool definitions")["tools"]
        .to_string();
    assert!(
        lead_tools.contains("exec_command"),
        "prompt_guided Lead should regain execution tools after the switch: {lead_tools}"
    );
    assert!(
        lead_tools.contains("test_sync_tool"),
        "prompt_guided Lead should have the test-only release tool needed by this fixture: {lead_tools}"
    );
    let second_lead_wake = wait_for_captured_request_with_timeout(
        &root_after_second_completion,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && request.body_contains_text(POLICY_SWITCH_SECOND_RESULT)
        },
        "prompt-guided Worker completion wake",
        std::time::Duration::from_secs(/*secs*/ 7),
    )
    .await;
    assert!(second_lead_wake.body_contains_text(POLICY_SWITCH_SECOND_RESULT));
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
