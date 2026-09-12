//! Exact-owned source cleanup for recovery migration.

use super::liveness::{collect_session_ids, session_is_live};
use super::state;
use super::storage;
use super::ControlState;
use crate::AGENTS_DIR;
use crate::ENTRY_METADATA_DIR;
use crate::LEASES_DIR;
use super::MIGRATION_LOCK_FILE;
use super::MIGRATION_MANIFEST_PREFIX;
use super::MIGRATION_MANIFEST_SUFFIX;
use crate::SESSIONS_DIR;
use crate::SESSION_METADATA_FILE;
use super::SessionTmpError;
use super::read_manifest;
use super::Path;
use super::PathBuf;
use std::fs;
use std::fs::File;
use std::io::ErrorKind;
use std::io::Read;

pub(super) fn retire_source_session(
    source: &ControlState,
    target: &ControlState,
    session_id: &str,
    moved_paths: &[(PathBuf, PathBuf)],
) -> Result<(), SessionTmpError> {
    let payload_session = source.payload_session_dir(session_id);
    remove_moved_payload_paths(source, target, session_id, moved_paths)?;
    remove_known_control_files(&payload_session, session_id)?;
    // Keep the legacy lock pathname even while this validated session is
    // retired. An older process may already hold or await the same inode;
    // unlinking it would split that lock domain when a later manager creates a
    // replacement path. A lock-only recovery root is a bounded compatibility
    // residue and remains outside normal payload cleanup.
    let state_session = source.state_session_dir(session_id);
    remove_known_control_files(&state_session, session_id)?;
    Ok(())
}

/// Remove only payload paths that were copied to a destination recorded in the
/// durable migration manifest. Files are compared before removal so a manual
/// edit made after the copy remains in the source tree for operator recovery.
fn remove_moved_payload_paths(
    source: &ControlState,
    target: &ControlState,
    session_id: &str,
    moved_paths: &[(PathBuf, PathBuf)],
) -> Result<(), SessionTmpError> {
    let mut paths = moved_paths.to_vec();
    paths.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (source_relative, target_relative) in paths {
        if !super::records::valid_manifest_path(&source_relative)
            || !super::records::valid_manifest_path(&target_relative)
        {
            continue;
        }
        let source_path = source.payload_session_dir(session_id).join(&source_relative);
        let target_path = target.payload_session_dir(session_id).join(&target_relative);
        let source_type = match fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata.file_type(),
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let target_type = match fs::symlink_metadata(&target_path) {
            Ok(metadata) => metadata.file_type(),
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let equivalent = if storage::file_type_is_link(source_type) {
            storage::file_type_is_link(target_type)
                && fs::read_link(&source_path)? == fs::read_link(&target_path)?
        } else if source_type.is_file() {
            target_type.is_file() && files_equal(&source_path, &target_path)?
        } else {
            source_type.is_dir() && target_type.is_dir() && !storage::file_type_is_link(target_type)
                && fs::read_dir(&source_path)?.next().transpose()?.is_none()
        };
        if !equivalent {
            continue;
        }
        if source_type.is_dir() {
            match fs::remove_dir(&source_path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(_) => {}
            }
        } else {
            match fs::remove_file(&source_path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    prune_empty_payload_dirs(&source.payload_session_dir(session_id).join(AGENTS_DIR))?;
    Ok(())
}

fn prune_empty_payload_dirs(path: &Path) -> Result<(), SessionTmpError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if storage::file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir() {
        return Ok(());
    }
    for item in fs::read_dir(path)? {
        let child = item?.path();
        let child_metadata = fs::symlink_metadata(&child)?;
        if child_metadata.file_type().is_dir() && !storage::file_type_is_link(child_metadata.file_type()) {
            prune_empty_payload_dirs(&child)?;
        }
    }
    if fs::read_dir(path)?.next().transpose()?.is_none() {
        let _ = fs::remove_dir(path);
    }
    Ok(())
}

fn files_equal(left: &Path, right: &Path) -> Result<bool, SessionTmpError> {
    let left_metadata = fs::metadata(left)?;
    let right_metadata = fs::metadata(right)?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }
    let mut left_file = File::open(left)?;
    let mut right_file = File::open(right)?;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left_file.read(&mut left_buffer)?;
        let right_read = right_file.read(&mut right_buffer)?;
        if left_read != right_read {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
        if left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
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
    Ok(payload_retirable && state_root_is_retirable(source.state_root()))
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
    let marker_present = match fs::symlink_metadata(&marker) {
        Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
            return Err(SessionTmpError::UnsafeManagedPath(marker));
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(SessionTmpError::UnsafeManagedPath(marker));
        }
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if !marker_present {
        // A missing marker is safe only when the retained external identity
        // still validates this exact source path. Unknown or malformed roots
        // remain untouched.
        match source.ensure_identity() {
            Ok(()) => {}
            Err(SessionTmpError::UnsafeManagedPath(path)) => {
                return Err(SessionTmpError::UnsafeManagedPath(path));
            }
            Err(_) => return Ok(false),
        }
    } else if !legacy_marker_is_valid(&marker)? {
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
                Err(error) if error.kind() == ErrorKind::DirectoryNotEmpty => return Ok(false),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        match fs::remove_dir(&sessions) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::DirectoryNotEmpty => return Ok(false),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if marker_present {
        if !legacy_marker_is_valid(&marker)? {
            return Ok(false);
        }
        match fs::remove_file(&marker) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    match fs::remove_dir(root) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) if error.kind() == ErrorKind::DirectoryNotEmpty => {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

/// Removes only recognized external source controls once the payload root has
/// retired. Unknown files or a concurrent writer keep the state root intact.
pub(super) fn retire_source_state(source: &ControlState) -> Result<bool, SessionTmpError> {
    let root = source.state_root();
    if !state_root_is_retirable(root) {
        return Ok(false);
    }
    match source.ensure_identity() {
        Ok(()) => {}
        Err(SessionTmpError::UnsafeManagedPath(path)) => {
            return Err(SessionTmpError::UnsafeManagedPath(path));
        }
        Err(_) => {
            return Ok(false);
        }
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
            if name == state::STATE_SESSIONS_DIR && state_sessions_only_lock_files(&path) {
                // Per-session external lock names remain as stable
                // coordination files. They are no longer tied to a live
                // source record, and removing them could split a waiter from
                // a new manager that opens the same state root.
                continue;
            }
            match fs::remove_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::DirectoryNotEmpty => {
                    return Ok(false)
                }
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
            // Keep identity and coordination files as durable bookkeeping.
            // They form a source tombstone so an interrupted payload
            // retirement remains discoverable on the next open.
            if name == state::STATE_MARKER
                || name == state::STATE_ROOT_RECORD
                || name == MIGRATION_LOCK_FILE
            {
                continue;
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
    // The identity marker, root record, and legacy migration lock intentionally
    // remain in place. They are a tiny durable source tombstone rather than
    // disposable payload control, and allow a later open to finish a payload
    // retirement after a process crash.
    Ok(true)
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

fn state_root_is_retirable(root: &Path) -> bool {
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
        if name == state::STATE_SESSIONS_DIR {
            if !metadata.file_type().is_dir() || !state_sessions_only_lock_files(&path) {
                return false;
            }
        } else if name == state::STATE_LOCKS_DIR {
            if !metadata.file_type().is_dir() || !directory_empty(&path) {
                return false;
            }
        } else if name != state::STATE_MARKER
            && name != state::STATE_ROOT_RECORD
            && name != MIGRATION_LOCK_FILE
            && !(name.starts_with(MIGRATION_MANIFEST_PREFIX)
                && name.ends_with(MIGRATION_MANIFEST_SUFFIX)
                && read_manifest(&path).is_some_and(|manifest| manifest.schema_version == 1))
        {
            return false;
        }
    }
    true
}

fn state_sessions_only_lock_files(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    for item in entries {
        let Ok(child) = item.map(|entry| entry.path()) else {
            return false;
        };
        let Some(name) = child.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        if name != ".locks" {
            return false;
        }
        let Ok(metadata) = fs::symlink_metadata(&child) else {
            return false;
        };
        if storage::file_type_is_link(metadata.file_type())
            || !metadata.file_type().is_dir()
            || !state_lock_files_are_safe(&child)
        {
            return false;
        }
    }
    true
}

fn state_lock_files_are_safe(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    for item in entries {
        let Ok(child) = item.map(|entry| entry.path()) else {
            return false;
        };
        let Ok(metadata) = fs::symlink_metadata(&child) else {
            return false;
        };
        if storage::file_type_is_link(metadata.file_type()) || !metadata.file_type().is_file() {
            return false;
        }
        let Some(name) = child.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let Some(session_id) = name.strip_suffix(".lock") else {
            return false;
        };
        if storage::validate_component(session_id).is_err() {
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
