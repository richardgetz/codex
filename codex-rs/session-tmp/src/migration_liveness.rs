//! Liveness and bounded discovery helpers for legacy migration.

use super::state;
use super::storage;
use super::ControlState;
use super::LEASES_DIR;
use super::MAX_MIGRATION_SESSIONS;
use super::SESSION_METADATA_FILE;
use super::SessionTmpError;
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;

pub(super) fn collect_session_ids(source: &ControlState) -> Result<Vec<String>, SessionTmpError> {
    let mut ids = HashSet::new();
    let state_sessions = source.state_sessions_dir();
    storage::ensure_directory_not_symlink(&state_sessions)?;
    if state_sessions.is_dir() {
        let mut items = fs::read_dir(&state_sessions)?;
        for _ in 0..MAX_MIGRATION_SESSIONS {
            let Some(item) = items.next() else {
                break;
            };
            let path = item?.path();
            let Some(session_id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if storage::validate_component(session_id).is_err()
                || !state::is_real_directory(&path)
            {
                continue;
            }
            let Ok(record) = storage::read_session_record(&path.join(SESSION_METADATA_FILE)) else {
                continue;
            };
            if record.schema_version == 1 && record.session_id == session_id {
                ids.insert(session_id.to_string());
            }
        }
        if items.next().transpose()?.is_some() {
            return Err(migration_limit_error("too many legacy sessions"));
        }
    }

    // A marker protects the root itself, but arbitrary leftovers can still be
    // present below its sessions directory. Only a validated legacy record
    // gives migration ownership of a session subtree.
    let payload_sessions = source.payload_sessions_dir();
    storage::ensure_directory_not_symlink(&payload_sessions)?;
    if payload_sessions.is_dir() {
        let mut items = fs::read_dir(&payload_sessions)?;
        for _ in 0..MAX_MIGRATION_SESSIONS {
            let Some(item) = items.next() else {
                break;
            };
            let path = item?.path();
            let Some(session_id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if storage::validate_component(session_id).is_err()
                || !state::is_real_directory(&path)
            {
                continue;
            }
            let Ok(record) = storage::read_session_record(&path.join(SESSION_METADATA_FILE)) else {
                continue;
            };
            if record.schema_version == 1 && record.session_id == session_id {
                ids.insert(session_id.to_string());
            }
        }
        if items.next().transpose()?.is_some() {
            return Err(migration_limit_error("too many legacy sessions"));
        }
    }
    let mut ids = ids.into_iter().collect::<Vec<_>>();
    ids.sort();
    Ok(ids)
}

pub(super) fn migration_limit_error(message: &str) -> SessionTmpError {
    SessionTmpError::Io(std::io::Error::new(ErrorKind::InvalidData, message))
}

pub(super) fn session_is_live(source: &ControlState, session_id: &str) -> Result<bool, SessionTmpError> {
    let state_session = source.state_session_dir(session_id);
    let _state_lock = match storage::try_lock_existing_session(&state_session)? {
        storage::ExistingSessionLock::Held(lock) => Some(lock),
        storage::ExistingSessionLock::Absent => None,
        storage::ExistingSessionLock::Unavailable => return Ok(true),
    };
    let _legacy_lock = if source.legacy_transition_active()? {
        match storage::try_lock_legacy_session(source.payload_root(), session_id)? {
            storage::LegacyLock::Held(lock) => Some(lock),
            storage::LegacyLock::Absent => None,
            storage::LegacyLock::Unavailable => return Ok(true),
        }
    } else {
        None
    };
    has_live_leases(source, session_id)
}

pub(super) fn has_live_leases(state: &ControlState, session_id: &str) -> Result<bool, SessionTmpError> {
    let external = storage::has_fresh_lease(
        &state.state_session_dir(session_id).join(LEASES_DIR),
        storage::LEASE_STALE_AFTER,
    )?;
    if state.legacy_transition_active()? {
        Ok(external
            || storage::has_fresh_lease(
                &state.legacy_session_dir(session_id).join(LEASES_DIR),
                storage::LEASE_STALE_AFTER,
            )?)
    } else {
        Ok(external)
    }
}
