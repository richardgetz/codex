use super::*;

use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageProjectionThread;
use core_test_support::responses::ev_completed_with_tokens;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use test_case::test_case;

const USAGE_ROOT_PROMPT: &str = "project the team's usage tree";
const USAGE_CHILD_TASK: &str = "measure the worker's usage";
const USAGE_GRANDCHILD_TASK: &str = "measure the nested worker's usage";
const USAGE_ROOT_SPAWN_CALL_ID: &str = "usage-root-spawn";
const USAGE_CHILD_SPAWN_CALL_ID: &str = "usage-child-spawn";

const USAGE_ROOT_INITIAL_RESPONSE: &str = "usage-root-initial";
const USAGE_ROOT_COMPLETION_RESPONSE: &str = "usage-root-completion";
const USAGE_CHILD_INITIAL_RESPONSE: &str = "usage-child-initial";
const USAGE_CHILD_COMPLETION_RESPONSE: &str = "usage-child-completion";
const USAGE_GRANDCHILD_RESPONSE: &str = "usage-grandchild";

const USAGE_ROOT_INITIAL_TOKENS: i64 = 101;
const USAGE_ROOT_COMPLETION_TOKENS: i64 = 103;
const USAGE_CHILD_INITIAL_TOKENS: i64 = 211;
const USAGE_CHILD_COMPLETION_TOKENS: i64 = 223;
const USAGE_GRANDCHILD_TOKENS: i64 = 307;

fn usage(total_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens: total_tokens,
        total_tokens,
        ..TokenUsage::default()
    }
}
fn usage_summary(
    thread: &TokenUsageProjectionThread,
) -> (
    ThreadId,
    Option<ThreadId>,
    Option<ThreadId>,
    Vec<(TokenUsage, Vec<String>)>,
    Vec<String>,
) {
    (
        thread.thread_id,
        thread.parent_thread_id,
        thread.forked_from_id,
        thread
            .sources
            .iter()
            .map(|source| (source.usage.clone(), source.response_ids.clone()))
            .collect(),
        thread
            .response_ids
            .iter()
            .map(|identity| identity.response_id.clone())
            .collect(),
    )
}

fn latest_user_message_text(request: &wiremock::Request) -> Option<String> {
    request_body(request)?
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find_map(|item| {
            (item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("user"))
            .then(|| item.get("content").and_then(Value::as_array))
            .flatten()
            .and_then(|content| {
                content.iter().find_map(|part| {
                    (part.get("type").and_then(Value::as_str) == Some("input_text"))
                        .then(|| part.get("text").and_then(Value::as_str))
                        .flatten()
                        .map(str::to_owned)
                })
            })
        })
}

#[test_case(MultiAgentVersion::V1, MULTI_AGENT_V1_NAMESPACE; "legacy lead with v1 worker")]
#[test_case(MultiAgentVersion::V2, MULTI_AGENT_V2_NAMESPACE; "v2 lead with v1 worker")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_usage_projection_reconstructs_recursive_worker_sources(
    lead_multi_agent_version: MultiAgentVersion,
    tool_namespace: &str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let child_spawn_args = match lead_multi_agent_version {
        MultiAgentVersion::V1 => json!({
            "message": USAGE_CHILD_TASK,
            "fork_context": false,
        }),
        MultiAgentVersion::V2 => json!({
            "message": USAGE_CHILD_TASK,
            "task_name": "usage_worker",
            "fork_turns": "none",
        }),
        MultiAgentVersion::Disabled => {
            unreachable!("team usage cases require an enabled multi-agent version")
        }
    }
    .to_string();
    let grandchild_spawn_args = match lead_multi_agent_version {
        MultiAgentVersion::V1 => json!({
            "message": USAGE_GRANDCHILD_TASK,
            "fork_context": true,
        }),
        MultiAgentVersion::V2 => json!({
            "message": USAGE_GRANDCHILD_TASK,
            "task_name": "usage_grandchild",
            "fork_turns": "all",
        }),
        MultiAgentVersion::Disabled => {
            unreachable!("team usage cases require an enabled multi-agent version")
        }
    }
    .to_string();

    let root_initial_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            latest_user_message_text(request).as_deref() == Some(USAGE_ROOT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && !request_has_function_call_output(request, USAGE_ROOT_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created(USAGE_ROOT_INITIAL_RESPONSE),
            ev_function_call_with_namespace(
                USAGE_ROOT_SPAWN_CALL_ID,
                tool_namespace,
                "spawn_agent",
                &child_spawn_args,
            ),
            ev_completed_with_tokens(USAGE_ROOT_INITIAL_RESPONSE, USAGE_ROOT_INITIAL_TOKENS),
        ]),
    )
    .await;
    let child_initial_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            latest_user_message_text(request).as_deref() == Some(USAGE_CHILD_TASK)
                && request_has_model(request, WORKER_MODEL)
                && !request_has_function_call_output(request, USAGE_CHILD_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created(USAGE_CHILD_INITIAL_RESPONSE),
            ev_function_call_with_namespace(
                USAGE_CHILD_SPAWN_CALL_ID,
                tool_namespace,
                "spawn_agent",
                &grandchild_spawn_args,
            ),
            ev_completed_with_tokens(USAGE_CHILD_INITIAL_RESPONSE, USAGE_CHILD_INITIAL_TOKENS),
        ]),
    )
    .await;
    let grandchild_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            latest_user_message_text(request).as_deref() == Some(USAGE_GRANDCHILD_TASK)
                && request_has_model(request, WORKER_MODEL)
        },
        sse(vec![
            ev_response_created(USAGE_GRANDCHILD_RESPONSE),
            ev_assistant_message("usage-grandchild-message", "nested usage recorded"),
            ev_completed_with_tokens(USAGE_GRANDCHILD_RESPONSE, USAGE_GRANDCHILD_TOKENS),
        ]),
    )
    .await;
    let child_completion_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            latest_user_message_text(request).as_deref() == Some(USAGE_CHILD_TASK)
                && request_has_model(request, WORKER_MODEL)
                && request_has_function_call_output(request, USAGE_CHILD_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created(USAGE_CHILD_COMPLETION_RESPONSE),
            ev_assistant_message("usage-child-message", "worker usage recorded"),
            ev_completed_with_tokens(
                USAGE_CHILD_COMPLETION_RESPONSE,
                USAGE_CHILD_COMPLETION_TOKENS,
            ),
        ]),
    )
    .await;
    let root_completion_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            latest_user_message_text(request).as_deref() == Some(USAGE_ROOT_PROMPT)
                && request_has_model(request, LEAD_MODEL)
                && request_has_function_call_output(request, USAGE_ROOT_SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created(USAGE_ROOT_COMPLETION_RESPONSE),
            ev_assistant_message("usage-root-message", "root usage recorded"),
            ev_completed_with_tokens(USAGE_ROOT_COMPLETION_RESPONSE, USAGE_ROOT_COMPLETION_TOKENS),
        ]),
    )
    .await;

    let builder = test_codex()
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
        });
    let test = builder.build_with_auto_env(&server).await?;
    let root_thread_id = test.codex.id();
    submit_turn(
        &test.codex,
        USAGE_ROOT_PROMPT,
        ThreadSettingsOverrides::default(),
    )
    .await?;

    let root_request = wait_for_captured_request(
        &root_initial_response,
        |request| {
            request
                .message_input_texts("user")
                .last()
                .is_some_and(|text| text == USAGE_ROOT_PROMPT)
                && response_request_has_model(request, LEAD_MODEL)
                && !response_request_has_function_call_output(request, USAGE_ROOT_SPAWN_CALL_ID)
        },
        "usage projection root",
    )
    .await;
    let child_request = wait_for_captured_request(
        &child_initial_response,
        |request| {
            request
                .message_input_texts("user")
                .last()
                .is_some_and(|text| text == USAGE_CHILD_TASK)
                && response_request_has_model(request, WORKER_MODEL)
                && !response_request_has_function_call_output(request, USAGE_CHILD_SPAWN_CALL_ID)
        },
        "usage projection child",
    )
    .await;
    let grandchild_request = wait_for_captured_request(
        &grandchild_response,
        |request| {
            request
                .message_input_texts("user")
                .last()
                .is_some_and(|text| text == USAGE_GRANDCHILD_TASK)
                && response_request_has_model(request, WORKER_MODEL)
        },
        "usage projection grandchild",
    )
    .await;
    let _child_completion_request = wait_for_captured_request(
        &child_completion_response,
        |request| {
            request
                .message_input_texts("user")
                .last()
                .is_some_and(|text| text == USAGE_CHILD_TASK)
                && response_request_has_model(request, WORKER_MODEL)
                && response_request_has_function_call_output(request, USAGE_CHILD_SPAWN_CALL_ID)
        },
        "usage projection child completion",
    )
    .await;
    let _root_completion_request = wait_for_captured_request(
        &root_completion_response,
        |request| {
            request
                .message_input_texts("user")
                .last()
                .is_some_and(|text| text == USAGE_ROOT_PROMPT)
                && response_request_has_model(request, LEAD_MODEL)
                && response_request_has_function_call_output(request, USAGE_ROOT_SPAWN_CALL_ID)
        },
        "usage projection root completion",
    )
    .await;

    let child_thread_id = child_request
        .body_json()
        .get("client_metadata")
        .and_then(|metadata| metadata.get("thread_id"))
        .and_then(Value::as_str)
        .and_then(|id| ThreadId::from_string(id).ok())
        .unwrap_or_else(|| {
            panic!(
                "child request missing a valid thread ID: {:?}",
                child_request.body_json()
            )
        });
    let grandchild_thread_id = grandchild_request
        .body_json()
        .get("client_metadata")
        .and_then(|metadata| metadata.get("thread_id"))
        .and_then(Value::as_str)
        .and_then(|id| ThreadId::from_string(id).ok())
        .unwrap_or_else(|| {
            panic!(
                "grandchild request missing a valid thread ID: {:?}",
                grandchild_request.body_json()
            )
        });
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    let grandchild_thread = test.thread_manager.get_thread(grandchild_thread_id).await?;
    wait_for_event(grandchild_thread.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    wait_for_event(child_thread.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let projection = test
        .thread_manager
        .token_usage_projection(root_thread_id)
        .await?;
    let mut actual = projection
        .threads
        .iter()
        .map(usage_summary)
        .collect::<Vec<_>>();
    actual.sort_by_key(|summary| summary.0.to_string());
    let mut expected = vec![
        (
            root_thread_id,
            None,
            None,
            vec![(
                usage(USAGE_ROOT_INITIAL_TOKENS + USAGE_ROOT_COMPLETION_TOKENS),
                vec![
                    USAGE_ROOT_COMPLETION_RESPONSE.to_string(),
                    USAGE_ROOT_INITIAL_RESPONSE.to_string(),
                ],
            )],
            vec![
                USAGE_ROOT_COMPLETION_RESPONSE.to_string(),
                USAGE_ROOT_INITIAL_RESPONSE.to_string(),
            ],
        ),
        (
            child_thread_id,
            Some(root_thread_id),
            None,
            vec![(
                usage(USAGE_CHILD_INITIAL_TOKENS + USAGE_CHILD_COMPLETION_TOKENS),
                vec![
                    USAGE_CHILD_COMPLETION_RESPONSE.to_string(),
                    USAGE_CHILD_INITIAL_RESPONSE.to_string(),
                ],
            )],
            vec![
                USAGE_CHILD_COMPLETION_RESPONSE.to_string(),
                USAGE_CHILD_INITIAL_RESPONSE.to_string(),
            ],
        ),
        (
            grandchild_thread_id,
            Some(child_thread_id),
            Some(child_thread_id),
            vec![(
                usage(USAGE_GRANDCHILD_TOKENS),
                vec![USAGE_GRANDCHILD_RESPONSE.to_string()],
            )],
            vec![USAGE_GRANDCHILD_RESPONSE.to_string()],
        ),
    ];
    expected.sort_by_key(|summary| summary.0.to_string());
    assert_eq!(actual, expected);
    assert_eq!(
        projection.total_usage,
        usage(
            USAGE_ROOT_INITIAL_TOKENS
                + USAGE_ROOT_COMPLETION_TOKENS
                + USAGE_CHILD_INITIAL_TOKENS
                + USAGE_CHILD_COMPLETION_TOKENS
                + USAGE_GRANDCHILD_TOKENS,
        )
    );

    let repeated_projection = test
        .thread_manager
        .token_usage_projection(root_thread_id)
        .await?;
    assert_eq!(repeated_projection, projection);

    let home = Arc::clone(&test.home);
    let rollout_path = test
        .session_configured
        .rollout_path
        .clone()
        .expect("root rollout path");
    grandchild_thread.shutdown_and_wait().await?;
    child_thread.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    let resume_builder = test_codex()
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
        });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    let cold_projection = resumed
        .thread_manager
        .token_usage_projection(root_thread_id)
        .await?;
    assert_eq!(cold_projection, projection);
    resumed.codex.shutdown_and_wait().await?;

    Ok(())
}
