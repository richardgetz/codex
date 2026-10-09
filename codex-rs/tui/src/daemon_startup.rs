//! Local daemon launch policy. Explicit embedded launches never discover or start a daemon;
//! optional attachment may fall back to embedded mode, while automatic launches
//! require a compatible shared server and a successful connection, except when
//! the Windows launcher forbids detaching a missing server. Elevated local
//! Windows sessions use explicit embedded behavior before discovery or startup.

use super::*;
use std::collections::BTreeMap;
use std::path::Path;

const SERVER_FEATURES: [Feature; 4] = [
    Feature::ApiKeyModelDiscovery,
    Feature::CodeModeHost,
    Feature::AuthElicitation,
    Feature::McpOAuthRefreshCoordination,
];

pub(super) const FAILURE_HINT: &str = "To work without the background server, rerun the same command with --no-daemon (including resume or fork and its arguments).";

#[cfg(any(windows, test))]
pub(super) const ELEVATED_LAUNCH_WARNING: &str = "Running as administrator: shared background server disabled. To enable it, restart Codex in a terminal without administrator permissions.";

#[derive(Debug, thiserror::Error)]
#[error("Cannot use the shared background server: {reason}.\n{FAILURE_HINT}")]
pub(super) struct CompatibilityError {
    pub reason: String,
    pub restart_features: Option<BTreeMap<String, bool>>,
}

pub(super) fn exclusion(
    cli: &Cli,
    cli_kv_overrides: &[(String, toml::Value)],
    loader_overrides: &LoaderOverrides,
    workload_identity_selected: bool,
    exec_server_url: Option<&std::ffi::OsStr>,
) -> Option<&'static str> {
    if cli.no_daemon {
        Some("--no-daemon")
    } else if cli.oss {
        Some("--oss")
    } else if workload_identity_selected {
        Some("workload identity")
    } else if exec_server_url.is_some() {
        Some("executor selection (CODEX_EXEC_SERVER_URL)")
    } else if cli.agents_overview {
        None
    } else if cli.config_profile_v2.is_some() {
        Some("--profile")
    } else {
        config_exclusion(
            cli_kv_overrides,
            loader_overrides,
            cli.strict_config,
            cli.bypass_hook_trust,
        )
    }
}

pub(super) fn config_exclusion(
    cli_kv_overrides: &[(String, toml::Value)],
    loader_overrides: &LoaderOverrides,
    strict_config: bool,
    bypass_hook_trust: bool,
) -> Option<&'static str> {
    if !cli_kv_overrides
        .iter()
        .all(|(key, value)| match key.as_str() {
            "suppress_unstable_features_warning" | "tui.fullscreen_transcript" => value.is_bool(),
            "tui" => value.as_table().is_some_and(|tui| {
                tui.len() == 1
                    && tui
                        .get("fullscreen_transcript")
                        .is_some_and(toml::Value::is_bool)
            }),
            "features" => value.as_table().is_some_and(|features| {
                !features.is_empty()
                    && features
                        .iter()
                        .all(|(name, value)| allowed_feature(name) && value.is_bool())
            }),
            _ => key.strip_prefix("features.").is_some_and(allowed_feature) && value.is_bool(),
        })
    {
        Some("command-line configuration overrides (-c, --enable, --disable, or --search)")
    } else if !loader_overrides_are_default(loader_overrides) {
        Some("custom configuration loader")
    } else if strict_config {
        Some("--strict-config")
    } else if bypass_hook_trust {
        Some("--dangerously-bypass-hook-trust")
    } else {
        None
    }
}

fn allowed_feature(name: &str) -> bool {
    matches!(
        name,
        // Client gates and per-thread settings already forwarded in thread requests.
        "daemon_auto_start" | "worktrees" | "transcript_v2" | "realtime_conversation" | "standalone_web_search"
        // Shared services and threadless MCP operations need daemon compatibility checks.
        | "api_key_model_discovery" | "code_mode_host" | "auth_elicitation"
        | "mcp_oauth_refresh_coordination"
        // Removed flags still passed by older launch scripts.
        | "remote_models" | "request_rule" | "responses_websockets_v2"
        | "workspace_owner_usage_nudge" | "tool_search_always_defer_mcp_tools"
        | "remote_compaction_v2" | "multi_agent_mode"
    )
}

pub(super) fn server_features(overrides: &[(String, toml::Value)]) -> BTreeMap<String, bool> {
    let layer = codex_config::build_cli_overrides_layer(overrides);
    SERVER_FEATURES
        .into_iter()
        .filter_map(|feature| {
            let name = feature.key();
            let enabled = layer.get("features")?.get(name)?.as_bool()?;
            Some((name.to_string(), enabled))
        })
        .collect()
}

/// Best-effort configured readback, not a guarantee about startup-captured service state.
pub(super) async fn compatibility_warning(
    target: &AppServerTarget,
    config: &Config,
) -> Result<Option<String>, CompatibilityError> {
    let AppServerTarget::LocalDaemon {
        allow_embedded_fallback,
        ..
    } = target
    else {
        return Ok(None);
    };
    let mut restart_features = None;
    let check = async {
        // The feature-list RPC cannot report this process-scoped structured setting.
        if !config.features.enabled(Feature::CodeModeHost)
            && config.code_mode.disable_in_process_fallback
        {
            return Err("code-mode host fallback policy requires embedded mode".to_string());
        }
        let client = app_server_connection::connect(target)
            .await
            .map_err(|_| "could not connect to check daemon feature settings".to_string())?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        crate::experimental_features::fetch(
            client.request_handle(),
            /*thread_id*/ None,
            "tui-daemon-features",
            tx,
        );
        let result = rx.await;
        let _ = client.shutdown().await;
        let features = result.map_err(|_| "daemon feature check was interrupted".to_string())??;
        // A previous client may have launched this daemon with overrides, even if
        // this client has none. Check effective values, including defaults.
        for feature in SERVER_FEATURES {
            let name = feature.key();
            let enabled = config.features.enabled(feature);
            if features
                .iter()
                .find(|feature| feature.name == name)
                .is_some_and(|feature| feature.enabled)
                != enabled
            {
                restart_features = Some(
                    SERVER_FEATURES
                        .into_iter()
                        .map(|feature| {
                            (feature.key().to_string(), config.features.enabled(feature))
                        })
                        .collect(),
                );
                let state = if enabled { "enabled" } else { "disabled" };
                return Err(format!("This session requires {name} to be {state}"));
            }
        }
        Ok::<(), String>(())
    }
    .await;
    match check {
        Ok(()) => Ok(None),
        Err(reason) => {
            let daemon_identity = if matches!(
                target,
                AppServerTarget::LocalDaemon {
                    endpoint: RemoteAppServerEndpoint::UnixSocket { .. },
                    ..
                }
            ) {
                codex_app_server_daemon::run(codex_app_server_daemon::LifecycleCommand::Version)
                    .await
                    .ok()
                    .map(|output| {
                        format!(
                            "; daemon launcher {} is installed at version {}, while the running launcher version is {} (app-server version {})",
                            output.managed_codex_path.display(),
                            output.managed_codex_version.as_deref().unwrap_or("unknown"),
                            output
                                .running_managed_codex_version
                                .as_deref()
                                .unwrap_or("unknown"),
                            output.app_server_version.as_deref().unwrap_or("unknown")
                        )
                    })
            } else {
                None
            };
            let reason = match daemon_identity {
                Some(identity) => format!("{reason}{identity}"),
                None => reason,
            };
            if *allow_embedded_fallback {
                Ok(Some(format!(
                    "Running without the shared background server: {reason}."
                )))
            } else {
                Err(CompatibilityError {
                    reason,
                    restart_features,
                })
            }
        }
    }
}

/// Whether the implicit local daemon can reproduce this invocation's launch configuration.
pub(super) fn can_reuse_implicit_local_daemon(
    cli_kv_overrides: &[(String, toml::Value)],
    loader_overrides: &LoaderOverrides,
    strict_config: bool,
    has_non_replayable_launch_overrides: bool,
) -> bool {
    cli_kv_overrides.is_empty()
        && loader_overrides_are_default(loader_overrides)
        && !strict_config
        && !has_non_replayable_launch_overrides
}

/// Owns the initialized client for an implicitly selected local daemon.
///
/// Startup reuses the successful handshake instead of probing the socket with one connection
/// and opening a second connection after configuration loading.
pub(super) struct PreparedDefaultDaemon {
    pub(super) socket_path: AbsolutePathBuf,
    pub(super) app_server: AppServerClient,
}

pub(super) async fn connect_default_daemon(
    codex_home: &Path,
) -> std::io::Result<Option<PreparedDefaultDaemon>> {
    let socket_path = codex_app_server_client::app_server_control_socket_path(codex_home)
        .map_err(std::io::Error::other)?;
    match std::fs::metadata(socket_path.as_path()) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(std::io::Error::other(format!(
                "failed to inspect the existing local app-server daemon socket at `{}`; refusing to start a competing embedded server: {err}",
                socket_path.display()
            )));
        }
    }
    connect_daemon_at(socket_path).await.map(Some)
}

/// Connect to an already selected daemon socket without falling back to an embedded owner.
///
/// Frontend refresh markers use this path so a missing or broken shared daemon is reported to
/// the caller instead of starting a second server with different ownership semantics.
pub(super) async fn connect_daemon_at(
    socket_path: AbsolutePathBuf,
) -> std::io::Result<PreparedDefaultDaemon> {
    match std::fs::metadata(socket_path.as_path()) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "the selected local app-server daemon socket `{}` is no longer available; refusing to start a competing embedded server",
                    socket_path.display()
                ),
            ));
        }
        Err(err) => {
            return Err(std::io::Error::other(format!(
                "failed to inspect the existing local app-server daemon socket at `{}`; refusing to start a competing embedded server: {err}",
                socket_path.display()
            )));
        }
    }

    let target = AppServerTarget::LocalDaemon {
        allow_embedded_fallback: false,
        endpoint: RemoteAppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone(),
        },
    };
    let app_server = app_server_connection::connect(&target)
        .await
        .map_err(|err| {
            std::io::Error::other(format!(
                "failed to connect to the existing local app-server daemon at `{}`; refusing to start a competing embedded server: {err}",
                socket_path.display()
            ))
        })?;
    Ok(PreparedDefaultDaemon {
        socket_path,
        app_server,
    })
}

pub(super) fn launcher_update_issue(
    output: &codex_app_server_daemon::ApplyOutput,
) -> CompatibilityError {
    let launcher = output
        .managed_codex_path
        .as_deref()
        .unwrap_or_else(|| Path::new("unknown"));
    let error = output
        .error
        .as_deref()
        .unwrap_or("daemon update reconciliation did not complete");
    let recovery_guidance = match output.failure_kind {
        Some(codex_app_server_daemon::ApplyFailureKind::HandoffJournalMissing) => {
            "The matching handoff journal is missing; Codex preserved the receipt and did not replay or discard its saved sessions."
        }
        Some(codex_app_server_daemon::ApplyFailureKind::HandoffStorageMismatch) => {
            "The daemon and handoff use different Codex homes; Codex preserved the handoff and did not mutate either home."
        }
        Some(codex_app_server_daemon::ApplyFailureKind::HandoffWorkPending) => {
            "Codex preserved the pending handoff and its saved session ownership."
        }
        Some(codex_app_server_daemon::ApplyFailureKind::RunningLauncherMismatch) | None => {
            "The selected launcher is not verified as the running fork build."
        }
    };
    CompatibilityError {
        reason: format!(
            "safe daemon update returned {:?}; selected launcher {} (installed version {}) is running as version {} (app-server version {}). {recovery_guidance} {error}",
            output.status,
            launcher.display(),
            output.managed_codex_version.as_deref().unwrap_or("unknown"),
            output
                .running_managed_codex_version
                .as_deref()
                .unwrap_or("unknown"),
            output.app_server_version.as_deref().unwrap_or("unknown")
        ),
        restart_features: None,
    }
}
