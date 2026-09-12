//! Exact-owned source cleanup for recovery migration.

use super::liveness::{collect_session_ids, session_is_live};
use super::state;
use super::storage;
use super::ControlState;
use super::AGENTS_DIR;
use super::ENTRY_METADATA_DIR;
use super::LEASES_DIR;
use super::MIGRATION_LOCK_FILE;
use super::MIGRATION_MANIFEST_PREFIX;
use super::MIGRATION_MANIFEST_SUFFIX;
use super::SESSIONS_DIR;
use super::SESSION_METADATA_FILE;
use super::SessionTmpError;
use super::read_manifest;
use super::Path;
use std::fs;
use std::io::ErrorKind;

pub(super) fn retire_source_session(
    source: &ControlState,
    session_id: &str,
) -> Result<(), SessionTmpError> {
    let payload_session = source.payload_session_dir(session_id);
    remove_known_control_files(&payload_session, session_id)?;
    // `merge_session` holds the legacy session lock while retiring the
    // source. Remove only this validated session's lock file before dropping
    // that lock so an otherwise empty `.locks` directory cannot keep a
    // successfully migrated recovery root alive forever.
    remove_legacy_lock_file(source, session_id)?;
    let state_session = source.state_session_dir(session_id);
    remove_known_control_files(&state_session, session_id)?;
    Ok(())
}

fn remove_legacy_lock_file(source: &ControlState, session_id: &str) -> Result<(), SessionTmpError> {
    if !source.legacy_transition_active()? {
        return Ok(());
    }
    let locks_dir = source.payload_sessions_dir().join(".locks");
    let path = locks_dir.join(format!("{session_id}.lock"));
    match fs::symlink_metadata(&path) {
        Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
            Err(SessionTmpError::UnsafeManagedPath(path))
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            Err(SessionTmpError::UnsafeManagedPath(path))
        }
        Ok(_) => match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_known_control_files(
    session_dir: &Path,
    session_id: &str,
) -> Result<(), SessionTmpError> {
    if !state::is_real_directory(session_dir) {
        return Ok(());
    }
    let record_path = session_dir.join(SESSION_METADATA_FILE);
    if let Ok(record) = storage::read_session_record(&record_path)
        && record.schema_version == 1
        && record.session_id == session_id
    {
        storage::remove_path(&record_path)?;
    }
    for name in [ENTRY_METADATA_DIR, LEASES_DIR] {
        let path = session_dir.join(name);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if storage::file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir() {
            continue;
        }
        for item in fs::read_dir(&path)? {
            let child = item?.path();
            let child_name = child.file_stem().and_then(|name| name.to_str());
            let is_valid = if name == ENTRY_METADATA_DIR {
                storage::read_metadata(&child).is_ok_and(|metadata| {
                    metadata.session_id == session_id
                        && child_name == Some(metadata.id.as_str())
                        && super::records::valid_metadata_path(&metadata)
                })
            } else {
                storage::read_lease_record(&child).is_ok_and(|record| {
                    record.schema_version == 1
                        && record.session_id == session_id
                        && child_name == Some(record.thread_id.as_str())
                })
            };
            if is_valid {
                storage::remove_path(&child)?;
            }
        }
        if fs::read_dir(&path)?.next().transpose()?.is_none() {
            fs::remove_dir(&path)?;
        }
    }
    let agents = session_dir.join(AGENTS_DIR);
    if state::is_real_directory(&agents) && fs::read_dir(&agents)?.next().transpose()?.is_none() {
        fs::remove_dir(&agents)?;
    }
    if fs::read_dir(session_dir)?.next().transpose()?.is_none() {
        fs::remove_dir(session_dir)?;
    }
    Ok(())
}

pub(super) fn source_root_is_retirable(source: &ControlState) -> Result<bool, SessionTmpError> {
    for session_id in collect_session_ids(source)? {
        if session_is_live(source, &session_id)? {
            return Ok(false);
        }
    }
    let payload_retirable = match fs::symlink_metadata(source.payload_root()) {
        Ok(metadata)
            if storage::file_type_is_link(metadata.file_type())
                || !metadata.file_type().is_dir() =>
        {
            return Err(SessionTmpError::UnsafeManagedPath(source.payload_root().to_path_buf()));
        }
        Ok(_) => root_has_only_managed_entries(source.payload_root(), true),
        Err(error) if error.kind() == ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    Ok(payload_retirable
        && root_has_only_managed_entries(source.state_root(), false))
}

/// Retires a recovery payload root without recursively deleting anything. The
/// caller holds both migration locks while this final emptiness check and
/// removal run; a concurrent old writer can therefore only make `remove_dir`
/// fail harmlessly, leaving its new data in place.
pub(super) fn retire_source_payload(source: &ControlState) -> Result<bool, SessionTmpError> {
    let root = source.payload_root();
    match fs::symlink_metadata(root) {
        Ok(metadata)
            if storage::file_type_is_link(metadata.file_type())
                || !metadata.file_type().is_dir() =>
        {
            return Err(SessionTmpError::UnsafeManagedPath(root.to_path_buf()));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    if !root_has_only_managed_entries(root, true) {
        return Ok(false);
    }
    let marker = root.join(state::LEGACY_MARKER);
    if !legacy_marker_is_valid(&marker)? {
        return Ok(false);
    }
    let sessions = root.join(SESSIONS_DIR);
    if let Ok(metadata) = fs::symlink_metadata(&sessions) {
        if storage::file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir() {
            return Err(SessionTmpError::UnsafeManagedPath(sessions));
        }
        let legacy_locks = sessions.join(".locks");
        if let Ok(metadata) = fs::symlink_metadata(&legacy_locks) {
            if storage::file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir() {
                return Err(SessionTmpError::UnsafeManagedPath(legacy_locks));
            }
            match fs::remove_dir(&legacy_locks) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotEmpty => return Ok(false),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        match fs::remove_dir(&sessions) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotEmpty => return Ok(false),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if !legacy_marker_is_valid(&marker)? {
        return Ok(false);
    }
    match fs::remove_file(&marker) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match fs::remove_dir(root) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotEmpty => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Removes only recognized external source controls once the payload root has
/// retired. Unknown files or a concurrent writer keep the state root intact.
pub(super) fn retire_source_state(source: &ControlState) -> Result<bool, SessionTmpError> {
    let root = source.state_root();
    if !root_has_only_managed_entries(root, false) {
        return Ok(false);
    }
    match source.ensure_identity() {
        Ok(()) => {}
        Err(SessionTmpError::UnsafeManagedPath(path)) => {
            return Err(SessionTmpError::UnsafeManagedPath(path));
        }
        Err(_) => return Ok(false),
    }
    for item in fs::read_dir(root)? {
        let path = item?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if storage::file_type_is_link(metadata.file_type()) {
            return Err(SessionTmpError::UnsafeManagedPath(path));
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Ok(false);
        };
        if name == state::STATE_SESSIONS_DIR || name == state::STATE_LOCKS_DIR {
            if !metadata.file_type().is_dir() {
                return Err(SessionTmpError::UnsafeManagedPath(path));
            }
            match fs::remove_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotEmpty => return Ok(false),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        } else if metadata.file_type().is_file()
            && (name == state::STATE_MARKER
                || name == state::STATE_ROOT_RECORD
                || name == MIGRATION_LOCK_FILE
                || (name.starts_with(MIGRATION_MANIFEST_PREFIX)
                    && name.ends_with(MIGRATION_MANIFEST_SUFFIX)
                    && read_manifest(&path).is_some_and(|manifest| {
                        manifest.schema_version == 1
                            && (manifest.source_root_id == source.root_id
                                || manifest.target_root_id == source.root_id)
                    })))
        {
            if name == state::STATE_MARKER || name == state::STATE_ROOT_RECORD {
                match source.ensure_identity() {
                    Ok(()) => {}
                    Err(SessionTmpError::UnsafeManagedPath(path)) => {
                        return Err(SessionTmpError::UnsafeManagedPath(path));
                    }
                    Err(_) => return Ok(false),
                }
            }
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        } else {
            return Ok(false);
        }
    }
    match fs::remove_dir(root) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotEmpty => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn legacy_marker_is_valid(path: &Path) -> Result<bool, SessionTmpError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if storage::file_type_is_link(metadata.file_type()) || !metadata.file_type().is_file() {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    Ok(fs::read_to_string(path)
        .ok()
        .is_some_and(|content| content == state::LEGACY_MARKER_CONTENT))
}

fn root_has_only_managed_entries(root: &Path, payload: bool) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    for item in entries {
        let Ok(path) = item.map(|entry| entry.path()) else {
            return false;
        };
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return false;
        };
        if storage::file_type_is_link(metadata.file_type()) {
            return false;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let allowed = if payload {
            name == state::LEGACY_MARKER || name == SESSIONS_DIR
        } else {
            name == state::STATE_MARKER
                || name == state::STATE_ROOT_RECORD
                || name == state::STATE_SESSIONS_DIR
                || name == state::STATE_LOCKS_DIR
                || name == MIGRATION_LOCK_FILE
                || (name.starts_with(MIGRATION_MANIFEST_PREFIX)
                    && name.ends_with(MIGRATION_MANIFEST_SUFFIX)
                    && read_manifest(&path).is_some_and(|manifest| manifest.schema_version == 1))
        };
        if !allowed {
            return false;
        }
        if payload && name == SESSIONS_DIR && !payload_sessions_empty_or_locks_only(&path) {
            return false;
        }
        if !payload
            && (name == state::STATE_SESSIONS_DIR || name == state::STATE_LOCKS_DIR)
            && !directory_empty(&path)
        {
            return false;
        }
    }
    true
}

fn directory_empty(path: &Path) -> bool {
    fs::read_dir(path)
        .ok()
        .and_then(|mut entries| entries.next().transpose().ok())
        .flatten()
        .is_none()
}

fn payload_sessions_empty_or_locks_only(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    for item in entries {
        let Ok(entry) = item else {
            return false;
        };
        let child = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&child) else {
            return false;
        };
        if storage::file_type_is_link(metadata.file_type()) {
            return false;
        }
        let Some(name) = child.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        if name != ".locks" || !metadata.file_type().is_dir() || !directory_empty(&child) {
            return false;
        }
    }
    true
}
