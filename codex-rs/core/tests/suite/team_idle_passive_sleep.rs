use super::*;
use codex_extension_items::ExtensionItem;

const PASSIVE_ROOT_PROMPT: &str = "check worker status without polling while it runs";
const PASSIVE_CHILD_TASK: &str = "stay active during the lead status check";
const PASSIVE_SPAWN_CALL_ID: &str = "team-passive-spawn";
const PASSIVE_FIRST_SLEEP_CALL_ID: &str = "team-passive-first-sleep";
const PASSIVE_LIST_CALL_ID: &str = "team-passive-list-agents";
const PASSIVE_SECOND_SLEEP_CALL_ID: &str = "team-passive-second-sleep";
const PASSIVE_WORKER_SLEEP_CALL_ID: &str = "team-passive-worker-sleep";

#[tokio::test(flavor = "current_thread")]
async fn lead_parks_repeated_sleep_after_worker_status_until_configured_oversight() -> Result<()> {
    run_passive_sleep_flow(codex_config::TeamLeadWorkPolicy::PromptGuided).await
}

#[tokio::test(flavor = "current_thread")]
async fn manager_only_lead_parks_repeated_sleep_after_worker_status_until_configured_oversight()
-> Result<()> {
    run_passive_sleep_flow(codex_config::TeamLeadWorkPolicy::ManagerOnly).await
}

async fn run_passive_sleep_flow(policy: codex_config::TeamLeadWorkPolicy) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": PASSIVE_CHILD_TASK,
        "task_name": "passive_poll_worker",
        "fork_turns": "none",
    }))?;
    let worker_sleep_args = serde_json::to_string(&json!({ "sleep_after_ms": 600_000 }))?;
    let first_sleep_args = serde_json::to_string(&json!({ "duration_ms": 55_000 }))?;
    let passive_sleep_args = serde_json::to_string(&json!({ "duration_ms": 55_000 }))?;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PASSIVE_ROOT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, PASSIVE_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-passive-root-1"),
            ev_function_call_with_namespace(
                PASSIVE_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("team-passive-root-1"),
        ]),
    )
    .await;
    let _root_first_sleep = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, PASSIVE_SPAWN_CALL_ID)
                && !request_has_function_call_output(request, PASSIVE_FIRST_SLEEP_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-passive-root-2"),
            ev_function_call_with_namespace(
                PASSIVE_FIRST_SLEEP_CALL_ID,
                "clock",
                "sleep",
                &first_sleep_args,
            ),
            ev_completed("team-passive-root-2"),
        ]),
    )
    .await;
    let _root_status_probe = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, PASSIVE_FIRST_SLEEP_CALL_ID)
                && !request_has_function_call_output(request, PASSIVE_LIST_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-passive-root-3"),
            ev_function_call_with_namespace(
                PASSIVE_LIST_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "list_agents",
                "{}",
            ),
            ev_completed("team-passive-root-3"),
        ]),
    )
    .await;
    let _root_second_sleep = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, PASSIVE_LIST_CALL_ID)
                && !request_has_function_call_output(request, PASSIVE_SECOND_SLEEP_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-passive-root-4"),
            ev_function_call_with_namespace(
                PASSIVE_SECOND_SLEEP_CALL_ID,
                "clock",
                "sleep",
                &passive_sleep_args,
            ),
            ev_completed("team-passive-root-4"),
        ]),
    )
    .await;
    let root_after_deadline = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, PASSIVE_SECOND_SLEEP_CALL_ID)
                && body_contains(request, "Lead oversight deadline reached")
        },
        sse(vec![
            ev_response_created("team-passive-root-5"),
            ev_assistant_message("team-passive-root-message", "oversight wake received"),
            ev_completed("team-passive-root-5"),
        ]),
    )
    .await;
    let _worker_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PASSIVE_CHILD_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, PASSIVE_WORKER_SLEEP_CALL_ID)
        },
        sse(vec![
            ev_response_created("team-passive-worker-1"),
            ev_function_call(
                PASSIVE_WORKER_SLEEP_CALL_ID,
                "test_sync_tool",
                &worker_sleep_args,
            ),
            ev_completed("team-passive-worker-1"),
        ]),
    )
    .await;

    let mut builder = test_codex()
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
        .with_config(move |config| {
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
                .lead_work_policy = policy;
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
            text: PASSIVE_ROOT_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let first_sleep_started = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::ItemStarted(started)
                if matches!(
                    &started.item,
                    TurnItem::Extension(ExtensionItem::Sleep(item))
                        if item.id == PASSIVE_FIRST_SLEEP_CALL_ID
                )
        )
    })
    .await;
    let EventMsg::ItemStarted(started) = first_sleep_started else {
        unreachable!("event predicate only accepts item/started events");
    };
    let TurnItem::Extension(ExtensionItem::Sleep(first_sleep)) = started.item else {
        unreachable!("event predicate only accepts sleep items");
    };
    pretty_assertions::assert_eq!(first_sleep.duration_ms, 55_000);

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(55)).await;
    tokio::time::resume();

    let second_sleep_started = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::ItemStarted(started)
                if matches!(
                    &started.item,
                    TurnItem::Extension(ExtensionItem::Sleep(item))
                        if item.id == PASSIVE_SECOND_SLEEP_CALL_ID
                )
        )
    })
    .await;
    let EventMsg::ItemStarted(started) = second_sleep_started else {
        unreachable!("event predicate only accepts item/started events");
    };
    let TurnItem::Extension(ExtensionItem::Sleep(passive_sleep)) = started.item else {
        unreachable!("event predicate only accepts sleep items");
    };
    assert!(
        (59_000..=60_000).contains(&passive_sleep.duration_ms),
        "the detected poll should park for the configured one-minute oversight interval, got {}ms",
        passive_sleep.duration_ms
    );

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(59)).await;
    tokio::task::yield_now().await;
    assert!(
        root_after_deadline.requests().iter().all(|request| {
            !response_request_has_function_call_output(request, PASSIVE_SECOND_SLEEP_CALL_ID)
        }),
        "the passive poll must remain parked before the configured deadline"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();

    let _ = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::Warning(warning)
                if warning.message.contains("Lead oversight deadline reached")
        )
    })
    .await;
    let deadline_request = wait_for_captured_request(
        &root_after_deadline,
        |request| {
            response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, PASSIVE_SECOND_SLEEP_CALL_ID)
                && request.body_contains_text("Lead oversight deadline reached")
        },
        "Lead passive-poll oversight wake",
    )
    .await;
    assert!(deadline_request.body_contains_text("Lead oversight deadline reached"));
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}
