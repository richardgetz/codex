use anyhow::Result;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::ResponsesRequest;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn eta_call_output(requests: &[ResponsesRequest], call_id: &str) -> Value {
    let output = requests
        .iter()
        .find_map(|request| request.function_call_output_text(call_id))
        .expect("ETA tool output should be sent to the model");
    serde_json::from_str(&output).expect("ETA tool output should be JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_eta_tool_is_registered_and_records_explicit_lifecycle() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let create_args = json!({
        "operations": [{
            "action": "create",
            "task_id": "compile",
            "title": "Compile the project",
            "estimate_lower_seconds": 10,
            "estimate_upper_seconds": 20
        }]
    });
    let start_args = json!({
        "operations": [{
            "action": "start",
            "task_id": "compile"
        }]
    });
    let complete_args = json!({
        "operations": [{
            "action": "complete",
            "task_id": "compile",
            "reason": "build passed"
        }]
    });
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("eta-create-response"),
                ev_function_call("eta-create", "update_eta", &create_args.to_string()),
                ev_completed("eta-create-response"),
            ]),
            sse(vec![
                ev_response_created("eta-start-response"),
                ev_function_call("eta-start", "update_eta", &start_args.to_string()),
                ev_completed("eta-start-response"),
            ]),
            sse(vec![
                ev_response_created("eta-complete-response"),
                ev_function_call("eta-complete", "update_eta", &complete_args.to_string()),
                ev_completed("eta-complete-response"),
            ]),
            sse(vec![
                ev_response_created("eta-final-response"),
                ev_assistant_message("eta-final-message", "ETA recorded"),
                ev_completed("eta-final-response"),
            ]),
        ],
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Sqlite)
            .expect("test config should allow sqlite");
    });
    let test = builder.build_with_auto_env(&server).await?;
    test.codex
        .start_or_steer_turn(codex_core::TurnInputRequest::user_input(vec![
            UserInput::Text {
                text: "Track the compile task and finish it.".to_string(),
                text_elements: Vec::new(),
            },
        ]))
        .await?;

    let mut updates = Vec::new();
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::ThreadEtaUpdated(update) => updates.push(update),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let requests = responses.requests();
    assert_eq!(requests.len(), 4);
    let first_body = requests[0].body_json();
    let registered = first_body
        .get("tools")
        .and_then(Value::as_array)
        .and_then(|tools| {
            tools.iter().find(|tool| {
                tool.get("type").and_then(Value::as_str) == Some("function")
                    && tool.get("name").and_then(Value::as_str) == Some("update_eta")
            })
        })
        .expect("update_eta should be model-visible as a function tool");
    assert_eq!(registered["name"], "update_eta");
    assert!(registered["description"]
        .as_str()
        .is_some_and(|description| description.contains("Completion is explicit")));

    assert_eq!(eta_call_output(&requests, "eta-create")["changed_tasks"], json!([
        {"task_id": "compile", "status": "pending"}
    ]));
    assert_eq!(eta_call_output(&requests, "eta-start")["changed_tasks"], json!([
        {"task_id": "compile", "status": "active"}
    ]));
    assert_eq!(eta_call_output(&requests, "eta-complete")["changed_tasks"], json!([
        {"task_id": "compile", "status": "completed"}
    ]));

    assert_eq!(updates.len(), 3);
    assert_eq!(updates[0].sequence, 1);
    assert_eq!(updates[0].changed_tasks[0].status, "pending");
    assert_eq!(updates[1].sequence, 2);
    assert_eq!(updates[1].changed_tasks[0].status, "active");
    assert_eq!(updates[2].sequence, 3);
    let completed = &updates[2].changed_tasks[0];
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.original_lower_seconds, Some(10));
    assert_eq!(completed.original_upper_seconds, Some(20));
    assert!(completed.started_at.is_some());
    assert!(completed.terminal_at.is_some());
    assert!(completed.actual_elapsed_seconds.is_some());

    Ok(())
}
