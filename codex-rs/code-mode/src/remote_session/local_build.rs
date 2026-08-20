use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use codex_utils_pty::SpawnedProcess;
use tokio::sync::mpsc::Receiver;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

const CODE_MODE_HOST_BINARY: &str = "codex-code-mode-host";
const RUN_LOCAL_CARGO_SCRIPT: &str = "scripts/run_local_cargo.py";
const LOCAL_BUILD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const LOCAL_BUILD_OUTPUT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BUILD_ERROR_BYTES: usize = 4_096;

#[derive(Clone, Debug, Eq, PartialEq)]
enum CargoProfile {
    Debug,
    Release,
    Named(String),
}

struct LocalCodeModeHostBuild {
    manifest_dir: PathBuf,
    wrapper: PathBuf,
    target_dir: PathBuf,
    target: Option<String>,
    profile: CargoProfile,
}

fn build_spec(host_program: &Path) -> Option<LocalCodeModeHostBuild> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .to_path_buf();
    if !manifest_dir.join("Cargo.toml").is_file()
        || !manifest_dir
            .parent()
            .is_some_and(|parent| parent.join(RUN_LOCAL_CARGO_SCRIPT).is_file())
    {
        return None;
    }
    let host_program = absolute_path(host_program)?;
    let default_target_dir = manifest_dir.join("target");
    let configured_target_dir = configured_target_dir();
    let target_dir = if host_program.starts_with(&default_target_dir) {
        default_target_dir
    } else {
        configured_target_dir.filter(|target_dir| host_program.starts_with(target_dir))?
    };
    let relative_components = host_program
        .strip_prefix(&target_dir)
        .ok()?
        .components()
        .collect::<Vec<_>>();
    let (target, profile_component) = match relative_components.as_slice() {
        [profile, _] => (None, profile.as_os_str()),
        [target, profile, _] => (
            Some(target.as_os_str().to_str()?.to_string()),
            profile.as_os_str(),
        ),
        _ => return None,
    };
    let profile_dir = host_program.parent()?;
    if profile_dir.file_name() != Some(profile_component) {
        return None;
    }
    let wrapper = manifest_dir.parent()?.join(RUN_LOCAL_CARGO_SCRIPT);

    let profile_name = profile_dir.file_name()?.to_str()?;
    let profile = match profile_name {
        "debug" => CargoProfile::Debug,
        "release" => CargoProfile::Release,
        name => CargoProfile::Named(name.to_string()),
    };
    Some(LocalCodeModeHostBuild {
        manifest_dir,
        wrapper,
        target_dir,
        target,
        profile,
    })
}

pub(super) fn can_build(host_program: &Path) -> bool {
    build_spec(host_program).is_some()
}

pub(super) async fn ensure(
    host_program: &Path,
    cancellation: CancellationToken,
) -> Result<(), String> {
    if host_program.is_file() {
        return Ok(());
    }
    let Some(build) = build_spec(host_program) else {
        return Ok(());
    };

    let python = if cfg!(windows) { "python" } else { "python3" };
    let mut args = vec![
        build.wrapper.to_string_lossy().into_owned(),
        "build".to_string(),
        "--bin".to_string(),
        CODE_MODE_HOST_BINARY.to_string(),
        "--target-dir".to_string(),
        build.target_dir.to_string_lossy().into_owned(),
    ];
    if let Some(target) = &build.target {
        args.extend(["--target".to_string(), target.clone()]);
    }
    match build.profile {
        CargoProfile::Debug => {}
        CargoProfile::Release => {
            args.push("--release".to_string());
        }
        CargoProfile::Named(name) => {
            args.extend(["--profile".to_string(), name]);
        }
    }

    let mut environment = env::vars().collect::<HashMap<_, _>>();
    if build.target.is_none() {
        // The executable path is in target/<profile>, so do not let a stale
        // process-wide target override make Cargo write the replacement host
        // under target/<target>/<profile> instead.
        environment.remove("CARGO_BUILD_TARGET");
    }
    let arg0 = None;
    let SpawnedProcess {
        session,
        mut stdout_rx,
        stderr_rx,
        mut exit_rx,
    } = codex_utils_pty::spawn_pipe_process_no_stdin(
        python,
        &args,
        &build.manifest_dir,
        &environment,
        &arg0,
        &[],
    )
    .await
    .map_err(|err| format!("could not start the local Cargo build: {err:#}"))?;
    let mut stdout_task = tokio::spawn(async move { while stdout_rx.recv().await.is_some() {} });
    let mut stderr_task = tokio::spawn(read_bounded_stderr(stderr_rx));
    let mut session = Some(session);
    let exit_code = tokio::select! {
        _ = cancellation.cancelled() => {
            drop(session.take());
            stdout_task.abort();
            stderr_task.abort();
            return Err("local Cargo build was cancelled".to_string());
        }
        _ = sleep(LOCAL_BUILD_TIMEOUT) => {
            drop(session.take());
            stdout_task.abort();
            stderr_task.abort();
            return Err(format!(
                "local Cargo build timed out after {} seconds",
                LOCAL_BUILD_TIMEOUT.as_secs()
            ));
        }
        exit_code = &mut exit_rx => exit_code
            .map_err(|err| format!("local Cargo build exited without a status: {err}"))?,
    };
    drop(session.take());
    let stderr = match tokio::time::timeout(LOCAL_BUILD_OUTPUT_TIMEOUT, &mut stderr_task).await {
        Ok(Ok(stderr)) => stderr,
        Ok(Err(err)) => {
            stdout_task.abort();
            return Err(format!(
                "failed to collect local Cargo build diagnostics: {err}"
            ));
        }
        Err(_) => {
            stderr_task.abort();
            stdout_task.abort();
            return Err(format!(
                "local Cargo build output did not close within {} seconds",
                LOCAL_BUILD_OUTPUT_TIMEOUT.as_secs()
            ));
        }
    };
    if tokio::time::timeout(LOCAL_BUILD_OUTPUT_TIMEOUT, &mut stdout_task)
        .await
        .is_err()
    {
        stdout_task.abort();
        return Err(format!(
            "local Cargo build output did not close within {} seconds",
            LOCAL_BUILD_OUTPUT_TIMEOUT.as_secs()
        ));
    }
    if exit_code != 0 {
        let stderr = String::from_utf8_lossy(&stderr);
        let detail = stderr.trim();
        let detail = detail.trim_start_matches("\n");
        return Err(if detail.is_empty() {
            format!("local Cargo build exited with status {exit_code}")
        } else {
            format!("local Cargo build exited with status {exit_code}: {detail}")
        });
    }
    if !host_program.is_file() {
        return Err(format!(
            "local Cargo build completed, but `{}` was not produced",
            host_program.display()
        ));
    }
    Ok(())
}

async fn read_bounded_stderr(mut stderr_rx: Receiver<Vec<u8>>) -> Vec<u8> {
    let mut stderr = Vec::new();
    while let Some(chunk) = stderr_rx.recv().await {
        stderr.extend_from_slice(&chunk);
        if stderr.len() > MAX_BUILD_ERROR_BYTES {
            let remove = stderr.len() - MAX_BUILD_ERROR_BYTES;
            stderr.drain(..remove);
        }
    }
    stderr
}

fn absolute_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

fn configured_target_dir() -> Option<PathBuf> {
    let target_dir = env::var_os("CARGO_TARGET_DIR")?;
    let target_dir = PathBuf::from(target_dir);
    absolute_path(&target_dir)
}

#[cfg(test)]
#[path = "local_build_tests.rs"]
mod tests;
