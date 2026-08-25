use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_tmp_tool_records_current_session_and_thread_lineage() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.session_tmp.enabled = true;
    });
    let test = builder.build(&server).await?;
    let call_id = "session-tmp-create";
    let arguments = json!({
        "name": "artifact.txt",
        "purpose": "integration artifact",
        "retention": "session",
        "kind": "file",
    })
    .to_string();

    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call_with_namespace(call_id, "session_tmp", "create", &arguments),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let completion = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "created"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_text_turn("create a temporary artifact").await?;

    let request = completion.single_request();
    let output_item = request.function_call_output(call_id).to_owned();
    let output = output_item
        .get("output")
        .and_then(Value::as_str)
        .expect("session_tmp output should be text");
    let entry: Value = serde_json::from_str(output)?;
    let session_id = test.session_configured.session_id.to_string();
    let thread_id = test.session_configured.thread_id.to_string();
    let managed_root = test.codex_home_path().join("session-tmp");
    let absolute_path = entry
        .get("absolute_path")
        .and_then(Value::as_str)
        .expect("created entry should include an absolute path");

    assert_eq!(entry["metadata"]["session_id"], session_id);
    assert_eq!(entry["metadata"]["thread_id"], thread_id);
    assert_eq!(entry["metadata"]["purpose"], "integration artifact");
    assert!(std::path::Path::new(absolute_path).starts_with(managed_root));
    assert!(std::path::Path::new(absolute_path).exists());

    Ok(())
}
