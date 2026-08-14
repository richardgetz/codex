//! Persistent round-robin selection for optional realtime voice rotations.

use codex_config::config_toml::RealtimeConfig;
use codex_protocol::protocol::RealtimeVoice;
use codex_protocol::protocol::RealtimeVoicesList;
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

use crate::realtime_voice_effects::load_named_preset;
use crate::realtime_voice_profiles::load_named_profile;

const ROTATION_STATE_FILE: &str = "realtime_voice_rotation.json";
const ROTATION_LOCK_FILE: &str = "realtime_voice_rotation.lock";
const LOCK_ATTEMPTS: usize = 50;
const STALE_LOCK_AFTER: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum RotationEntry {
    Voice(RealtimeVoice),
    Profile(String),
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RotationState {
    entries: Vec<RotationEntry>,
    next_index: usize,
}

#[derive(Debug, Deserialize)]
struct LegacyRotationState {
    voices: Vec<RealtimeVoice>,
    next_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedVoice {
    pub(crate) voice: RealtimeVoice,
    pub(crate) profile: Option<String>,
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
    select_startup_selection(config, codex_home).map(|selection| selection.voice)
}

pub(crate) fn select_startup_selection(
    config: &RealtimeConfig,
    codex_home: &Path,
) -> Option<SelectedVoice> {
    let voices = config
        .voice_rotation
        .as_deref()
        .unwrap_or_default()
        .iter()
        .copied()
        .filter_map(|voice| {
            if RealtimeVoicesList::builtin().v1.contains(&voice) {
                Some(RotationEntry::Voice(voice))
            } else {
                tracing::warn!(
                    voice = voice.wire_name(),
                    "ignoring unavailable voice in realtime voice rotation"
                );
                None
            }
        })
        .chain(
            config
                .voice_profile_rotation
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter_map(|name| {
                    let profile = match load_named_profile(codex_home, name) {
                        Ok(profile) => profile,
                        Err(err) => {
                            tracing::warn!(
                                profile = name,
                                error = %err,
                                "ignoring invalid voice profile in realtime voice rotation"
                            );
                            return None;
                        }
                    };
                    if let Err(err) = load_named_preset(codex_home, &profile.effect) {
                        tracing::warn!(
                            profile = name,
                            effect = profile.effect,
                            error = %err,
                            "ignoring voice profile with an invalid effect in realtime voice rotation"
                        );
                        return None;
                    }
                    Some(RotationEntry::Profile(name.to_string()))
                }),
        )
        .collect::<Vec<_>>();
    if voices.is_empty() {
        return None;
    }

    let state_path = codex_home.join(ROTATION_STATE_FILE);
    let lock_path = codex_home.join(ROTATION_LOCK_FILE);
    let Some(lock) = acquire_lock(&lock_path) else {
        tracing::debug!(path = %lock_path.display(), "realtime voice rotation lock is busy");
        return None;
    };

    let state = read_state(&state_path).unwrap_or_default();
    let next_index = if state.entries == voices {
        state.next_index % voices.len()
    } else {
        0
    };
    let selected = voices[next_index].clone();
    let next_state = RotationState {
        entries: voices.clone(),
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
    match selected {
        RotationEntry::Voice(voice) => Some(SelectedVoice {
            voice,
            profile: None,
        }),
        RotationEntry::Profile(name) => {
            let profile = load_named_profile(codex_home, &name).ok()?;
            Some(SelectedVoice {
                voice: profile.voice,
                profile: Some(name),
            })
        }
    }
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
    if let Ok(state) = serde_json::from_slice(&bytes) {
        return Some(state);
    }
    let legacy: LegacyRotationState = serde_json::from_slice(&bytes).ok()?;
    Some(RotationState {
        entries: legacy
            .voices
            .into_iter()
            .map(RotationEntry::Voice)
            .collect(),
        next_index: legacy.next_index,
    })
}

fn write_state(path: &Path, state: &RotationState) -> std::io::Result<()> {
    let temporary_path = temporary_path(path);
    let bytes = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    fs::write(&temporary_path, bytes)?;
    match fs::rename(&temporary_path, path) {
        Ok(()) => Ok(()),
        Err(initial_error) => {
            #[cfg(windows)]
            {
                if path.exists() {
                    let mut backup_path = path.to_path_buf();
                    backup_path.set_extension(format!("bak-{}", std::process::id()));
                    fs::rename(path, &backup_path)?;
                    match fs::rename(&temporary_path, path) {
                        Ok(()) => {
                            let _ = fs::remove_file(&backup_path);
                            return Ok(());
                        }
                        Err(replacement_error) => {
                            let restore_error = fs::rename(&backup_path, path).err();
                            let _ = fs::remove_file(&temporary_path);
                            return match restore_error {
                                None => Err(replacement_error),
                                Some(restore_error) => Err(std::io::Error::other(format!(
                                    "replacing {} failed: {replacement_error}; restoring the previous file failed: {restore_error}",
                                    path.display()
                                ))),
                            };
                        }
                    }
                }
            }
            let _ = fs::remove_file(&temporary_path);
            Err(initial_error)
        }
    }
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
