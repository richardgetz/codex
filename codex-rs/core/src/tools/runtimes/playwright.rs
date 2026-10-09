use codex_protocol::permissions::FileSystemSandboxPolicy;
#[cfg(unix)]
use codex_shell_command::bash::parse_shell_script_into_commands;
use codex_utils_absolute_path::AbsolutePathBuf;
#[cfg(unix)]
use std::path::Path;

/// Resolve a plain Playwright CLI script to a direct executable command.
///
/// The browser exception must not turn a shell wrapper, pipeline, redirect, or
/// command substitution into an unsandboxed execution path. The tree-sitter
/// parser rejects those constructs before we inspect the executable name. The
/// returned argv intentionally omits the shell wrapper so no shell startup
/// files, aliases, functions, or snapshots run outside the sandbox.
/// This exception is intentionally Unix-only; other hosts continue to use the
/// normal process sandbox until they have an equivalent direct-command path.
pub(crate) fn resolve_direct_playwright_cli_script(
    script: &str,
    configured_path: Option<&AbsolutePathBuf>,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    cwd: &AbsolutePathBuf,
) -> Option<Vec<String>> {
    #[cfg(unix)]
    {
        let commands = parse_shell_script_into_commands(script)?;
        let [single_command] = commands.as_slice() else {
            return None;
        };
        resolve_playwright_cli_words(
            single_command,
            configured_path,
            file_system_sandbox_policy,
            cwd.as_path(),
        )
    }

    #[cfg(not(unix))]
    {
        let _ = (script, configured_path, file_system_sandbox_policy, cwd);
        None
    }
}

#[cfg(unix)]
fn resolve_playwright_cli_words(
    command: &[String],
    configured_path: Option<&AbsolutePathBuf>,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    cwd: &Path,
) -> Option<Vec<String>> {
    let program = command.first()?;
    let program_name = Path::new(program).file_name()?.to_str()?;
    if program_name != "playwright-cli" {
        return None;
    }

    if configured_path.is_some_and(|path| {
        path.as_path()
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| name != "playwright-cli")
    }) {
        return None;
    }

    let resolved_program = if program.contains('/') {
        if let Some(configured_path) = configured_path {
            let resolved_configured_path = resolve_playwright_cli_path(
                configured_path.as_path(),
                file_system_sandbox_policy,
                cwd,
                /*require_trusted_path*/ false,
            )?;
            (configured_path.as_path() == Path::new(program)
                || resolved_configured_path == program.as_str())
            .then_some(resolved_configured_path)?
        } else {
            resolve_playwright_cli_path(
                Path::new(program),
                file_system_sandbox_policy,
                cwd,
                /*require_trusted_path*/ true,
            )?
        }
    } else if let Some(configured_path) = configured_path {
        resolve_playwright_cli_path(
            configured_path.as_path(),
            file_system_sandbox_policy,
            cwd,
            /*require_trusted_path*/ false,
        )?
    } else {
        let resolved_path = which::which(program).ok()?;
        resolve_playwright_cli_path(
            &resolved_path,
            file_system_sandbox_policy,
            cwd,
            /*require_trusted_path*/ true,
        )?
    };

    let mut command = command.to_vec();
    command[0] = resolved_program;
    Some(command)
}

#[cfg(unix)]
fn is_trusted_playwright_cli_path(path: &Path) -> bool {
    const TRUSTED_PLAYWRIGHT_ROOTS: &[&str] = &[
        "/bin",
        "/usr/bin",
        "/usr/local",
        "/opt/homebrew/bin",
        "/opt/homebrew",
        "/opt/local/bin",
        "/opt/local",
    ];

    TRUSTED_PLAYWRIGHT_ROOTS
        .iter()
        .map(Path::new)
        .any(|trusted_dir| path.starts_with(trusted_dir))
}

#[cfg(unix)]
fn resolve_playwright_cli_path(
    path: &Path,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    cwd: &Path,
    require_trusted_path: bool,
) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return None;
    }

    let canonical_path = path.canonicalize().ok()?;
    let canonical_metadata = std::fs::metadata(&canonical_path).ok()?;
    if !canonical_metadata.is_file() || canonical_metadata.permissions().mode() & 0o111 == 0 {
        return None;
    }

    if is_agent_writable_path(path, file_system_sandbox_policy, cwd)
        || is_agent_writable_path(&canonical_path, file_system_sandbox_policy, cwd)
    {
        return None;
    }

    if require_trusted_path && !is_trusted_playwright_cli_path(&canonical_path) {
        return None;
    }

    Some(canonical_path.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn is_agent_writable_path(
    path: &Path,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    cwd: &Path,
) -> bool {
    let writable_roots = file_system_sandbox_policy.get_writable_roots_with_cwd(cwd);
    file_system_sandbox_policy.can_write_path_with_cwd(path, cwd)
        || writable_roots
            .iter()
            .any(|writable_root| writable_root.is_path_writable(path))
}
