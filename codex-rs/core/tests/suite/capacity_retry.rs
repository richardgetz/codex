use anyhow::Result;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_failed;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuous_mode_recovers_from_model_capacity_errors() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse_failed("resp-1", "server_is_overloaded", "model at capacity"),
            sse_failed("resp-2", "server_is_overloaded", "model at capacity"),
            sse(vec![ev_response_created("resp-3"), ev_completed("resp-3")]),
        ],
    )
    .await;
    let mut builder = test_codex().with_config(move |config| {
        config.scratchpad.capacity_retry.enabled = true;
        config.scratchpad.capacity_retry.delay = Duration::ZERO;
    });
    let test = builder.build_with_auto_env(&server).await?;
    write_continuous_scratchpad(&test, true).await?;

    submit_turn(&test, "keep going").await?;

    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Warning(warning) => warnings.push(warning.message),
            EventMsg::Error(error) => errors.push(error.message),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(responses.requests().len(), 3);
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(
        warnings,
        vec![
            "Selected model is at capacity. Continuous mode will retry in 0 minute(s). Use `/continuous off` to stop automatic retries."
                .to_string(),
            "Selected model is at capacity. Continuous mode will retry in 0 minute(s). Use `/continuous off` to stop automatic retries."
                .to_string(),
        ]
    );
    Ok(())
}

#[test_case(false, true; "feature disabled")]
#[test_case(true, false; "continuous mode disabled")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capacity_retry_requires_both_feature_and_continuous_mode(
    feature_enabled: bool,
    continuous_enabled: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = responses::mount_sse_once(
        &server,
        sse_failed("resp-1", "server_is_overloaded", "model at capacity"),
    )
    .await;
    let mut builder = test_codex().with_config(move |config| {
        config.scratchpad.capacity_retry.enabled = feature_enabled;
        config.scratchpad.capacity_retry.delay = Duration::ZERO;
    });
    let test = builder.build_with_auto_env(&server).await?;
    write_continuous_scratchpad(&test, continuous_enabled).await?;

    submit_turn(&test, "do not retry").await?;

    let error = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    let EventMsg::Error(error) = error else {
        unreachable!("event filter only accepts errors");
    };
    assert_eq!(response.requests().len(), 1);
    assert_eq!(
        error.message,
        "Selected model is at capacity. Please try a different model."
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_continuous_mode_during_wait_stops_capacity_retries() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = responses::mount_sse_once(
        &server,
        sse_failed("resp-1", "server_is_overloaded", "model at capacity"),
    )
    .await;
    let mut builder = test_codex().with_config(move |config| {
        config.scratchpad.capacity_retry.enabled = true;
        config.scratchpad.capacity_retry.delay = Duration::from_secs(60);
    });
    let test = builder.build_with_auto_env(&server).await?;
    write_continuous_scratchpad(&test, true).await?;

    submit_turn(&test, "stop retrying when continuous mode is disabled").await?;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::Warning(_))).await;
    write_continuous_scratchpad(&test, false).await?;

    let error = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    let EventMsg::Error(error) = error else {
        unreachable!("event filter only accepts errors");
    };
    assert_eq!(response.requests().len(), 1);
    assert_eq!(
        error.message,
        "Selected model is at capacity. Please try a different model."
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupting_capacity_wait_aborts_without_retrying() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = responses::mount_sse_once(
        &server,
        sse_failed("resp-1", "server_is_overloaded", "model at capacity"),
    )
    .await;
    let mut builder = test_codex().with_config(move |config| {
        config.scratchpad.capacity_retry.enabled = true;
        config.scratchpad.capacity_retry.delay = Duration::from_secs(60);
    });
    let test = builder.build_with_auto_env(&server).await?;
    write_continuous_scratchpad(&test, true).await?;

    submit_turn(&test, "interrupt the capacity wait").await?;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::Warning(_))).await;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;

    assert_eq!(response.requests().len(), 1);
    Ok(())
}

async fn write_continuous_scratchpad(test: &TestCodex, enabled: bool) -> Result<()> {
    let thread_id = test.session_configured.session_id.to_string();
    let entries_dir = test.codex_home_path().join("scratchpad").join("entries");
    tokio::fs::create_dir_all(&entries_dir).await?;
    let scratchpad = json!({
        "scratchpad_id": thread_id,
        "origin_thread_id": thread_id,
        "status": "active",
        "run_policy": {
            "continuous": {
                "enabled": enabled
            }
        },
        "next_steps": []
    });
    tokio::fs::write(
        entries_dir.join(format!("{thread_id}.json")),
        serde_json::to_vec(&scratchpad)?,
    )
    .await?;
    Ok(())
}

async fn submit_turn(test: &TestCodex, text: &str) -> Result<()> {
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    Ok(())
}
