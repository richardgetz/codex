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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_tmp_schema_describes_disposable_lineage_and_cleanup() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.session_tmp.enabled = true;
    });
    let test = builder.build(&server).await?;
    let call_id = "session-tmp-schema";

    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call_with_namespace(call_id, "session_tmp", "get_schema", "{}"),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let completion = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "schema"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_text_turn("describe temporary storage").await?;

    let request = completion.single_request();
    let output_item = request.function_call_output(call_id).to_owned();
    let output = output_item
        .get("output")
        .and_then(Value::as_str)
        .expect("session_tmp schema output should be text");
    let schema: Value = serde_json::from_str(output)?;

    assert_eq!(
        schema,
        json!({
            "namespace": "session_tmp",
            "retention": {
                "session": "remove when the owning session ends",
                "manual": "survive normal session cleanup; removable by /tmp clear or stale-session reap",
                "ttl": "ttl:<seconds>, with <seconds> replaced by a non-negative integer",
            },
            "lineage": "Paths under the returned agent_root belong to the current session and thread. Treat every path there as disposable; untracked shell-created paths are also eligible for cleanup.",
            "cleanup": "Session retention is removed when the root session ends; manual retention survives normal cleanup but can be removed by user clear or stale reap.",
        })
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_tmp_guidance_is_only_injected_when_enabled() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let enabled_response = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("enabled-resp"),
            ev_assistant_message("enabled-msg", "enabled"),
            ev_completed("enabled-resp"),
        ]),
    )
    .await;
    let mut enabled_builder = test_codex().with_config(|config| {
        config.session_tmp.enabled = true;
    });
    let enabled_test = enabled_builder.build(&server).await?;
    enabled_test
        .submit_text_turn("use temporary storage")
        .await?;

    let enabled_developer_text = enabled_response
        .single_request()
        .message_input_texts("developer")
        .join("\n");
    assert!(enabled_developer_text.contains("<session_tmp_instructions>"));
    assert!(enabled_developer_text.contains("Treat this agent directory as disposable"));

    let disabled_response = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("disabled-resp"),
            ev_assistant_message("disabled-msg", "disabled"),
            ev_completed("disabled-resp"),
        ]),
    )
    .await;
    let mut disabled_builder = test_codex();
    let disabled_test = disabled_builder.build(&server).await?;
    disabled_test
        .submit_text_turn("use temporary storage")
        .await?;

    let disabled_developer_text = disabled_response
        .single_request()
        .message_input_texts("developer")
        .join("\n");
    assert!(!disabled_developer_text.contains("<session_tmp_instructions>"));

    Ok(())
}
