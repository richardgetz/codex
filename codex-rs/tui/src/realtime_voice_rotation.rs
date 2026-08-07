//! Persistent round-robin selection for optional realtime voice rotations.

use codex_config::config_toml::RealtimeConfig;
use codex_protocol::protocol::RealtimeVoice;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

const ROTATION_STATE_FILE: &str = "realtime_voice_rotation.json";
const ROTATION_LOCK_FILE: &str = "realtime_voice_rotation.lock";
const LOCK_ATTEMPTS: usize = 50;
const STALE_LOCK_AFTER: Duration = Duration::from_secs(5);

#[derive(Debug, Default, Deserialize, Serialize)]
struct RotationState {
    voices: Vec<RealtimeVoice>,
    next_index: usize,
}

/// Selects and persists the next voice for a new Codex process.
///
/// An empty or absent rotation leaves the configured voice unchanged. State is kept under the
/// Codex home rather than in `config.toml`, so starting Codex does not rewrite user configuration.
/// A short-lived create-new lock keeps separate launches from consuming the same slot in normal
/// cases; if another process owns the lock, the configured voice is used for that launch.
pub(crate) fn select_startup_voice(
    config: &RealtimeConfig,
    codex_home: &Path,
) -> Option<RealtimeVoice> {
    let voices = config.voice_rotation.as_deref()?;
    if voices.is_empty() {
        return None;
    }

    let state_path = codex_home.join(ROTATION_STATE_FILE);
    let lock_path = codex_home.join(ROTATION_LOCK_FILE);
    let Some(lock) = acquire_lock(&lock_path) else {
        tracing::debug!(path = %lock_path.display(), "realtime voice rotation lock is busy");
        return config.voice;
    };

    let state = read_state(&state_path).unwrap_or_default();
    let next_index = if state.voices == voices {
        state.next_index % voices.len()
    } else {
        0
    };
    let selected = voices[next_index];
    let next_state = RotationState {
        voices: voices.to_vec(),
        next_index: (next_index + 1) % voices.len(),
    };
    if let Err(err) = write_state(&state_path, &next_state) {
        tracing::debug!(
            path = %state_path.display(),
            error = %err,
            "failed to persist realtime voice rotation state"
        );
    }
    drop(lock);
    let _ = fs::remove_file(&lock_path);
    Some(selected)
}

fn acquire_lock(path: &Path) -> Option<File> {
    for _ in 0..LOCK_ATTEMPTS {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(lock) => return Some(lock),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                if lock_is_stale(path) {
                    let _ = fs::remove_file(path);
                } else {
                    std::thread::yield_now();
                }
            }
            Err(err) => {
                tracing::debug!(path = %path.display(), error = %err, "failed to acquire realtime voice rotation lock");
                return None;
            }
        }
    }
    None
}

fn lock_is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .map_err(std::io::Error::other)
        })
        .is_ok_and(|age| age > STALE_LOCK_AFTER)
}

fn read_state(path: &Path) -> Option<RotationState> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_state(path: &Path, state: &RotationState) -> std::io::Result<()> {
    let temporary_path = temporary_path(path);
    let bytes = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    fs::write(&temporary_path, bytes)?;
    if let Err(err) = fs::rename(&temporary_path, path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(err);
    }
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut temporary_path = path.to_path_buf();
    let suffix = format!("{}.tmp", std::process::id());
    temporary_path.set_extension(suffix);
    temporary_path
}

#[cfg(test)]
#[path = "realtime_voice_rotation_tests.rs"]
mod tests;
