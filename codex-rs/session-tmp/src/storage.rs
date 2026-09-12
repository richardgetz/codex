use super::AGENTS_DIR;
use super::CleanupReport;
use super::EntryMetadata;
use super::ReapMode;
use super::SESSION_METADATA_FILE;
use super::SESSIONS_DIR;
use super::SessionTmpError;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use uuid::Uuid;

pub(super) const LEASES_DIR: &str = "leases";
const SESSION_LOCKS_DIR: &str = ".locks";
pub(super) const LEASE_STALE_AFTER: Duration = Duration::from_secs(90);
/// Persistent coordination files are retained after a migration completes so
/// a later open cannot race a fresh migration by recreating the same pathname.
pub(super) const MIGRATION_LOCK_CONTENT: &[u8] =
    b"codex session temporary migration lock\nschema_version=1\n";

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct SessionRecord {
    pub(super) schema_version: u8,
    pub(super) session_id: String,
    pub(super) created_at: u64,
    pub(super) updated_at: u64,
    pub(super) status: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct LeaseRecord {
    pub(super) schema_version: u8,
    pub(super) session_id: String,
    pub(super) thread_id: String,
    pub(super) process_id: u32,
    /// Unique ownership token for one lease acquisition. Older records omit
    /// this field; those tokenless records are never overwritten by a newer
    /// heartbeat and are only reclaimed by the normal stale-record path.
    #[serde(default)]
    pub(super) owner_token: Option<String>,
    pub(super) updated_at: u64,
}

pub(super) fn read_lease_record(path: &Path) -> Result<LeaseRecord, SessionTmpError> {
    if fs::symlink_metadata(path)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

pub(super) fn reap_sessions(
    state: &super::state::ControlState,
    mode: ReapMode,
    excluded_session_id: Option<&str>,
) -> Result<CleanupReport, SessionTmpError> {
    let sessions_dir = state.state_sessions_dir();
    state.ensure_state_layout()?;
    ensure_directory_not_symlink(&sessions_dir)?;
    let now = now_seconds();
    let mut report = CleanupReport::default();
    if matches!(mode, ReapMode::OlderThan(age) if age.is_zero()) || !sessions_dir.is_dir() {
        return Ok(report);
    }
    let legacy_transition_active = state.legacy_transition_active()?;
    for item in fs::read_dir(&sessions_dir)? {
        let session_dir = item?.path();
        let is_real_directory = fs::symlink_metadata(&session_dir)
            .map(|metadata| {
                metadata.file_type().is_dir() && !file_type_is_link(metadata.file_type())
            })
            .unwrap_or(false);
        if !is_real_directory
            || excluded_session_id.is_some_and(|session_id| {
                session_dir.file_name().and_then(|name| name.to_str()) == Some(session_id)
            })
        {
            continue;
        }
        let Some(directory_session_id) = session_dir.file_name().and_then(|name| name.to_str())
        else {
            continue;
        };
        let record_path = session_dir.join(super::SESSION_METADATA_FILE);
        let Ok(record) = read_session_record(&record_path) else {
            continue;
        };
        let heartbeat_is_old_enough = match mode {
            ReapMode::OlderThan(max_age) => {
                now.saturating_sub(record.updated_at) >= max_age.as_secs()
            }
            ReapMode::Force => true,
        };
        if record.schema_version != 1
            || record.session_id != directory_session_id
            || !heartbeat_is_old_enough
        {
            continue;
        }
        let Some(_session_lock) = (match try_lock_session(&session_dir) {
            Ok(lock) => lock,
            Err(error) if is_skippable_reap_error(mode, &error) => {
                tracing::debug!(
                    error = %error,
                    session_dir = %session_dir.display(),
                    "skipping stale session with inaccessible cleanup state"
                );
                continue;
            }
            Err(error) => return Err(error),
        }) else {
            continue;
        };
        let _legacy_lock = if legacy_transition_active {
            match try_lock_legacy_session(state.payload_root(), directory_session_id)? {
                LegacyLock::Held(lock) => Some(lock),
                LegacyLock::Absent => None,
                LegacyLock::Unavailable => continue,
            }
        } else {
            None
        };
        let Ok(record) = read_session_record(&record_path) else {
            continue;
        };
        let fresh_lease = match has_fresh_lease(&session_dir.join(LEASES_DIR), LEASE_STALE_AFTER)
            .and_then(|state_lease| {
                if legacy_transition_active {
                    has_fresh_lease(
                        &state.legacy_session_dir(directory_session_id).join(LEASES_DIR),
                        LEASE_STALE_AFTER,
                    )
                    .map(|legacy_lease| state_lease || legacy_lease)
                } else {
                    Ok(state_lease)
                }
            })
        {
            Ok(fresh_lease) => fresh_lease,
            Err(error) if is_skippable_reap_error(mode, &error) => {
                tracing::debug!(
                    error = %error,
                    session_dir = %session_dir.display(),
                    "skipping stale session with inaccessible lease state"
                );
                continue;
            }
            Err(error) => return Err(error),
        };
        let heartbeat_is_old_enough = match mode {
            ReapMode::OlderThan(max_age) => {
                now.saturating_sub(record.updated_at) >= max_age.as_secs()
            }
            ReapMode::Force => true,
        };
        if record.schema_version != 1
            || record.session_id != directory_session_id
            || !heartbeat_is_old_enough
            || fresh_lease
        {
            continue;
        }
        let payload_session = state.payload_session_dir(directory_session_id);
        if let Ok(metadata) = fs::symlink_metadata(&payload_session)
            && (file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir())
        {
            let error = SessionTmpError::UnsafeManagedPath(payload_session);
            if is_skippable_reap_error(mode, &error) {
                continue;
            }
            return Err(error);
        }
        match remove_path(&payload_session) {
            Ok(true) => report.removed_paths += 1,
            Ok(false) => {}
            Err(error) if is_skippable_reap_error(mode, &error) => {
                tracing::debug!(
                    error = %error,
                    session_dir = %payload_session.display(),
                    "skipping stale session that could not be removed"
                );
            }
            Err(error) => return Err(error),
        }
        if let Err(error) = remove_state_session(&session_dir) {
            if is_skippable_reap_error(mode, &error) {
                tracing::debug!(error = %error, session_dir = %session_dir.display(), "skipping stale session control cleanup");
                continue;
            }
            return Err(error);
        }
        if !session_dir.exists() {
            drop(_legacy_lock);
            drop(_session_lock);
            if let Err(error) = remove_session_lock(&sessions_dir, directory_session_id) {
                if is_skippable_reap_error(mode, &error) {
                    tracing::debug!(
                        error = %error,
                        session_id = %directory_session_id,
                        "stale session lock cleanup deferred"
                    );
                } else {
                    return Err(error);
                }
            }
            report.removed_sessions += 1;
        }
    }
    Ok(report)
}

pub(super) fn remove_session_lock(
    sessions_dir: &Path,
    session_id: &str,
) -> Result<(), SessionTmpError> {
    let locks_dir = sessions_dir.join(SESSION_LOCKS_DIR);
    ensure_directory_not_symlink(&locks_dir)?;
    if !locks_dir.is_dir() {
        return Ok(());
    }
    let lock_path = locks_dir.join(format!("{session_id}.lock"));
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) if file_type_is_link(metadata.file_type()) => {
            return Err(SessionTmpError::UnsafeManagedPath(lock_path));
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(SessionTmpError::UnsafeManagedPath(lock_path));
        }
        Ok(_) => match fs::remove_file(&lock_path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    if fs::read_dir(&locks_dir)?.next().transpose()?.is_none() {
        fs::remove_dir(locks_dir)?;
    }
    Ok(())
}

fn remove_state_session(session_dir: &Path) -> Result<(), SessionTmpError> {
    for name in [SESSION_METADATA_FILE, super::ENTRY_METADATA_DIR, LEASES_DIR] {
        let path = session_dir.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if file_type_is_link(metadata.file_type()) {
            return Err(SessionTmpError::UnsafeManagedPath(path));
        }
        if metadata.is_dir() {
            let entries = fs::read_dir(&path)?;
            for item in entries {
                let child = item?.path();
                let child_name = child.file_stem().and_then(|name| name.to_str());
                let child_metadata = fs::symlink_metadata(&child)?;
                if file_type_is_link(child_metadata.file_type()) {
                    return Err(SessionTmpError::UnsafeManagedPath(child));
                }
                if name == super::ENTRY_METADATA_DIR {
                    let Ok(record) = read_metadata(&child) else {
                        continue;
                    };
                    if record.session_id
                        != session_dir
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                        || child_name != Some(record.id.as_str())
                    {
                        continue;
                    }
                } else if name == LEASES_DIR {
                    let Ok(record) = read_lease_record(&child) else {
                        continue;
                    };
                    if record.session_id
                        != session_dir
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                        || record.schema_version != 1
                        || child_name != Some(record.thread_id.as_str())
                        || validate_component(&record.thread_id).is_err()
                    {
                        continue;
                    }
                }
                remove_path(&child)?;
            }
            if fs::read_dir(&path)?.next().transpose()?.is_none() {
                fs::remove_dir(&path)?;
            }
        } else if name == SESSION_METADATA_FILE {
            let Ok(record) = read_session_record(&path) else {
                continue;
            };
            if record.schema_version != 1
                || record.session_id
                    != session_dir
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default()
            {
                continue;
            }
            remove_path(&path)?;
        } else {
            return Err(SessionTmpError::UnsafeManagedPath(path));
        }
    }
    if fs::read_dir(session_dir)?.next().transpose()?.is_none() {
        fs::remove_dir(session_dir)?;
    }
    Ok(())
}

fn is_skippable_reap_error(mode: ReapMode, error: &SessionTmpError) -> bool {
    match mode {
        ReapMode::OlderThan(_) => matches!(error, SessionTmpError::Io(_)),
        ReapMode::Force => matches!(
            error,
            SessionTmpError::Io(_) | SessionTmpError::UnsafeManagedPath(_)
        ),
    }
}

pub(super) fn resolve_user_session_id(
    state: &super::state::ControlState,
    candidate_session_id: &str,
    thread_id: &str,
) -> Result<String, SessionTmpError> {
    validate_component(candidate_session_id)?;
    validate_component(thread_id)?;
    state.ensure_state_layout()?;
    let sessions_dir = state.state_sessions_dir();
    ensure_directory_not_symlink(&sessions_dir)?;
    let candidate_dir = state.state_session_dir(candidate_session_id);
    if candidate_dir.is_dir()
        && read_session_record(&candidate_dir.join(SESSION_METADATA_FILE))
            .is_ok_and(|record| record.session_id == candidate_session_id)
    {
        return Ok(candidate_session_id.to_string());
    }

    for item in fs::read_dir(&sessions_dir)? {
        let session_dir = item?.path();
        if !fs::symlink_metadata(&session_dir)
            .map(|metadata| {
                metadata.file_type().is_dir() && !file_type_is_link(metadata.file_type())
            })
            .unwrap_or(false)
        {
            continue;
        }
        let Some(session_id) = session_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if validate_component(session_id).is_err() {
            continue;
        }
        let Ok(record) = read_session_record(&session_dir.join(SESSION_METADATA_FILE)) else {
            continue;
        };
        if record.session_id != session_id {
            continue;
        }
        let agent_dir = state.payload_session_dir(session_id).join(AGENTS_DIR).join(thread_id);
        let payload_agent_exists = fs::symlink_metadata(&agent_dir)
            .map(|metadata| {
                metadata.file_type().is_dir() && !file_type_is_link(metadata.file_type())
            })
            .unwrap_or(false);
        let metadata_dir = session_dir.join(super::ENTRY_METADATA_DIR);
        let metadata_mentions_thread = metadata_dir.is_dir()
            && fs::read_dir(&metadata_dir)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .any(|item| {
                    read_metadata(&item.path())
                        .is_ok_and(|metadata| metadata.session_id == session_id && metadata.thread_id == thread_id)
                });
        let lease_path = session_dir
            .join(LEASES_DIR)
            .join(format!("{thread_id}.json"));
        if payload_agent_exists || metadata_mentions_thread || lease_path.exists() {
            return Ok(session_id.to_string());
        }
    }

    Ok(candidate_session_id.to_string())
}

pub(super) fn validate_component(value: &str) -> Result<(), SessionTmpError> {
    let mut components = Path::new(value).components();
    let is_single_normal_component = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(component)), None) if component == OsStr::new(value)
    );
    if !is_single_normal_component {
        return Err(SessionTmpError::InvalidComponent(value.to_string()));
    }
    Ok(())
}

pub(super) fn validate_name(value: &str) -> Result<String, SessionTmpError> {
    validate_component(value)?;
    Ok(value.to_string())
}

pub(super) fn read_metadata(path: &Path) -> Result<EntryMetadata, SessionTmpError> {
    if fs::symlink_metadata(path)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|source| SessionTmpError::InvalidMetadata {
        path: path.to_path_buf(),
        source,
    })
}

pub(super) fn read_session_record(path: &Path) -> Result<SessionRecord, SessionTmpError> {
    if fs::symlink_metadata(path)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) fn lease_is_fresh(path: &Path, max_age: Duration) -> bool {
    let Some(updated_at) = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<LeaseRecord>(&bytes).ok())
        .map(|record| record.updated_at)
        .or_else(|| {
            fs::metadata(path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
        })
    else {
        return false;
    };
    now_seconds().saturating_sub(updated_at) < max_age.as_secs()
}

pub(super) fn has_fresh_lease(
    leases_dir: &Path,
    max_age: Duration,
) -> Result<bool, SessionTmpError> {
    match fs::symlink_metadata(leases_dir) {
        Ok(metadata)
            if file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir() =>
        {
            return Err(SessionTmpError::UnsafeManagedPath(leases_dir.to_path_buf()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    for item in fs::read_dir(leases_dir)? {
        let path = item?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if file_type_is_link(metadata.file_type()) || !metadata.file_type().is_file() {
            return Err(SessionTmpError::UnsafeManagedPath(path));
        }
        if lease_is_fresh(&path, max_age) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn lease_is_fresh_for_thread(session_dir: &Path, thread_id: &str) -> bool {
    let path = session_dir
        .join(LEASES_DIR)
        .join(format!("{thread_id}.json"));
    if fs::symlink_metadata(&path)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        // A tampered lease must not make the root session delete a live agent
        // directory. The stricter stale-session reaper rejects this case.
        return true;
    }
    lease_is_fresh(&path, LEASE_STALE_AFTER)
}

fn open_session_lock(session_dir: &Path) -> Result<File, SessionTmpError> {
    ensure_directory_not_symlink(session_dir)?;
    let sessions_dir = session_dir
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "session path has no parent"))?;
    let locks_dir = sessions_dir.join(SESSION_LOCKS_DIR);
    ensure_directory_not_symlink(&locks_dir)?;
    fs::create_dir_all(&locks_dir)?;
    set_private_directory(&locks_dir)?;
    let session_id = session_dir
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "session path has no name"))?;
    let lock_path = locks_dir.join(format!("{}.lock", session_id.to_string_lossy()));
    if fs::symlink_metadata(&lock_path)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(lock_path));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)?;
    set_private_file(&lock_path)?;
    Ok(file)
}

/// Try to lock a legacy payload lock without creating a new control file.
/// New code keeps locks in external state; this read-only compatibility check
/// prevents migration from racing an older binary that still owns the old
/// lock domain.
pub(super) fn try_lock_legacy_session(
    payload_root: &Path,
    session_id: &str,
) -> Result<LegacyLock, SessionTmpError> {
    let locks_dir = payload_root.join(SESSIONS_DIR).join(SESSION_LOCKS_DIR);
    ensure_directory_not_symlink(&locks_dir)?;
    let lock_path = locks_dir.join(format!("{session_id}.lock"));
    let metadata = match fs::symlink_metadata(&lock_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(LegacyLock::Absent),
        Err(error) => return Err(error.into()),
    };
    if file_type_is_link(metadata.file_type()) {
        return Err(SessionTmpError::UnsafeManagedPath(lock_path));
    }
    if !metadata.file_type().is_file() {
        return Err(SessionTmpError::UnsafeManagedPath(lock_path));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)?;
    match file.try_lock() {
        Ok(()) => Ok(LegacyLock::Held(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(LegacyLock::Unavailable),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

pub(super) enum LegacyLock {
    Held(File),
    Unavailable,
    Absent,
}

pub(super) fn lock_session(session_dir: &Path) -> Result<File, SessionTmpError> {
    let file = open_session_lock(session_dir)?;
    file.lock()?;
    Ok(file)
}

pub(super) fn try_lock_session(session_dir: &Path) -> Result<Option<File>, SessionTmpError> {
    let file = open_session_lock(session_dir)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Try the external lock for an already enrolled session without creating a
/// lock file or parent directory. Heartbeats and migration probes use this
/// form so a reaper can remove a session while they are asleep without a
/// racing probe recreating partial control state.
pub(super) fn try_lock_existing_session(
    session_dir: &Path,
) -> Result<ExistingSessionLock, SessionTmpError> {
    let metadata = match fs::symlink_metadata(session_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(ExistingSessionLock::Absent);
        }
        Err(error) => return Err(error.into()),
    };
    if file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir() {
        return Err(SessionTmpError::UnsafeManagedPath(session_dir.to_path_buf()));
    }
    let sessions_dir = session_dir
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "session path has no parent"))?;
    let locks_dir = sessions_dir.join(SESSION_LOCKS_DIR);
    let lock_metadata = match fs::symlink_metadata(&locks_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(ExistingSessionLock::Absent);
        }
        Err(error) => return Err(error.into()),
    };
    if file_type_is_link(lock_metadata.file_type()) || !lock_metadata.file_type().is_dir() {
        return Err(SessionTmpError::UnsafeManagedPath(locks_dir));
    }
    let session_id = session_dir
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "session path has no name"))?;
    let lock_path = locks_dir.join(format!("{}.lock", session_id.to_string_lossy()));
    let metadata = match fs::symlink_metadata(&lock_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(ExistingSessionLock::Absent);
        }
        Err(error) => return Err(error.into()),
    };
    if file_type_is_link(metadata.file_type()) || !metadata.file_type().is_file() {
        return Err(SessionTmpError::UnsafeManagedPath(lock_path));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)?;
    match file.try_lock() {
        Ok(()) => Ok(ExistingSessionLock::Held(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(ExistingSessionLock::Unavailable),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Wait for a migration barrier that already exists. Normal opens never
/// create this file; they only honor a migration lock created by the
/// consolidation pass so source-state retirement cannot race a new manager.
pub(super) fn wait_for_migration_lock(path: &Path) -> Result<Option<File>, SessionTmpError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if file_type_is_link(metadata.file_type()) || !metadata.file_type().is_file() {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.lock()?;
    // Legacy inline barriers were created without a marker. Keep accepting
    // those empty files during the upgrade, while external migration barriers
    // write their durable ownership marker before release. A nonempty file
    // with different contents is untrusted and must never be retired by name.
    let file_len = file.metadata()?.len();
    if file_len > 0 && fs::read(path)?.as_slice() != MIGRATION_LOCK_CONTENT {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    // The caller retains this guard for the entire initialization phase.
    if file_len == 0 {
        file.write_all(MIGRATION_LOCK_CONTENT)?;
        file.sync_data()?;
    }
    Ok(Some(file))
}

pub(super) enum ExistingSessionLock {
    Held(File),
    Unavailable,
    Absent,
}

pub(super) fn write_json_atomically<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), SessionTmpError> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "metadata path has no parent directory",
        )
    })?;
    ensure_directory_not_symlink(parent)?;
    fs::create_dir_all(parent)?;
    ensure_directory_not_symlink(parent)?;
    write_json_atomically_in_existing_parent(path, value)
}

/// Atomically updates a control file only when its parent already exists.
/// Heartbeats use this variant for legacy compatibility files so deleting a
/// disposable payload cannot cause the heartbeat to recreate its old control
/// hierarchy.
pub(super) fn write_json_atomically_existing<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), SessionTmpError> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "metadata path has no parent directory",
        )
    })?;
    ensure_directory_not_symlink(parent)?;
    if !parent.is_dir() {
        return Err(io::Error::new(ErrorKind::NotFound, "metadata parent directory is missing").into());
    }
    write_json_atomically_in_existing_parent(path, value)
}

fn write_json_atomically_in_existing_parent<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), SessionTmpError> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "metadata path has no parent directory",
        )
    })?;
    ensure_directory_not_symlink(parent)?;
    if fs::symlink_metadata(path)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "metadata path has no file name"))?;
    let file_name = file_name.to_string_lossy();
    let suffix = Uuid::new_v4().simple();
    let temporary = parent.join(format!(".{file_name}.{suffix}.tmp"));
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(&temporary, bytes)?;
    set_private_file(&temporary)?;
    match fs::rename(&temporary, path) {
        Ok(()) => {
            set_private_file(path)?;
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            fs::remove_file(path)?;
            fs::rename(temporary, path)?;
            set_private_file(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn ensure_directory_not_symlink(path: &Path) -> Result<(), SessionTmpError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if file_type_is_link(metadata.file_type()) => {
            Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()))
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn collect_paths(
    directory: &Path,
    tracked_paths: &[PathBuf],
    output: &mut Vec<PathBuf>,
    limit: usize,
) -> Result<(), SessionTmpError> {
    ensure_directory_not_symlink(directory)?;
    if output.len() >= limit || !directory.is_dir() {
        return Ok(());
    }
    for item in fs::read_dir(directory)? {
        let path = item?.path();
        let file_type = fs::symlink_metadata(&path)?.file_type();
        if tracked_paths
            .iter()
            .any(|tracked| path == *tracked || path.starts_with(tracked))
        {
            continue;
        }
        if file_type.is_dir() && !file_type_is_link(file_type) {
            collect_paths(&path, tracked_paths, output, limit)?;
        } else {
            output.push(path);
        }
        if output.len() >= limit {
            break;
        }
    }
    Ok(())
}

pub(super) fn remove_untracked_paths(
    directory: &Path,
    preserved_paths: &HashSet<PathBuf>,
    preserved_directories: &HashSet<PathBuf>,
    protected_directories: &HashSet<PathBuf>,
    report: &mut CleanupReport,
) -> Result<(), SessionTmpError> {
    ensure_directory_not_symlink(directory)?;
    if !directory.is_dir() {
        return Ok(());
    }
    for item in fs::read_dir(directory)? {
        let path = item?.path();
        let file_type = fs::symlink_metadata(&path)?.file_type();
        if preserved_directories.contains(&path) {
            continue;
        }
        let is_preserved = preserved_paths
            .iter()
            .any(|preserved| *preserved == path || preserved.starts_with(&path));
        let is_preserved_root = preserved_paths.iter().any(|preserved| *preserved == path);
        let is_protected_directory = protected_directories.contains(&path)
            && file_type.is_dir()
            && !file_type_is_link(file_type);
        let should_recurse = (is_preserved
            && !is_preserved_root
            && file_type.is_dir()
            && !file_type_is_link(file_type))
            || is_protected_directory;
        if should_recurse {
            remove_untracked_paths(
                &path,
                preserved_paths,
                preserved_directories,
                protected_directories,
                report,
            )?;
        } else if !is_preserved && remove_path(&path)? {
            report.removed_paths += 1;
        }
    }
    Ok(())
}

pub(super) fn remove_path(path: &Path) -> Result<bool, SessionTmpError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if file_type_is_link(metadata.file_type()) {
        fs::remove_file(path)?;
        Ok(true)
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)?;
        Ok(true)
    } else {
        fs::remove_file(path)?;
        Ok(true)
    }
}

pub(super) fn file_type_is_link(file_type: fs::FileType) -> bool {
    if file_type.is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        return file_type.is_symlink_dir() || file_type.is_symlink_file();
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub(super) fn set_private_directory(path: &Path) -> Result<(), SessionTmpError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path)?;
        if file_type_is_link(metadata.file_type()) {
            return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

pub(super) fn set_private_file(path: &Path) -> Result<(), SessionTmpError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path)?;
        if file_type_is_link(metadata.file_type()) {
            return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

pub(super) fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
