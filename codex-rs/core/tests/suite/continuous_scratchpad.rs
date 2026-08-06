use anyhow::Result;
use codex_config::CONFIG_TOML_FILE;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuous_scratchpad_stops_after_the_configured_loopback_count() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "done"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_pre_build_hook(|home| {
        std::fs::write(
            home.join(CONFIG_TOML_FILE),
            "[scratchpad.loopback]\nmax_loopbacks = 2\nwindow_minutes = 7\n",
        )
        .expect("write scratchpad loopback config");
    });
    let test = builder.build_with_auto_env(&server).await?;
    write_continuous_scratchpad(&test).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "keep going".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let mut warnings = Vec::new();
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Warning(warning) => warnings.push(warning.message),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(responses.requests().len(), 3);
    assert_eq!(
        warnings,
        vec![
            "Continuous scratchpad loopback limit reached (2 loopbacks in 7 minutes); stopping automatic continuation."
                .to_string()
        ]
    );
    Ok(())
}

async fn write_continuous_scratchpad(test: &TestCodex) -> Result<()> {
    let thread_id = test.session_configured.session_id.to_string();
    let entries_dir = test.codex_home_path().join("scratchpad").join("entries");
    tokio::fs::create_dir_all(&entries_dir).await?;
    let scratchpad = json!({
        "scratchpad_id": thread_id,
        "origin_thread_id": thread_id,
        "status": "active",
        "run_policy": {
            "continuous": {
                "enabled": true
            }
        },
        "next_steps": ["keep working"]
    });
    tokio::fs::write(
        entries_dir.join(format!("{thread_id}.json")),
        serde_json::to_vec(&scratchpad)?,
    )
    .await?;
    Ok(())
}
