//! Local daemon launch policy. Explicit embedded launches never discover or start a daemon;
//! optional attachment may fall back to embedded mode, while automatic launches
//! require a compatible shared server and a successful connection, except when
//! the Windows launcher forbids detaching a missing server. Elevated local
//! Windows sessions use explicit embedded behavior before discovery or startup.

use super::*;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigRequirementsReadResponse;
use codex_app_server_protocol::RequestId;
use std::collections::BTreeMap;
use std::path::Path;

const SERVER_FEATURES: [Feature; 4] = [
    Feature::ApiKeyModelDiscovery,
    Feature::CodeModeHost,
    Feature::AuthElicitation,
    Feature::McpOAuthRefreshCoordination,
];

pub(super) const FAILURE_HINT: &str = "To work without the background server, rerun the same command with --no-daemon (including resume or fork and its arguments).";
pub(super) const WSL_DRVFS_EXCLUSION: &str = "a Windows-mounted WSL CODEX_HOME (DrvFS/9p)";

/// Returns whether `codex_home` is on a Windows-mounted WSL filesystem.
pub fn uses_wsl_drvfs(codex_home: &std::path::Path) -> bool {
    // The managed daemon writes an executable and Unix-style state beneath CODEX_HOME.
    // DrvFS does not reliably support the required permission semantics, so starting it
    // there can fail before the TUI opens.
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        if !codex_utils_path::is_wsl() && std::env::var_os("WSL_INTEROP").is_none() {
            return false;
        }
        let Some(directory) = codex_home
            .ancestors()
            .find_map(|path| std::fs::File::open(path).ok())
        else {
            return false;
        };
        let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::fstatfs(directory.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
            return false;
        }
        unsafe { stats.assume_init() }.f_type as u64 == 0x0102_1997
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = codex_home;
        false
    }
}

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
    cli_kv_overrides: &[(String, toml::Value)],
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
        let client = app_server_connection::connect(target)
            .await
            .map_err(|_| "could not connect to check daemon feature settings".to_string())?;
        // File settings and defaults belong to the daemon until it restarts.
        // Only explicit invocation flags can request a shared-service restart.
        let mut requested = server_features(cli_kv_overrides);
        if requested.is_empty() {
            let _ = client.shutdown().await;
            return Ok(());
        }
        // Both legacy managed config and current requirements override CLI flags.
        let layers = crate::config_update::read_effective_config_if_supported(
            client.request_handle(),
            config.cwd.as_path(),
        )
        .await
        .map_err(|_| "could not read daemon configuration".to_string())?
        .and_then(|config| config.layers)
        .unwrap_or_default();
        for layer in layers.iter().filter(|layer| {
            layer["disabledReason"].is_null()
                && matches!(
                    layer["name"]["type"].as_str(),
                    Some("legacyManagedConfigTomlFromFile" | "legacyManagedConfigTomlFromMdm")
                )
        }) {
            requested.retain(|name, _| layer["config"]["features"].get(name).is_none());
        }
        match client
            .request_handle()
            .request_typed::<ConfigRequirementsReadResponse>(
                ClientRequest::ConfigRequirementsRead {
                    request_id: RequestId::String("tui-daemon-requirements".to_string()),
                    params: None,
                },
            )
            .await
        {
            Ok(response) => {
                if let Some(required) = response.requirements.and_then(|r| r.feature_requirements) {
                    requested.retain(|name, _| !required.contains_key(name));
                }
            }
            Err(TypedRequestError::Server { source, .. })
                if source.code == -32601
                    || source.code == -32600
                        && source.message.contains("configRequirements/read")
                        && (source.message.contains("unknown variant")
                            || source.message.contains("unknown method")) => {}
            Err(_) => return Err("could not check daemon configuration requirements".to_string()),
        }
        if requested.is_empty() {
            let _ = client.shutdown().await;
            return Ok(());
        }
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
        for (name, enabled) in &requested {
            if features
                .iter()
                .find(|feature| feature.name == *name)
                .is_some_and(|feature| feature.enabled)
                != *enabled
            {
                let state = if *enabled { "enabled" } else { "disabled" };
                let reason = format!("This session requires {name} to be {state}");
                restart_features = Some(requested);
                return Err(reason);
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
