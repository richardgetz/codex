//! Opportunistic attachment may fall back; automatic startup requires a shared server.

use super::*;
use crate::daemon_startup::connect_default_daemon;
use crate::legacy_core::config::ConfigBuilder;
use codex_app_server_daemon::ApplyOutput;
use codex_app_server_daemon::ApplyStatus;
use codex_app_server_protocol::JSONRPCMessage;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

#[test]
fn audited_overrides_allow_daemon_without_allowing_arbitrary_config() {
    for (raw, eligible) in [
        ("features.transcript_v2=true", true),
        ("features.transcript_v2=false", true),
        ("features={transcript_v2=true}", true),
        ("features.transcript_v2='true'", false),
        ("features.worktrees=true", true),
        ("features.worktrees=false", true),
        ("features={worktrees=true}", true),
        (
            "features={worktrees=true,api_key_model_discovery=false}",
            true,
        ),
        ("features.auth_elicitation=false", true),
        ("features.code_mode_host=false", true),
        ("features.mcp_oauth_refresh_coordination=false", true),
        ("suppress_unstable_features_warning=true", true),
        ("suppress_unstable_features_warning='true'", false),
        ("features={worktrees=true,shell_tool=false}", false),
        ("features.shell_tool=false", false),
        ("features.worktrees.enabled=true", false),
        ("tui.fullscreen_transcript=true", true),
        ("tui.fullscreen_transcript=false", true),
        ("tui.fullscreen_transcript=\"true\"", false),
        ("tui={fullscreen_transcript=true}", true),
        ("tui={fullscreen_transcript='true'}", false),
        ("tui={fullscreen_transcript=true,animations=false}", false),
        ("features={}", false),
        ("model='test'", false),
    ] {
        let overrides = codex_utils_cli::CliConfigOverrides {
            raw_overrides: vec![raw.to_string()],
        }
        .parse_overrides()
        .unwrap();
        assert_eq!(
            daemon_startup::config_exclusion(
                &overrides,
                &LoaderOverrides::default(),
                /*strict_config*/ false,
                /*bypass_hook_trust*/ false,
            )
            .is_none(),
            eligible,
            "{raw}"
        );
    }
}

#[test]
fn monorepo_wrapper_overrides_are_eligible_and_select_only_server_features() {
    let overrides = codex_utils_cli::CliConfigOverrides {
        raw_overrides: [
            "features.realtime_conversation=true",
            "features.worktrees=true",
            "features.remote_models=true",
            "features.api_key_model_discovery=true",
            "features.request_rule=true",
            "features.auth_elicitation=true",
            "features.mcp_oauth_refresh_coordination=true",
            "features.responses_websockets_v2=true",
            "features.workspace_owner_usage_nudge=true",
            "features.tool_search_always_defer_mcp_tools=true",
            "features.remote_compaction_v2=true",
            "features.standalone_web_search=true",
            "features.multi_agent_mode=true",
            "features.code_mode_host=true",
            "suppress_unstable_features_warning=true",
        ]
        .map(str::to_string)
        .to_vec(),
    }
    .parse_overrides()
    .unwrap();
    assert_eq!(
        daemon_startup::config_exclusion(
            &overrides,
            &LoaderOverrides::default(),
            /*strict_config*/ false,
            /*bypass_hook_trust*/ false,
        ),
        None
    );
    assert_eq!(
        daemon_startup::server_features(&overrides),
        std::collections::BTreeMap::from([
            ("api_key_model_discovery".to_string(), true),
            ("auth_elicitation".to_string(), true),
            ("code_mode_host".to_string(), true),
            ("mcp_oauth_refresh_coordination".to_string(), true),
        ])
    );
}

#[test]
fn missing_handoff_journal_reports_versions_without_invalid_recovery_guidance() {
    let issue = daemon_startup::launcher_update_issue(&ApplyOutput {
        status: ApplyStatus::NeedsAttention,
        handoff_id: Some("handoff-1".to_string()),
        state: Some("needsAttention".to_string()),
        runtime_version: Some("0.156.1".to_string()),
        created_at: Some(1),
        nodes: Vec::new(),
        managed_codex_path: Some("/opt/homebrew/bin/codex-rick".into()),
        managed_codex_version: Some("0.157.1-rick.2".to_string()),
        running_managed_codex_version: Some("0.156.1-rick.2".to_string()),
        socket_path: "/tmp/codex.sock".into(),
        app_server_version: Some("0.156.1".to_string()),
        quarantined: false,
        can_retry: false,
        can_quarantine: false,
        handoff_resolution: None,
        handoff_resolutions: Vec::new(),
        failure_kind: Some(codex_app_server_daemon::ApplyFailureKind::HandoffJournalMissing),
        error: Some("handoff blocked".to_string()),
    });

    assert!(issue.reason.contains("/opt/homebrew/bin/codex-rick"));
    assert!(issue.reason.contains("0.157.1-rick.2"));
    assert!(issue.reason.contains("0.156.1-rick.2"));
    assert!(issue.reason.contains("handoff blocked"));
    assert!(issue.reason.contains("matching handoff journal is missing"));
    assert!(!issue.reason.contains("daemon recover"));
    assert_eq!(issue.restart_features, None);
}

#[test]
fn daemon_features_follow_cli_table_replacement_and_last_value() {
    let overrides = codex_utils_cli::CliConfigOverrides {
        raw_overrides: [
            "features.api_key_model_discovery=true",
            "features={code_mode_host=false}",
            "features.code_mode_host=true",
        ]
        .map(str::to_string)
        .to_vec(),
    }
    .parse_overrides()
    .unwrap();
    assert_eq!(
        daemon_startup::server_features(&overrides),
        std::collections::BTreeMap::from([("code_mode_host".to_string(), true),])
    );
}

#[test]
fn daemon_launch_telemetry_records_once_on_connection_or_early_return() {
    for connected in [false, true] {
        let observations = std::cell::RefCell::new(Vec::new());
        let launch = daemon_telemetry::Launch(Some(|target: &AppServerTarget, actual: bool| {
            observations.borrow_mut().push((target.clone(), actual));
        }));
        if connected {
            launch.record(&AppServerTarget::Embedded, connected);
        } else {
            drop(launch);
        }
        assert_eq!(
            observations.into_inner(),
            vec![(AppServerTarget::Embedded, connected)]
        );
    }
}

#[tokio::test]
async fn daemon_feature_compatibility_respects_required_and_optional_attachment()
-> color_eyre::Result<()> {
    use codex_app_server_protocol::JSONRPCMessage;
    use futures::SinkExt;
    use futures::StreamExt;
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;

    for allow_embedded_fallback in [false, true] {
        for scenario in [
            "matching",
            "stale overrides",
            "conflicting client",
            "unsupported RPC",
        ] {
            let home = TempDir::new()?;
            let mut config = ConfigBuilder::default()
                .codex_home(home.path().to_path_buf())
                .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
                .build()
                .await?;
            config.features.enable(Feature::ApiKeyModelDiscovery)?;
            if scenario == "conflicting client" {
                config.features.disable(Feature::ApiKeyModelDiscovery)?;
            }
            let features = [
                Feature::ApiKeyModelDiscovery,
                Feature::CodeModeHost,
                Feature::AuthElicitation,
                Feature::McpOAuthRefreshCoordination,
            ].map(|feature| {
                let enabled = if feature == Feature::ApiKeyModelDiscovery && scenario == "stale overrides" {
                    false
                } else if feature == Feature::ApiKeyModelDiscovery && scenario == "conflicting client" {
                    true
                } else {
                    config.features.enabled(feature)
                };
                json!({"name": feature.key(), "stage": "beta", "enabled": enabled, "defaultEnabled": false})
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let target = AppServerTarget::LocalDaemon {
                endpoint: RemoteAppServerEndpoint::WebSocket {
                    websocket_url: format!("ws://{}", listener.local_addr()?),
                    auth_token: None,
                },
                allow_embedded_fallback,
            };
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(text))) = socket.next().await {
                    let JSONRPCMessage::Request(request) = serde_json::from_str(&text).unwrap()
                    else {
                        continue;
                    };
                    let response = if request.method == "initialize" {
                        json!({"id": request.id, "result": {"userAgent": "daemon-test"}})
                    } else {
                        assert_eq!(request.method, "experimentalFeature/list");
                        assert_eq!(request.params.unwrap()["threadId"], json!(null));
                        if scenario == "unsupported RPC" {
                            json!({"id": request.id, "error": {"code": -32601, "message": "method not found"}})
                        } else {
                            json!({"id": request.id, "result": {"data": features, "nextCursor": null}})
                        }
                    };
                    socket
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .unwrap();
                }
            });
            let result = daemon_startup::compatibility_warning(&target, &config).await;
            server.await?;
            if scenario == "matching" {
                assert_eq!(result?, None);
                continue;
            }
            let reason = match scenario {
                "stale overrides" => "This session requires api_key_model_discovery to be enabled",
                "conflicting client" => {
                    "This session requires api_key_model_discovery to be disabled"
                }
                "unsupported RPC" => {
                    "experimentalFeature/list failed: method not found (code -32601)"
                }
                _ => unreachable!(),
            };
            if allow_embedded_fallback {
                assert_eq!(
                    result?,
                    Some(format!(
                        "Running without the shared background server: {reason}."
                    ))
                );
            } else {
                let error = result.unwrap_err().to_string();
                assert_eq!(
                    error,
                    format!(
                        "Cannot use the shared background server: {reason}.\n{}",
                        daemon_startup::FAILURE_HINT
                    )
                );
                if scenario == "stale overrides" {
                    insta::assert_snapshot!("daemon_feature_mismatch_error", error);
                }
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn daemon_connection_rejects_unprotected_socket_before_handshake() -> color_eyre::Result<()> {
    let home = TempDir::new()?;
    let parent = home.path().join("control");
    std::fs::create_dir(&parent)?;
    let socket_path = AbsolutePathBuf::from_absolute_path_checked(parent.join("server.sock"))?;
    let mut listener = codex_uds::UnixListener::bind(socket_path.as_path()).await?;
    let target = AppServerTarget::LocalDaemon {
        allow_embedded_fallback: true,
        endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
    };
    tokio::select! {
        result = app_server_connection::connect(&target) => assert!(result.is_err()),
        _ = listener.accept() => panic!("unprotected listener must not receive a connection"),
    }
    Ok(())
}

#[tokio::test]
async fn daemon_startup_falls_back_only_for_implicit_endpoints() -> color_eyre::Result<()> {
    for scenario in [
        "missing socket",
        "failed handshake",
        "explicit endpoint",
        "required daemon",
    ] {
        let home = TempDir::new()?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = if scenario == "missing socket" {
            RemoteAppServerEndpoint::UnixSocket {
                socket_path: AbsolutePathBuf::from_absolute_path_checked(
                    home.path().join("gone.sock"),
                )?,
            }
        } else {
            RemoteAppServerEndpoint::WebSocket {
                websocket_url: format!("ws://{}", listener.local_addr()?),
                auth_token: None,
            }
        };
        let reject_handshake = tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let mut target = if scenario == "explicit endpoint" {
            AppServerTarget::Remote { endpoint }
        } else {
            AppServerTarget::LocalDaemon {
                endpoint,
                allow_embedded_fallback: scenario != "required daemon",
            }
        };
        let original_target = target.clone();
        let mut state_db = None;
        let result = start_app_server(
            &mut target,
            Arg0DispatchPaths::default(),
            config,
            Vec::new(),
            LoaderOverrides::default(),
            /*strict_config*/ false,
            CloudConfigBundleLoader::default(),
            codex_feedback::CodexFeedback::new(),
            /*log_db*/ None,
            &mut state_db,
            Arc::new(EnvironmentManager::default_for_tests()),
            Default::default(),
        )
        .await;
        reject_handshake.abort();
        if scenario == "missing socket" {
            let server = AppServerSession::new(result?, target.thread_params_mode());
            assert!(server.uses_embedded_app_server());
            assert_eq!(target, AppServerTarget::Embedded);
            assert!(state_db.is_some());
            server.shutdown().await?;
        } else {
            assert!(result.is_err());
            if scenario == "required daemon" {
                let message = result.err().unwrap().to_string();
                assert!(message.contains("rerun the same command with --no-daemon"));
                assert!(message.contains("failed to connect to remote app server"));
            }
            assert_eq!(target, original_target);
            assert!(state_db.is_none());
        }
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn default_daemon_discovery_distinguishes_absent_and_stale_sockets() -> color_eyre::Result<()>
{
    let home = TempDir::new_in("/tmp")?;
    assert!(connect_default_daemon(home.path()).await?.is_none());

    let socket_path = codex_app_server_client::app_server_control_socket_path(home.path())?;
    std::fs::create_dir_all(socket_path.as_path().parent().unwrap())?;
    let listener = codex_uds::UnixListener::bind(socket_path.as_path()).await?;
    drop(listener);

    let error = match connect_default_daemon(home.path()).await {
        Ok(_) => color_eyre::eyre::bail!("an existing but unavailable socket must fail closed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("refusing to start a competing embedded server")
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn default_daemon_startup_reuses_authoritative_handshake_for_resume() -> color_eyre::Result<()>
{
    let home = TempDir::new_in("/tmp")?;
    let socket_path = codex_app_server_client::app_server_control_socket_path(home.path())?;
    std::fs::create_dir_all(socket_path.as_path().parent().unwrap())?;
    let mut listener = codex_uds::UnixListener::bind(socket_path.as_path()).await?;
    let server = tokio::spawn(async move {
        let stream = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut methods = Vec::new();
        while let Some(message) = socket.next().await {
            let Message::Text(text) = message? else {
                continue;
            };
            match serde_json::from_str::<JSONRPCMessage>(&text)? {
                JSONRPCMessage::Request(request) => {
                    methods.push(request.method.clone());
                    assert_eq!(request.method, "initialize");
                    socket
                        .send(Message::Text(
                            json!({
                                "id": request.id,
                                "result": {"userAgent": "implicit-resume-test"}
                            })
                            .to_string()
                            .into(),
                        ))
                        .await?;
                }
                JSONRPCMessage::Notification(notification)
                    if notification.method == "initialized" =>
                {
                    break;
                }
                JSONRPCMessage::Notification(_)
                | JSONRPCMessage::Response(_)
                | JSONRPCMessage::Error(_) => {}
            }
        }
        Ok::<_, color_eyre::Report>(methods)
    });

    let prepared = connect_default_daemon(home.path())
        .await?
        .expect("the bound daemon socket should be discovered");
    let target = AppServerTarget::LocalDaemon {
        allow_embedded_fallback: false,
        endpoint: RemoteAppServerEndpoint::UnixSocket {
            socket_path: prepared.socket_path.clone(),
        },
    };
    // Reuse the connection that already completed `initialize`; startup must not handshake a
    // second connection before sending thread-resume requests.
    let app_server = AppServerSession::new(prepared.app_server, target.thread_params_mode());
    assert!(!app_server.uses_embedded_app_server());
    app_server.shutdown().await?;

    let methods = tokio::time::timeout(Duration::from_secs(5), server)
        .await??
        .expect("initialize methods should be captured");
    assert_eq!(methods, vec!["initialize".to_string()]);
    Ok(())
}

#[test]
fn daemon_eligibility_preserves_launch_options_and_explains_exclusions() {
    use clap::Parser;
    for (args, expected) in [
        ("--no-daemon", Some("--no-daemon")),
        ("--worktree", None),
        ("--worktree --no-daemon", Some("--no-daemon")),
        ("--oss", Some("--oss")),
        ("--profile test", Some("--profile")),
        ("--strict-config", Some("--strict-config")),
        (
            "--dangerously-bypass-hook-trust",
            Some("--dangerously-bypass-hook-trust"),
        ),
        (
            "-m test --cd /tmp -i image.png -a never -s workspace-write --add-dir /tmp --no-alt-screen hello",
            None,
        ),
    ] {
        let cli = Cli::parse_from(std::iter::once("codex").chain(args.split_whitespace()));
        assert_eq!(
            daemon_startup::exclusion(
                &cli,
                &[],
                &LoaderOverrides::default(),
                /*workload_identity_selected*/ false,
                /*exec_server_url*/ None
            ),
            expected
        );
    }
    let mut cli = Cli::parse_from(["codex"]);
    let overrides = vec![("web_search".into(), toml::Value::String("live".into()))];
    let loader = LoaderOverrides {
        ignore_user_config: true,
        ..Default::default()
    };
    for (kv, loader, workload, executor, expected) in [
        (
            &overrides[..],
            LoaderOverrides::default(),
            false,
            None,
            "command-line configuration overrides (-c, --enable, --disable, or --search)",
        ),
        (&[][..], loader, false, None, "custom configuration loader"),
        (
            &[][..],
            LoaderOverrides::default(),
            true,
            None,
            "workload identity",
        ),
        (
            &[][..],
            LoaderOverrides::default(),
            false,
            Some(std::ffi::OsStr::new("executor")),
            "executor selection (CODEX_EXEC_SERVER_URL)",
        ),
    ] {
        assert_eq!(
            daemon_startup::exclusion(&cli, kv, &loader, workload, executor),
            Some(expected)
        );
    }
    cli.agents_overview = true;
    cli.strict_config = true;
    assert_eq!(
        daemon_startup::exclusion(
            &cli,
            &overrides,
            &LoaderOverrides::default(),
            /*workload_identity_selected*/ false,
            /*exec_server_url*/ None
        ),
        None
    );
}

#[test]
fn daemon_exclusion_warning_snapshot() {
    use crate::history_cell::HistoryCell;
    let cell = crate::history_cell::StartupWarningsCell::new(vec![
        "Running without the shared background server: --strict-config requires embedded mode."
            .into(),
    ]);
    let text = cell
        .transcript_lines(/*width*/ 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("daemon_exclusion_warning", text);
}
