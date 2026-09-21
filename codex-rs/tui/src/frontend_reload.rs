//! Frontend replacement state and launcher helpers.
//!
//! This module owns the inherited reload marker, exact-session context, and the
//! launcher used to re-enter Codex after a durable app-server handoff. Keeping
//! this protocol separate from startup orchestration makes its all-or-nothing
//! validation and no-replay rules easier to review.

use super::*;

/// Restore terminal modes before a fatal startup exit bypasses destructor cleanup.
pub(crate) fn restore_terminal_before_fatal_exit() {
    if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
        let _ = tui::restore_after_exit();
    }
}

const FRONTEND_RELOAD_THREAD_ENV: &str = "CODEX_TUI_RELOAD_THREAD_ID";
const FRONTEND_RELOAD_ACCOUNT_ENV: &str = "CODEX_TUI_RELOAD_ACCOUNT_ALIAS";
const FRONTEND_RELOAD_CWD_ENV: &str = "CODEX_TUI_RELOAD_CWD";
const FRONTEND_RELOAD_PROVIDER_ENV: &str = "CODEX_TUI_RELOAD_MODEL_PROVIDER";
const FRONTEND_RELOAD_MODEL_ENV: &str = "CODEX_TUI_RELOAD_MODEL";
const FRONTEND_RELOAD_REASONING_ENV: &str = "CODEX_TUI_RELOAD_REASONING_EFFORT";
const FRONTEND_RELOAD_SERVICE_TIER_ENV: &str = "CODEX_TUI_RELOAD_SERVICE_TIER";
const FRONTEND_RELOAD_HANDOFF_ENV: &str = "CODEX_TUI_RELOAD_HANDOFF_ID";
const FRONTEND_RELOAD_LAUNCHER_ENV: &str = "CODEX_TUI_RELOAD_LAUNCHER";
const FRONTEND_RELOAD_LOCAL_DAEMON_SOCKET_ENV: &str = "CODEX_TUI_RELOAD_LOCAL_DAEMON_SOCKET";
const FRONTEND_LAUNCHER_ENV: &str = "CODEX_TUI_FRONTEND_LAUNCHER";
const MANAGED_PACKAGE_ROOT_ENV: &str = "CODEX_MANAGED_PACKAGE_ROOT";

fn validate_frontend_reload_handoff_id(handoff_id: &str) -> std::io::Result<()> {
    if handoff_id.is_empty()
        || !handoff_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload handoff id must contain only letters, numbers, `-`, or `_`",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrontendReloadContext {
    thread_id: String,
    account_alias: Option<String>,
    cwd: PathBuf,
    model_provider: Option<String>,
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    handoff_id: Option<String>,
    launcher: Option<PathBuf>,
    local_daemon_socket: Option<PathBuf>,
}

pub(crate) fn launcher_is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn resolve_frontend_launcher_from_inputs(
    configured: Option<&Path>,
    argv0: Option<&Path>,
    managed_package_root: bool,
) -> Option<PathBuf> {
    if let Some(configured) = configured {
        return (configured.is_absolute() && launcher_is_executable(configured))
            .then(|| configured.to_path_buf());
    }

    // A package-managed child may expose only its versioned vendor binary as argv[0]. The
    // wrapper's package-root marker records that provenance so a platform without a
    // spawnable stable shim refuses to prepare a durable handoff instead of re-executing a stale
    // binary after an upgrade.
    if managed_package_root {
        return None;
    }

    if let Some(argv0) = argv0
        && argv0.is_absolute()
        && launcher_is_executable(argv0)
    {
        return Some(argv0.to_path_buf());
    }

    // A native child launched through npm may report the versioned vendor binary as both
    // `argv[0]` and `current_exe()`. Re-executing that path after an upgrade can select a removed
    // or stale binary, so an explicit launcher or native absolute argv[0] is required.
    None
}

pub(crate) fn resolve_frontend_launcher() -> Option<PathBuf> {
    let configured = std::env::var_os(FRONTEND_LAUNCHER_ENV).map(PathBuf::from);
    let argv0 = std::env::args_os().next().map(PathBuf::from);
    let managed_package_root = std::env::var_os(MANAGED_PACKAGE_ROOT_ENV).is_some();
    resolve_frontend_launcher_from_inputs(
        configured.as_deref(),
        argv0.as_deref(),
        managed_package_root,
    )
}

pub(crate) fn take_frontend_reload_context() -> std::io::Result<Option<FrontendReloadContext>> {
    let thread = std::env::var_os(FRONTEND_RELOAD_THREAD_ENV);
    let account = std::env::var_os(FRONTEND_RELOAD_ACCOUNT_ENV);
    let cwd = std::env::var_os(FRONTEND_RELOAD_CWD_ENV);
    let model_provider = std::env::var_os(FRONTEND_RELOAD_PROVIDER_ENV);
    let model = std::env::var_os(FRONTEND_RELOAD_MODEL_ENV);
    let reasoning_effort = std::env::var_os(FRONTEND_RELOAD_REASONING_ENV);
    let service_tier = std::env::var_os(FRONTEND_RELOAD_SERVICE_TIER_ENV);
    let handoff_id = std::env::var_os(FRONTEND_RELOAD_HANDOFF_ENV);
    let launcher = std::env::var_os(FRONTEND_RELOAD_LAUNCHER_ENV);
    let local_daemon_socket = std::env::var_os(FRONTEND_RELOAD_LOCAL_DAEMON_SOCKET_ENV);
    if thread.is_none()
        && account.is_none()
        && cwd.is_none()
        && model_provider.is_none()
        && model.is_none()
        && reasoning_effort.is_none()
        && service_tier.is_none()
        && handoff_id.is_none()
        && launcher.is_none()
        && local_daemon_socket.is_none()
    {
        return Ok(None);
    }

    let thread = thread.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload marker omitted its thread id",
        )
    })?;
    let thread = thread.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload marker contained a non-UTF-8 thread id",
        )
    })?;
    ThreadId::from_string(thread).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Codex frontend reload marker contained an invalid thread id: {error}"),
        )
    })?;

    let account = account.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload marker omitted its account alias",
        )
    })?;
    let account = account.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload marker contained a non-UTF-8 account alias",
        )
    })?;
    let cwd = cwd.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload marker omitted its working directory",
        )
    })?;
    let cwd = PathBuf::from(cwd);
    if !cwd.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Codex frontend reload marker contained a non-absolute working directory `{}`",
                cwd.display()
            ),
        ));
    }

    let model_provider = model_provider
        .map(|value| {
            value.to_str().map(str::to_owned).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Codex frontend reload marker contained a non-UTF-8 model provider",
                )
            })
        })
        .transpose()?
        .filter(|value| !value.is_empty());

    let model = model.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload marker omitted the current model",
        )
    })?;
    let model = model.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload marker contained a non-UTF-8 model",
        )
    })?;
    if model.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload marker contained an empty model",
        ));
    }
    let reasoning_effort = reasoning_effort
        .map(|value| {
            value.to_str().map(str::to_owned).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Codex frontend reload marker contained a non-UTF-8 reasoning effort",
                )
            })
        })
        .transpose()?
        .filter(|value| !value.is_empty());
    let service_tier = service_tier
        .map(|value| {
            value.to_str().map(str::to_owned).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Codex frontend reload marker contained a non-UTF-8 service tier",
                )
            })
        })
        .transpose()?
        .filter(|value| !value.is_empty());
    let handoff_id = handoff_id
        .map(|value| {
            value.to_str().map(str::to_owned).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Codex frontend reload marker contained a non-UTF-8 handoff id",
                )
            })
        })
        .transpose()?
        .filter(|value| !value.is_empty());
    let launcher = launcher
        .map(|value| {
            let launcher = PathBuf::from(value);
            if !launcher.is_absolute() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "Codex frontend reload marker contained a non-absolute launcher `{}`",
                        launcher.display()
                    ),
                ));
            }
            Ok(launcher)
        })
        .transpose()?;
    let local_daemon_socket = local_daemon_socket
        .map(PathBuf::from)
        .map(|socket| {
            if !socket.is_absolute() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "Codex frontend reload marker contained a non-absolute local daemon socket `{}`",
                        socket.display()
                    ),
                ));
            }
            Ok(socket)
        })
        .transpose()?;
    if handoff_id.is_some() != launcher.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload marker must include both handoff id and launcher",
        ));
    }
    if handoff_id.is_some() && model_provider.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload marker omitted the effective model provider for its handoff",
        ));
    }
    if handoff_id.is_some() && local_daemon_socket.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Codex frontend reload marker cannot combine an embedded handoff with a local daemon socket",
        ));
    }
    if let Some(handoff_id) = handoff_id.as_deref() {
        validate_frontend_reload_handoff_id(handoff_id).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("Codex frontend reload marker contained an invalid handoff id: {error}"),
            )
        })?;
    }
    if let Some(launcher) = launcher.as_deref()
        && !launcher_is_executable(launcher)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Codex frontend reload marker contained a launcher that is not executable: `{}`",
                launcher.display()
            ),
        ));
    }

    // Consume markers only after every field has been validated. A malformed inherited
    // environment must not clear the caller's retry context or accidentally select a different
    // session when startup is retried.
    unsafe {
        std::env::remove_var(FRONTEND_RELOAD_THREAD_ENV);
        std::env::remove_var(FRONTEND_RELOAD_ACCOUNT_ENV);
        std::env::remove_var(FRONTEND_RELOAD_CWD_ENV);
        std::env::remove_var(FRONTEND_RELOAD_PROVIDER_ENV);
        std::env::remove_var(FRONTEND_RELOAD_MODEL_ENV);
        std::env::remove_var(FRONTEND_RELOAD_REASONING_ENV);
        std::env::remove_var(FRONTEND_RELOAD_SERVICE_TIER_ENV);
        std::env::remove_var(FRONTEND_RELOAD_HANDOFF_ENV);
        std::env::remove_var(FRONTEND_RELOAD_LAUNCHER_ENV);
        std::env::remove_var(FRONTEND_RELOAD_LOCAL_DAEMON_SOCKET_ENV);
    }

    Ok(Some(FrontendReloadContext {
        thread_id: thread.to_string(),
        account_alias: (!account.is_empty()).then(|| account.to_string()),
        cwd,
        model_provider,
        model: model.to_string(),
        reasoning_effort,
        service_tier,
        handoff_id,
        launcher,
        local_daemon_socket,
    }))
}

pub(crate) fn apply_frontend_reload_context(cli: &mut Cli, context: FrontendReloadContext) {
    cli.resume_picker = false;
    cli.resume_last = false;
    cli.resume_session_id = Some(context.thread_id);
    cli.resume_show_all = false;
    cli.resume_include_non_interactive = false;
    // A `codex fork` or `codex agents` invocation may have populated one of these internal
    // startup modes before the handoff marker was consumed.  The exact displayed thread must
    // win on re-entry; otherwise startup orchestration can fork a second thread or reopen the
    // daemon overview instead of resuming the selected thread.
    cli.agents_overview = false;
    cli.fork_picker = false;
    cli.fork_last = false;
    cli.fork_session_id = None;
    cli.fork_show_all = false;
    // The original launch mode may have requested a new worktree or an OSS provider selection.
    // Those operations belong to the old session and must not run before the durable handoff is
    // recovered by the replacement embedded server.
    cli.shared.worktree = false;
    cli.shared.oss = false;
    cli.shared.oss_provider = None;
    // The original invocation may have carried a prompt or image arguments. They were already
    // submitted before the daemon handoff and must never be replayed by the replacement process.
    cli.prompt = None;
    cli.images.clear();
    cli.cwd = Some(context.cwd);
    if let Some(model_provider) = context.model_provider {
        cli.config_overrides.raw_overrides.push(format!(
            "model_provider={}",
            toml_string_literal(&model_provider)
        ));
    }
    cli.model = Some(context.model);
    if let Some(effort) = context.reasoning_effort {
        cli.config_overrides.raw_overrides.push(format!(
            "model_reasoning_effort={}",
            toml_string_literal(&effort)
        ));
    }
    if let Some(service_tier) = context.service_tier {
        cli.config_overrides.raw_overrides.push(format!(
            "service_tier={}",
            toml_string_literal(&service_tier)
        ));
    }
    // Account switching is session-local, so restore the effective alias rather than the alias
    // that happened to be present in the original process arguments.
    cli.startup_account_alias = context.account_alias;
    cli.frontend_reload_handoff_id = context.handoff_id;
    cli.frontend_reload_thread_id = None;
    cli.frontend_launcher = context.launcher;
    cli.frontend_reload_local_daemon_socket = context.local_daemon_socket;
}

pub(crate) fn apply_frontend_reload_cli_args(cli: &mut Cli) -> std::io::Result<()> {
    let Some(handoff_id) = cli.frontend_reload_handoff_id.as_deref() else {
        if cli.frontend_reload_thread_id.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--recover-handoff-thread requires --recover-handoff",
            ));
        }
        return Ok(());
    };
    validate_frontend_reload_handoff_id(handoff_id)?;

    if let Some(thread_id) = cli.frontend_reload_thread_id.take() {
        ThreadId::from_string(&thread_id).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("--recover-handoff-thread contained an invalid thread id: {error}"),
            )
        })?;
        if let Some(existing_thread_id) = cli.resume_session_id.as_deref()
            && existing_thread_id != thread_id
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--recover-handoff-thread conflicts with the selected resume session",
            ));
        }
        cli.resume_session_id = Some(thread_id);
    }

    if cli.resume_session_id.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--recover-handoff requires --recover-handoff-thread so Codex can reattach the exact session",
        ));
    }
    if cli.prompt.is_some() || !cli.images.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--recover-handoff cannot be combined with a prompt or images; recovery resumes the saved turn",
        ));
    }
    Ok(())
}

fn display_frontend_reload_argument(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:/@+".contains(character))
    {
        return value.to_string();
    }
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('`', "\\`")
            .replace('!', "\\!")
    )
}

pub(crate) fn frontend_reload_recovery_command(
    launcher: &Path,
    handoff_id: &str,
    thread_id: ThreadId,
    account_alias: Option<&str>,
    cwd: &Path,
    model_provider: Option<&str>,
    model: &str,
    reasoning_effort: Option<&str>,
    service_tier: Option<&str>,
) -> String {
    let mut args = vec![
        display_frontend_reload_argument(&launcher.display().to_string()),
        "--recover-handoff".to_string(),
        display_frontend_reload_argument(handoff_id),
        "--recover-handoff-thread".to_string(),
        display_frontend_reload_argument(&thread_id.to_string()),
        "--cd".to_string(),
        display_frontend_reload_argument(&cwd.display().to_string()),
    ];
    if let Some(account_alias) = account_alias {
        args.extend([
            "--account".to_string(),
            display_frontend_reload_argument(account_alias),
        ]);
    }
    args.extend([
        "--model".to_string(),
        display_frontend_reload_argument(model),
    ]);
    if let Some(model_provider) = model_provider {
        args.extend([
            "-c".to_string(),
            display_frontend_reload_argument(&format!(
                "model_provider={}",
                toml_string_literal(model_provider)
            )),
        ]);
    }
    if let Some(reasoning_effort) = reasoning_effort {
        args.extend([
            "-c".to_string(),
            display_frontend_reload_argument(&format!(
                "model_reasoning_effort={}",
                toml_string_literal(reasoning_effort)
            )),
        ]);
    }
    if let Some(service_tier) = service_tier {
        args.extend([
            "-c".to_string(),
            display_frontend_reload_argument(&format!(
                "service_tier={}",
                toml_string_literal(service_tier)
            )),
        ]);
    }
    args.join(" ")
}

fn toml_string_literal(value: &str) -> String {
    // JSON string escaping is compatible with TOML basic strings and handles quotes, control
    // characters, and arbitrary Unicode without interpolating malformed config overrides.
    serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
}

fn frontend_reload_args<I>(args: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    args.into_iter().skip(1).collect()
}

fn build_frontend_reload_command(
    launcher: &Path,
    context: &FrontendReloadContext,
    args: Vec<std::ffi::OsString>,
) -> std::process::Command {
    let mut command = std::process::Command::new(launcher);
    command
        .args(args)
        .env(FRONTEND_RELOAD_THREAD_ENV, &context.thread_id)
        .env(
            FRONTEND_RELOAD_ACCOUNT_ENV,
            context.account_alias.as_deref().unwrap_or_default(),
        )
        .env(FRONTEND_RELOAD_CWD_ENV, &context.cwd)
        .env(
            FRONTEND_RELOAD_PROVIDER_ENV,
            context.model_provider.as_deref().unwrap_or_default(),
        )
        .env(FRONTEND_RELOAD_MODEL_ENV, &context.model)
        .env(
            FRONTEND_RELOAD_REASONING_ENV,
            context.reasoning_effort.as_deref().unwrap_or_default(),
        )
        .env(
            FRONTEND_RELOAD_SERVICE_TIER_ENV,
            context.service_tier.as_deref().unwrap_or_default(),
        );
    if let Some(handoff_id) = context.handoff_id.as_deref() {
        command.env(FRONTEND_RELOAD_HANDOFF_ENV, handoff_id);
    }
    if let Some(launcher) = context.launcher.as_deref() {
        command.env(FRONTEND_RELOAD_LAUNCHER_ENV, launcher);
    }
    if let Some(socket) = context.local_daemon_socket.as_deref() {
        command.env(FRONTEND_RELOAD_LOCAL_DAEMON_SOCKET_ENV, socket);
    }
    command
}

pub(crate) fn reexec_frontend(
    launcher: &Path,
    thread_id: ThreadId,
    account_alias: Option<String>,
    cwd: PathBuf,
    model_provider: Option<String>,
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    handoff_id: Option<String>,
) -> std::io::Error {
    reexec_frontend_with_local_daemon_socket(
        launcher,
        thread_id,
        account_alias,
        cwd,
        model_provider,
        model,
        reasoning_effort,
        service_tier,
        handoff_id,
        None,
    )
}

pub(crate) fn reexec_frontend_with_local_daemon_socket(
    launcher: &Path,
    thread_id: ThreadId,
    account_alias: Option<String>,
    cwd: PathBuf,
    model_provider: Option<String>,
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    handoff_id: Option<String>,
    local_daemon_socket: Option<PathBuf>,
) -> std::io::Error {
    let marker_launcher = handoff_id.as_ref().map(|_| launcher.to_path_buf());
    let context = FrontendReloadContext {
        thread_id: thread_id.to_string(),
        account_alias,
        cwd,
        model_provider,
        model,
        reasoning_effort,
        service_tier,
        handoff_id,
        launcher: marker_launcher,
        local_daemon_socket,
    };
    let args = frontend_reload_args(std::env::args_os());
    let mut command = build_frontend_reload_command(launcher, &context, args);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.exec()
    }
    #[cfg(windows)]
    {
        match command.status() {
            Ok(status) if status.success() => std::process::exit(0),
            Ok(status) => std::io::Error::other(format!(
                "replacement Codex frontend exited with status {status}"
            )),
            Err(error) => error,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = command;
        std::io::Error::other("automatic Codex frontend reload is unsupported on this platform")
    }
}

#[cfg(test)]
#[path = "frontend_reload_tests.rs"]
mod tests;
