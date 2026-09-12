use codex_session_tmp::SessionTmpConfig;
use codex_session_tmp::SessionTmpOwner;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
#[cfg(unix)]
use core_test_support::responses::ev_function_call;
#[cfg(unix)]
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::protocol::EventMsg;
use serde_json::Value;
use serde_json::json;
use std::fs;
use std::time::Duration;

const SESSION_TMP_UNAVAILABLE_WARNING: &str =
    "Session temporary storage is unavailable; continuing without it for this runtime. Use a new empty root or repair its managed marker after verifying its contents.";

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
    let output_item = request.function_call_output(call_id);
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
    let output_item = request.function_call_output(call_id);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_session_tmp_fails_open_and_disables_runtime_consumers() -> anyhow::Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex()
        .with_pre_build_hook(|home| {
            let root = home.join("session-tmp");
            fs::create_dir_all(root.join("sessions")).unwrap();
            fs::write(root.join("preserved.txt"), b"keep this file").unwrap();
        })
        .with_config(|config| {
            config.session_tmp.enabled = true;
        });
    let test = builder.build(&server).await?;

    let warning = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::Warning(warning) if warning.message == SESSION_TMP_UNAVAILABLE_WARNING
        )
    })
    .await;
    assert!(matches!(
        warning,
        EventMsg::Warning(warning) if warning.message == SESSION_TMP_UNAVAILABLE_WARNING
    ));

    let completion = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("unavailable-resp"),
            ev_assistant_message("unavailable-msg", "continued"),
            ev_completed("unavailable-resp"),
        ]),
    )
    .await;
    test.submit_text_turn("continue without managed temporary storage")
        .await?;

    let request = completion.single_request();
    assert!(request.tool_by_name("session_tmp", "create").is_none());
    let developer_text = request.message_input_texts("developer").join("\n");
    assert!(!developer_text.contains("<session_tmp_instructions>"));

    let root = test.codex_home_path().join("session-tmp");
    assert_eq!(fs::read(root.join("preserved.txt"))?, b"keep this file");
    assert!(!root.join(".codex-managed-session-tmp").exists());

    let snapshot = test.codex.config_snapshot().await;
    let invalid_root_in_sandbox = snapshot
        .permission_profile
        .file_system_sandbox_policy()
        .entries
        .iter()
        .any(|entry| {
            let FileSystemPath::Path { path } = &entry.path else {
                return false;
            };
            path.to_abs_path()
                .is_ok_and(|path| path.as_path().starts_with(&root))
    });
    assert!(!invalid_root_in_sandbox);

    #[cfg(unix)]
    {
        let call_id = "unavailable-session-tmp-env";
        let arguments = json!({
            "cmd": "printf '%s' \"$TMPDIR\"",
            "yield_time_ms": 1000,
        });
        let env_responses = mount_sse_sequence(
            &server,
            vec![
                sse(vec![
                    ev_response_created("unavailable-env-resp"),
                    ev_function_call(call_id, "exec_command", &arguments.to_string()),
                    ev_completed("unavailable-env-resp"),
                ]),
                sse(vec![
                    ev_response_created("unavailable-env-final-resp"),
                    ev_assistant_message("unavailable-env-final-msg", "env checked"),
                    ev_completed("unavailable-env-final-resp"),
                ]),
            ],
        )
        .await;
        test.submit_text_turn("check the temporary directory environment")
            .await?;
        let temp_dir = env_responses
            .function_call_output_text(call_id)
            .expect("exec command output should be captured");
        assert!(!temp_dir.contains(root.to_str().expect("test root should be UTF-8")));
        assert_eq!(env_responses.requests().len(), 2);
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_session_tmp_fails_open_on_resume_and_preserves_data() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut initial_builder = test_codex().with_config(|config| {
        config.session_tmp.enabled = false;
    });
    let initial = initial_builder.build(&server).await?;
    let _initial_response = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("initial-resp"),
            ev_assistant_message("initial-msg", "initial"),
            ev_completed("initial-resp"),
        ]),
    )
    .await;
    initial.submit_text_turn("create a resumable session").await?;
    assert!(initial.session_configured.rollout_path.is_some());
    let mut resume_builder = test_codex()
        .with_pre_build_hook(|home| {
            let root = home.join("session-tmp-custom");
            fs::create_dir_all(root.join("sessions")).unwrap();
            fs::write(root.join("preserved.txt"), b"keep this file").unwrap();
        })
        .with_config(|config| {
            config.session_tmp.enabled = true;
            config.session_tmp.root = Some(config.codex_home.join("session-tmp-custom"));
        });
    let resumed = resume_builder.restart(&server, &initial).await?;

    let warning = wait_for_event(&resumed.codex, |event| {
        matches!(
            event,
            EventMsg::Warning(warning) if warning.message == SESSION_TMP_UNAVAILABLE_WARNING
        )
    })
    .await;
    assert!(matches!(
        warning,
        EventMsg::Warning(warning) if warning.message == SESSION_TMP_UNAVAILABLE_WARNING
    ));

    let completion = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resumed-resp"),
            ev_assistant_message("resumed-msg", "resumed"),
            ev_completed("resumed-resp"),
        ]),
    )
    .await;
    resumed
        .submit_text_turn("continue after resume without managed temporary storage")
        .await?;

    let request = completion.single_request();
    assert!(request.tool_by_name("session_tmp", "create").is_none());
    assert!(!request
        .message_input_texts("developer")
        .join("\n")
        .contains("<session_tmp_instructions>"));

    let root = resumed.codex_home_path().join("session-tmp-custom");
    assert_eq!(fs::read(root.join("preserved.txt"))?, b"keep this file");
    assert!(!root.join(".codex-managed-session-tmp").exists());

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inaccessible_stale_session_lock_does_not_block_provenance_startup() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex()
        .with_pre_build_hook(|home| {
            let session_tmp_config = SessionTmpConfig {
                enabled: true,
                root: Some(home.join("session-tmp")),
                stale_after: Duration::from_secs(60),
            };
            let old_session_root = {
                let manager = codex_session_tmp::SessionTmpManager::open(
                    &session_tmp_config,
                    home,
                    "old-session",
                    "old-thread",
                    SessionTmpOwner::RootSession,
                )
                .unwrap()
                .unwrap();
                manager.session_root().to_path_buf()
            };
            fs::write(
                old_session_root.join("session.json"),
                serde_json::json!({
                    "schema_version": 1,
                    "session_id": "old-session",
                    "created_at": 0,
                    "updated_at": 0,
                    "status": "active"
                })
                .to_string(),
            )
            .unwrap();
            let lock_path = old_session_root
                .parent()
                .unwrap()
                .join(".locks")
                .join("old-session.lock");
            let mut permissions = fs::metadata(&lock_path).unwrap().permissions();
            permissions.set_mode(0o400);
            fs::set_permissions(&lock_path, permissions).unwrap();
        })
        .with_config(|config| {
            config.session_tmp.enabled = true;
            config.decision_provenance.enabled = true;
            config.decision_provenance.git_intent_bridge = true;
        });

    let test = builder.build(&server).await?;
    assert!(test.session_configured.rollout_path.is_some());

    Ok(())
}
