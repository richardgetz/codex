use super::SessionTmpError;
use super::state::ControlState;
use super::storage::LEASE_STALE_AFTER;
use super::storage::LEASES_DIR;
use super::storage::LeaseRecord;
use super::storage::ensure_directory_not_symlink;
use super::storage::file_type_is_link;
use super::storage::lease_is_fresh;
use super::storage::now_seconds;
use super::storage::set_private_file;
use super::storage::write_json_atomically;
use super::storage::write_json_atomically_existing;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;
use uuid::Uuid;

#[cfg(not(test))]
const LEASE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
#[cfg(test)]
const LEASE_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(25);

pub(super) struct SessionLease {
    path: PathBuf,
    legacy_path: Option<PathBuf>,
    owner_token: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SessionLease {
    pub(super) fn acquire(
        state: &ControlState,
        session_dir: &Path,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Self, SessionTmpError> {
        let mut session_lock = None;
        for _ in 0..16 {
            match super::storage::try_lock_session(session_dir)? {
                Some(lock) => {
                    session_lock = Some(lock);
                    break;
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        let Some(_session_lock) = session_lock else {
            return Err(SessionTmpError::SessionAlreadyOwned(thread_id.to_string()));
        };
        let legacy_session_dir = state.legacy_session_dir(session_id);
        let legacy_transition_active = state.legacy_transition_active()?;
        let legacy_lock = if legacy_transition_active {
            match super::storage::try_lock_existing_session(&legacy_session_dir)? {
                super::storage::ExistingSessionLock::Held(lock) => Some(lock),
                super::storage::ExistingSessionLock::Absent => None,
                super::storage::ExistingSessionLock::Unavailable => {
                    return Err(SessionTmpError::SessionAlreadyOwned(thread_id.to_string()));
                }
            }
        } else {
            None
        };
        let legacy_lease_path = legacy_session_dir
            .join(LEASES_DIR)
            .join(format!("{thread_id}.json"));
        if legacy_transition_active
            && super::storage::lease_is_fresh(&legacy_lease_path, LEASE_STALE_AFTER)
        {
            return Err(SessionTmpError::SessionAlreadyOwned(thread_id.to_string()));
        }
        let leases_dir = session_dir.join(LEASES_DIR);
        ensure_directory_not_symlink(session_dir)?;
        ensure_directory_not_symlink(&leases_dir)?;
        fs::create_dir_all(&leases_dir)?;
        super::storage::set_private_directory(&leases_dir)?;
        let path = leases_dir.join(format!("{thread_id}.json"));
        let record = LeaseRecord {
            schema_version: 1,
            session_id: session_id.to_string(),
            thread_id: thread_id.to_string(),
            process_id: std::process::id(),
            owner_token: Some(Uuid::new_v4().to_string()),
            updated_at: now_seconds(),
        };
        let owner_token = record
            .owner_token
            .clone()
            .expect("newly acquired leases always have an owner token");
        let legacy_path = if legacy_transition_active
            && legacy_lock.is_some()
            && legacy_session_dir.is_dir()
        {
            let legacy_leases_dir = legacy_session_dir.join(LEASES_DIR);
            ensure_directory_not_symlink(&legacy_session_dir)?;
            ensure_directory_not_symlink(&legacy_leases_dir)?;
            if legacy_leases_dir.is_dir() {
                super::storage::set_private_directory(&legacy_leases_dir)?;
                Some(legacy_leases_dir.join(format!("{thread_id}.json")))
            } else {
                None
            }
        } else {
            None
        };

        for _ in 0..2 {
            if fs::symlink_metadata(&path)
                .map(|metadata| file_type_is_link(metadata.file_type()))
                .unwrap_or(false)
            {
                return Err(SessionTmpError::UnsafeManagedPath(path));
            }
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    serde_json::to_writer(&mut file, &record)?;
                    file.sync_all()?;
                    set_private_file(&path)?;
                    if let Some(legacy_path) = legacy_path.as_ref()
                        && fs::symlink_metadata(&legacy_path)
                            .map(|metadata| file_type_is_link(metadata.file_type()))
                            .unwrap_or(false)
                    {
                        let _ = fs::remove_file(&path);
                        return Err(SessionTmpError::UnsafeManagedPath(legacy_path.clone()));
                    }
                    if let Some(legacy_path) = legacy_path.as_ref() {
                        match write_json_atomically_existing(legacy_path, &record) {
                            Ok(()) => {}
                            Err(SessionTmpError::Io(error))
                                if error.kind() == ErrorKind::NotFound => {}
                            Err(error) => {
                                let _ = fs::remove_file(&path);
                                return Err(error);
                            }
                        }
                    }
                    drop(legacy_lock);
                    let stop = Arc::new(AtomicBool::new(false));
                    let stop_for_thread = Arc::clone(&stop);
                    let path_for_thread = path.clone();
                    let state_root_for_thread = state.state_root().to_path_buf();
                    let state_session_dir_for_thread = session_dir.to_path_buf();
                    let legacy_path_for_thread = legacy_path.clone();
                    let owner_token_for_thread = owner_token.clone();
                    let legacy_marker_for_thread = state
                        .payload_root()
                        .join(super::state::LEGACY_MARKER);
                    let legacy_session_for_thread = legacy_session_dir.clone();
                    let session_id_for_thread = session_id.to_string();
                    let thread_id_for_thread = thread_id.to_string();
                    let thread = match std::thread::Builder::new()
                        .name("codex-session-tmp-lease".to_string())
                        .spawn(move || {
                            while !stop_for_thread.load(Ordering::Relaxed) {
                                let mut remaining = LEASE_HEARTBEAT_INTERVAL;
                                while !remaining.is_zero()
                                    && !stop_for_thread.load(Ordering::Relaxed)
                                {
                                    let wait = remaining.min(Duration::from_millis(100));
                                    std::thread::sleep(wait);
                                    remaining = remaining.saturating_sub(wait);
                                }
                                if stop_for_thread.load(Ordering::Relaxed) {
                                    break;
                                }
                                // The reaper may have removed the external
                                // session while this thread was asleep. Do
                                // not let the heartbeat recreate a deleted
                                // lease or a partial control tree.
                                if !state_session_is_live(
                                    &state_root_for_thread,
                                    &state_session_dir_for_thread,
                                    &session_id_for_thread,
                                ) {
                                    break;
                                }
                                let _session_lock = match super::storage::try_lock_existing_session(
                                    &state_session_dir_for_thread,
                                ) {
                                    Ok(super::storage::ExistingSessionLock::Held(lock)) => lock,
                                    Ok(super::storage::ExistingSessionLock::Absent) => break,
                                    Ok(super::storage::ExistingSessionLock::Unavailable) => {
                                        continue;
                                    }
                                    Err(error) => {
                                        tracing::debug!(
                                            error = %error,
                                            session_dir = %state_session_dir_for_thread.display(),
                                            "session temporary lease lock probe failed"
                                        );
                                        continue;
                                    }
                                };
                                if !state_session_is_live(
                                    &state_root_for_thread,
                                    &state_session_dir_for_thread,
                                    &session_id_for_thread,
                                ) {
                                    break;
                                }
                                if !lease_is_owned(
                                    &path_for_thread,
                                    &owner_token_for_thread,
                                    &session_id_for_thread,
                                    &thread_id_for_thread,
                                ) {
                                    break;
                                }
                                let record = LeaseRecord {
                                    schema_version: 1,
                                    session_id: session_id_for_thread.clone(),
                                    thread_id: thread_id_for_thread.clone(),
                                    process_id: std::process::id(),
                                    owner_token: Some(owner_token_for_thread.clone()),
                                    updated_at: now_seconds(),
                                };
                                if let Err(error) = write_json_atomically(&path_for_thread, &record)
                                {
                                    tracing::debug!(
                                        error = %error,
                                        path = %path_for_thread.display(),
                                        "session temporary lease heartbeat failed"
                                    );
                                }
                                if let Some(legacy_path) = legacy_path_for_thread.as_ref()
                                    && fs::read_to_string(&legacy_marker_for_thread)
                                        .ok()
                                        .as_deref()
                                        == Some(super::state::LEGACY_MARKER_CONTENT)
                                    && legacy_session_for_thread.is_dir()
                                    && let Ok(super::storage::ExistingSessionLock::Held(
                                        _legacy_lock,
                                    )) = super::storage::try_lock_existing_session(
                                        &legacy_session_for_thread,
                                    )
                                    && lease_is_owned(
                                        legacy_path,
                                        &owner_token_for_thread,
                                        &session_id_for_thread,
                                        &thread_id_for_thread,
                                    )
                                    && let Err(error) = write_json_atomically_existing(legacy_path, &record)
                                {
                                    tracing::debug!(
                                        error = %error,
                                        path = %legacy_path.display(),
                                        "legacy session temporary lease heartbeat failed"
                                    );
                                }
                            }
                        }) {
                        Ok(thread) => thread,
                        Err(error) => {
                            let _ = fs::remove_file(&path);
                            if let Some(legacy_path) = legacy_path.as_ref() {
                                remove_own_legacy_lease(legacy_path, &owner_token);
                            }
                            return Err(error.into());
                        }
                    };
                    return Ok(Self {
                        path,
                        legacy_path,
                        owner_token,
                        stop,
                        thread: Some(thread),
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if lease_is_fresh(&path, LEASE_STALE_AFTER) {
                        return Err(SessionTmpError::SessionAlreadyOwned(thread_id.to_string()));
                    }
                    match fs::remove_file(&path) {
                        Ok(()) => continue,
                        Err(error) if error.kind() == ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }

        Err(SessionTmpError::SessionAlreadyOwned(thread_id.to_string()))
    }
}

fn state_session_is_live(state_root: &Path, state_session_dir: &Path, session_id: &str) -> bool {
    let root_metadata = fs::symlink_metadata(state_root).ok();
    if root_metadata
        .as_ref()
        .is_none_or(|metadata| {
            file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir()
        })
    {
        return false;
    }
    if fs::read_to_string(state_root.join(super::state::STATE_MARKER)).ok().as_deref()
        != Some(super::state::STATE_MARKER_CONTENT)
    {
        return false;
    }
    if !fs::symlink_metadata(state_session_dir)
        .map(|metadata| metadata.file_type().is_dir() && !file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return false;
    }
    super::storage::read_session_record(&state_session_dir.join(super::SESSION_METADATA_FILE))
        .is_ok_and(|record| record.schema_version == 1 && record.session_id == session_id)
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        remove_own_state_lease(&self.path, &self.owner_token);
        if let Some(path) = &self.legacy_path {
            remove_own_legacy_lease(path, &self.owner_token);
        }
    }
}

fn lease_is_owned(path: &Path, owner_token: &str, session_id: &str, thread_id: &str) -> bool {
    super::storage::read_lease_record(path).is_ok_and(|record| {
        record.schema_version == 1
            && record.session_id == session_id
            && record.thread_id == thread_id
            && record.process_id == std::process::id()
            && record.owner_token.as_deref() == Some(owner_token)
    })
}

fn remove_own_state_lease(path: &Path, owner_token: &str) {
    let Some(leases_dir) = path.parent() else {
        return;
    };
    let Some(session_dir) = leases_dir.parent() else {
        return;
    };
    let Ok(super::storage::ExistingSessionLock::Held(lock)) =
        super::storage::try_lock_existing_session(session_dir)
    else {
        return;
    };
    let Some(session_id) = session_dir.file_name().and_then(|name| name.to_str()) else {
        drop(lock);
        return;
    };
    let Some(thread_id) = path.file_stem().and_then(|name| name.to_str()) else {
        drop(lock);
        return;
    };
    if lease_is_owned(path, owner_token, session_id, thread_id) {
        let _ = fs::remove_file(path);
    }
    drop(lock);
}

fn remove_own_legacy_lease(path: &Path, owner_token: &str) {
    let Some(leases_dir) = path.parent() else {
        return;
    };
    let Some(session_dir) = leases_dir.parent() else {
        return;
    };
    let Ok(super::storage::ExistingSessionLock::Held(_session_lock)) =
        super::storage::try_lock_existing_session(session_dir)
    else {
        return;
    };
    let session_id = session_dir.file_name().and_then(|name| name.to_str());
    let thread_id = path.file_stem().and_then(|name| name.to_str());
    if let (Some(session_id), Some(thread_id)) = (session_id, thread_id)
        && lease_is_owned(path, owner_token, session_id, thread_id)
    {
        let _ = fs::remove_file(path);
    }
}
