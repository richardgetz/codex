use super::*;
use codex_extension_items::ExtensionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use core_test_support::responses::ev_function_call;
use pretty_assertions::assert_eq;

const QUESTION_ROOT_PROMPT: &str = "spawn a Worker before asking its question";
const QUESTION_GATE_RELEASE_PROMPT: &str = "release the Worker’s initial answer gate";
const QUESTION_WORKER_TASK: &str = "QUESTION_WORKER_TASK_MARKER keep working after the answer";
const QUESTION_INITIAL_ANSWER: &str = "I am continuing the assigned task.";
const QUESTION_ASK_PROMPT: &str = "ask the active Worker a question";
const QUESTION_SPAWN_CALL_ID: &str = "worker-question-spawn";
const QUESTION_WORKER_GATE_CALL_ID: &str = "worker-question-initial-gate";
const QUESTION_ROOT_GATE_CALL_ID: &str = "worker-question-root-gate";
const QUESTION_ASK_CALL_ID: &str = "worker-question-ask";
const QUESTION_SLEEP_CALL_ID: &str = "worker-question-sleep";
const QUESTION_GATE_BARRIER_ID: &str = "worker-question-initial-answer";
const QUESTION_ANSWER: &str = "The answer is 42.";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_worker_question_reply_precedes_completion_for_v1_and_v2() -> Result<()> {
    skip_if_no_network!(Ok(()));

    run_question_flow(MultiAgentVersion::V1).await?;
    run_question_flow(MultiAgentVersion::V2).await
}

async fn run_question_flow(version: MultiAgentVersion) -> Result<()> {
    let server = start_mock_server().await;
    let namespace = match version {
        MultiAgentVersion::V1 => MULTI_AGENT_V1_NAMESPACE,
        MultiAgentVersion::V2 => MULTI_AGENT_V2_NAMESPACE,
        MultiAgentVersion::Disabled => unreachable!(),
    };
    let spawn_args = match version {
        MultiAgentVersion::V1 => json!({
            "message": QUESTION_WORKER_TASK,
            "fork_turns": "none"
        }),
        MultiAgentVersion::V2 => json!({
            "task_name": "question_worker",
            "message": QUESTION_WORKER_TASK,
            "fork_turns": "none"
        }),
        MultiAgentVersion::Disabled => unreachable!(),
    };
    let gate_args = serde_json::to_string(&json!({
        "barrier": {
            "id": QUESTION_GATE_BARRIER_ID,
            "participants": 2,
            "timeout_ms": 30_000,
        },
    }))?;

    let _root_spawn = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, QUESTION_ROOT_PROMPT),
        sse(vec![
            ev_response_created("worker-question-root-spawn"),
            ev_function_call_with_namespace(
                QUESTION_SPAWN_CALL_ID,
                namespace,
                "spawn_agent",
                &spawn_args.to_string(),
            ),
            ev_completed("worker-question-root-spawn"),
        ]),
    )
    .await;
    let _root_after_spawn = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_function_call_output(request, QUESTION_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("worker-question-root-after-spawn"),
            ev_assistant_message("worker-question-spawned", "Worker started."),
            ev_completed("worker-question-root-after-spawn"),
        ]),
    )
    .await;
    let worker_initial = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL) && body_contains(request, QUESTION_WORKER_TASK)
        },
        sse_response(sse(vec![
            ev_response_created("worker-question-worker-initial"),
            ev_function_call(QUESTION_WORKER_GATE_CALL_ID, "test_sync_tool", &gate_args),
            ev_completed("worker-question-worker-initial"),
        ])),
    )
    .await;
    let worker_after_gate = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, QUESTION_WORKER_GATE_CALL_ID)
        },
        sse_response(sse(vec![
            ev_response_created("worker-question-worker-after-gate"),
            ev_assistant_message(
                "worker-question-worker-initial-message",
                QUESTION_INITIAL_ANSWER,
            ),
            ev_completed("worker-question-worker-after-gate"),
        ])),
    )
    .await;
    let worker_continuous = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && body_contains(request, "Scratchpad continuous run policy is enabled")
        },
        sse(vec![
            ev_response_created("worker-question-worker-continuous"),
            ev_function_call_with_namespace(
                QUESTION_SLEEP_CALL_ID,
                "clock",
                "sleep",
                &json!({"duration_ms": 600_000}).to_string(),
            ),
            ev_completed("worker-question-worker-continuous"),
        ]),
    )
    .await;
    let _root_gate_release = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && body_contains(request, QUESTION_GATE_RELEASE_PROMPT)
        },
        sse(vec![
            ev_response_created("worker-question-root-gate"),
            ev_function_call(QUESTION_ROOT_GATE_CALL_ID, "test_sync_tool", &gate_args),
            ev_completed("worker-question-root-gate"),
        ]),
    )
    .await;
    let _root_after_gate = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, QUESTION_ROOT_GATE_CALL_ID)
        },
        sse(vec![
            ev_response_created("worker-question-root-after-gate"),
            ev_assistant_message("worker-question-gate-released", "Worker answer released."),
            ev_completed("worker-question-root-after-gate"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model(INITIAL_MODEL)
        .with_model_info_override(LEAD_MODEL, move |model_info| {
            model_info.tool_mode = Some(ToolMode::Direct);
            model_info.supports_search_tool = false;
            model_info.multi_agent_version = Some(version);
            model_info
                .experimental_supported_tools
                .push("test_sync_tool".to_string());
        })
        .with_model_info_override(WORKER_MODEL, move |model_info| {
            model_info.multi_agent_version = Some(version);
            model_info
                .experimental_supported_tools
                .push("clock".to_string());
            model_info
                .experimental_supported_tools
                .push("test_sync_tool".to_string());
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("Collab feature");
            match version {
                MultiAgentVersion::V1 => {
                    config
                        .features
                        .disable(Feature::MultiAgentV2)
                        .expect("disable MultiAgentV2 feature");
                }
                MultiAgentVersion::V2 => {
                    config
                        .features
                        .enable(Feature::MultiAgentV2)
                        .expect("MultiAgentV2 feature");
                }
                MultiAgentVersion::Disabled => unreachable!(),
            }
            configure_team(config, TeamMode::LeadWorker);
            config
                .team
                .profiles
                .as_mut()
                .expect("team profiles")
                .lead_work_policy = codex_config::TeamLeadWorkPolicy::ManagerOnly;
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: QUESTION_ROOT_PROMPT.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(Default::default()),
        )
        .await?;
    let worker_initial_request = wait_for_captured_request(
        &worker_initial,
        |request| request.body_contains_text(QUESTION_WORKER_TASK),
        "Worker initial response",
    )
    .await;

    let worker_thread_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|thread_id| *thread_id != test.session_configured.thread_id)
        .ok_or_else(|| anyhow::anyhow!("spawned Worker thread id was not registered"))?;
    let worker_thread = test.thread_manager.get_thread(worker_thread_id).await?;
    write_worker_continuous_scratchpad(
        &test,
        worker_thread_id,
        &["keep working on the assigned task"],
    )
    .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    // The Lead joins only after scratchpad setup; the barrier holds the Worker until then.
    submit_turn(
        &test.codex,
        QUESTION_GATE_RELEASE_PROMPT,
        Default::default(),
    )
    .await?;
    let worker_after_gate_request = wait_for_captured_request(
        &worker_after_gate,
        |request| {
            request
                .function_call_output_text(QUESTION_WORKER_GATE_CALL_ID)
                .is_some()
        },
        "Worker initial answer",
    )
    .await;
    let worker_continuous_request = wait_for_captured_request(
        &worker_continuous,
        |request| request.body_contains_text("Scratchpad continuous run policy is enabled"),
        "Worker continuous response",
    )
    .await;
    wait_for_event(worker_thread.as_ref(), |event| {
        matches!(
            event,
            EventMsg::ItemStarted(started)
                if matches!(
                    &started.item,
                    TurnItem::Extension(ExtensionItem::Sleep(item))
                        if item.id == QUESTION_SLEEP_CALL_ID
                )
        )
    })
    .await;
    let worker_answer = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL) && body_contains(request, "<worker_question>")
        },
        sse(vec![
            ev_response_created("worker-question-worker-answer"),
            ev_assistant_message("worker-question-answer", QUESTION_ANSWER),
            ev_completed("worker-question-worker-answer"),
        ]),
    )
    .await;
    let worker_continue = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, WORKER_MODEL)
                && body_contains(request, "<worker_question_answered>")
        },
        sse_response(sse(vec![
            ev_response_created("worker-question-worker-continued"),
            ev_assistant_message("worker-question-task-complete", "Assigned task complete."),
            ev_completed("worker-question-worker-continued"),
        ]))
        .set_delay(Duration::from_secs(2)),
    )
    .await;
    let root_ask = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, QUESTION_ASK_PROMPT),
        sse(vec![
            ev_response_created("worker-question-root-ask"),
            ev_function_call(
                QUESTION_ASK_CALL_ID,
                "ask_worker_question",
                &json!({
                    "target": worker_thread_id.to_string(),
                    "question": "What result did you find?"
                })
                .to_string(),
            ),
            ev_completed("worker-question-root-ask"),
        ]),
    )
    .await;
    let root_after_ask = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_function_call_output(request, QUESTION_ASK_CALL_ID)
        },
        sse(vec![
            ev_response_created("worker-question-root-after-ask"),
            ev_assistant_message("worker-question-asked", "Question sent."),
            ev_completed("worker-question-root-after-ask"),
        ]),
    )
    .await;
    let root_reply = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "<worker_question_reply>"),
        sse(vec![
            ev_response_created("worker-question-root-reply"),
            ev_assistant_message("worker-question-reply-received", "Reply received."),
            ev_completed("worker-question-root-reply"),
        ]),
    )
    .await;
    // V1 uses the legacy terminal-status wake; V2 batches successful completion under ManagerOnly.
    let completion_notification = match version {
        MultiAgentVersion::V1 => "completed with status",
        MultiAgentVersion::V2 => "The direct Workers have finished",
        MultiAgentVersion::Disabled => unreachable!(),
    };
    let root_completion = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| body_contains(request, completion_notification),
        sse(vec![
            ev_response_created("worker-question-root-completion"),
            ev_assistant_message("worker-question-completion-reviewed", "Task complete."),
            ev_completed("worker-question-root-completion"),
        ]),
    )
    .await;

    submit_turn(&test.codex, QUESTION_ASK_PROMPT, Default::default()).await?;
    let ask_request = wait_for_captured_request(
        &root_ask,
        |request| request.body_contains_text(QUESTION_ASK_PROMPT),
        "Lead question ask",
    )
    .await;
    let ask_output_request = wait_for_captured_request(
        &root_after_ask,
        |request| {
            request
                .function_call_output_text(QUESTION_ASK_CALL_ID)
                .is_some()
        },
        "accepted Worker question",
    )
    .await;
    let ask_tool_output = ask_output_request.function_call_output(QUESTION_ASK_CALL_ID);
    let ask_output = ask_tool_output["output"]
        .as_str()
        .expect("ask tool returns its question id");
    let ask_output: serde_json::Value = serde_json::from_str(ask_output)
        .map_err(|error| anyhow::anyhow!("ask_worker_question returned {ask_output:?}: {error}"))?;
    let question_id = ask_output["question_id"]
        .as_str()
        .expect("ask output includes question_id")
        .to_owned();
    assert_eq!(ask_output["target"], worker_thread_id.to_string());
    let worker_answer_request = wait_for_captured_request(
        &worker_answer,
        |request| request.body_contains_text("<worker_question>"),
        "Worker question answer",
    )
    .await;
    let worker_answer_input = worker_answer_request.body_json()["input"].to_string();
    assert!(
        worker_answer_input.contains(&question_id),
        "Worker question request did not carry its question ID: {worker_answer_input}"
    );
    let reply_request = wait_for_captured_request(
        &root_reply,
        |request| request.body_contains_text("<worker_question_reply>"),
        "Lead question reply",
    )
    .await;
    let worker_continue_request = wait_for_captured_request(
        &worker_continue,
        |request| request.body_contains_text("<worker_question_answered>"),
        "Worker task continuation",
    )
    .await;
    assert_eq!(worker_thread.agent_status().await, AgentStatus::Running);
    assert!(
        !root_completion
            .requests()
            .iter()
            .any(|request| request.body_contains_text(completion_notification))
    );
    write_worker_continuous_scratchpad(&test, worker_thread_id, &[]).await?;
    wait_for_event(worker_thread.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(matches!(
        worker_thread.agent_status().await,
        AgentStatus::Completed(_)
    ));
    let completion_request = wait_for_captured_request(
        &root_completion,
        |request| request.body_contains_text(completion_notification),
        "Worker completion notification",
    )
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let request_body = ask_request.body_json();
    let serialized_tools = request_body["tools"]
        .as_array()
        .or_else(|| {
            request_body["input"]
                .as_array()?
                .iter()
                .find(|item| item["type"] == "additional_tools")?["tools"]
                .as_array()
        })
        .cloned()
        .expect("outbound request contains serialized tools");
    let function_tools = serialized_tools
        .iter()
        .find(|tool| tool["name"] == "functions" && tool["type"] == "namespace")
        .map(|tool| tool["tools"].as_array().expect("functions namespace tools"))
        .unwrap_or(&serialized_tools);
    let ask_tool = function_tools
        .iter()
        .find(|tool| tool["name"] == "ask_worker_question")
        .expect("question helper is serialized as a standalone function");
    assert_eq!(
        ask_tool["parameters"]["required"],
        json!(["target", "question"])
    );
    assert_eq!(
        ask_tool["parameters"]["properties"]["target"]["type"],
        "string"
    );
    assert_eq!(
        ask_tool["parameters"]["properties"]["question"]["type"],
        "string"
    );
    let namespace_tool = serialized_tools
        .iter()
        .find(|tool| tool["name"] == namespace)
        .expect("reserved multi-agent namespace remains present");
    let namespace_tools = namespace_tool["tools"].as_array().expect("namespace tools");
    assert!(
        namespace_tools
            .iter()
            .all(|tool| tool["name"] != "ask_worker_question")
    );
    let mut namespace_tool_names = namespace_tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("namespace tool name"))
        .collect::<Vec<_>>();
    namespace_tool_names.sort_unstable();
    let expected_namespace_tool_names = match version {
        MultiAgentVersion::V1 => vec![
            "close_agent",
            "resume_agent",
            "send_input",
            "spawn_agent",
            "wait_agent",
            "worker_capacity",
        ],
        MultiAgentVersion::V2 => vec![
            "followup_task",
            "interrupt_agent",
            "list_agents",
            "send_message",
            "spawn_agent",
            "wait_agent",
        ],
        MultiAgentVersion::Disabled => unreachable!(),
    };
    assert_eq!(namespace_tool_names, expected_namespace_tool_names);
    assert!(reply_request.body_contains_text(&question_id));
    assert!(reply_request.body_contains_text(QUESTION_ANSWER));
    assert!(worker_answer_request.body_contains_text("<worker_question>"));
    assert!(worker_answer_request.body_contains_text(&question_id));
    assert!(reply_request.body_contains_text(&question_id));
    assert!(worker_initial_request.body_contains_text(QUESTION_WORKER_TASK));
    assert!(
        worker_after_gate_request
            .function_call_output_text(QUESTION_WORKER_GATE_CALL_ID)
            .is_some()
    );
    assert!(
        worker_continuous_request.body_contains_text("Scratchpad continuous run policy is enabled")
    );
    assert!(worker_continue_request.body_contains_text("<worker_question_answered>"));
    assert!(completion_request.body_contains_text(completion_notification));
    let completion_count = root_completion
        .requests()
        .iter()
        .filter(|request| request.body_contains_text(completion_notification))
        .count();
    assert_eq!(completion_count, 1);
    Ok(())
}

async fn write_worker_continuous_scratchpad(
    test: &core_test_support::test_codex::TestCodex,
    worker_thread_id: ThreadId,
    next_steps: &[&str],
) -> Result<()> {
    let scratchpad_id = worker_thread_id.to_string();
    let entries_dir = test.codex_home_path().join("scratchpad").join("entries");
    tokio::fs::create_dir_all(&entries_dir).await?;
    let scratchpad = json!({
        "scratchpad_id": scratchpad_id,
        "origin_thread_id": scratchpad_id,
        "status": "active",
        "run_policy": {
            "continuous": {
                "enabled": true
            }
        },
        "next_steps": next_steps
    });
    tokio::fs::write(
        entries_dir.join(format!("{scratchpad_id}.json")),
        serde_json::to_vec(&scratchpad)?,
    )
    .await?;
    Ok(())
}
