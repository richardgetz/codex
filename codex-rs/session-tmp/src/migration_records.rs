//! Validated session and entry records for recovery migration.

use super::ControlState;
use super::MAX_MIGRATION_ENTRIES;
use super::Path;
use super::PathBuf;
use super::SessionTmpError;
use super::liveness::migration_limit_error;
use super::storage;
use crate::AGENTS_DIR;
use crate::ENTRY_METADATA_DIR;
use crate::EntryMetadata;
use crate::LEASES_DIR;
use crate::SESSION_METADATA_FILE;
use std::fs;
use std::io::ErrorKind;

pub(super) fn merge_session_record(
    source: &ControlState,
    target: &ControlState,
    session_id: &str,
) -> Result<(), SessionTmpError> {
    let source_path = source
        .state_session_dir(session_id)
        .join(SESSION_METADATA_FILE);
    let target_dir = target.state_session_dir(session_id);
    let target_path = target_dir.join(SESSION_METADATA_FILE);
    let source_record = match storage::read_session_record(&source_path) {
        Ok(record) => record,
        Err(_) => return Ok(()),
    };
    storage::ensure_directory_not_symlink(&target_dir)?;
    fs::create_dir_all(&target_dir)?;
    storage::set_private_directory(&target_dir)?;
    if let Ok(metadata) = fs::symlink_metadata(&target_path)
        && storage::file_type_is_link(metadata.file_type())
    {
        return Err(SessionTmpError::UnsafeManagedPath(target_path));
    }
    if !target_path.exists() {
        storage::write_json_atomically(&target_path, &source_record)?;
    } else {
        let target_record = storage::read_session_record(&target_path)?;
        if source_record.updated_at > target_record.updated_at {
            storage::write_json_atomically(&target_path, &source_record)?;
        }
    }
    Ok(())
}

pub(super) fn merge_metadata(
    source: &ControlState,
    target: &ControlState,
    session_id: &str,
    moved_paths: &[(PathBuf, PathBuf)],
) -> Result<(), SessionTmpError> {
    let source_dir = source
        .state_session_dir(session_id)
        .join(ENTRY_METADATA_DIR);
    storage::ensure_directory_not_symlink(&source_dir)?;
    if !source_dir.is_dir() {
        return Ok(());
    }
    let target_dir = target
        .state_session_dir(session_id)
        .join(ENTRY_METADATA_DIR);
    storage::ensure_directory_not_symlink(&target_dir)?;
    fs::create_dir_all(&target_dir)?;
    storage::set_private_directory(&target_dir)?;
    let mut items = fs::read_dir(source_dir)?;
    for _ in 0..MAX_MIGRATION_ENTRIES {
        let Some(item) = items.next() else {
            break;
        };
        let source_path = item?.path();
        if source_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("json")
        {
            continue;
        }
        let Ok(mut metadata) = storage::read_metadata(&source_path) else {
            continue;
        };
        if metadata.session_id != session_id
            || storage::validate_component(&metadata.id).is_err()
            || storage::validate_component(&metadata.thread_id).is_err()
            || !valid_metadata_path(&metadata)
        {
            continue;
        }
        let original_path = metadata.path.clone();
        metadata.path = map_relative_path(&original_path, moved_paths);
        if metadata_dir_contains(&target_dir, &metadata)? {
            continue;
        }
        let mut target_path = target_dir.join(format!("{}.json", metadata.id));
        if let Ok(target_metadata) = fs::symlink_metadata(&target_path)
            && storage::file_type_is_link(target_metadata.file_type())
        {
            return Err(SessionTmpError::UnsafeManagedPath(target_path));
        }
        if fs::symlink_metadata(&target_path).is_ok()
            && let Ok(existing) = storage::read_metadata(&target_path)
            && (existing == metadata || metadata_matches_without_id(&existing, &metadata))
        {
            continue;
        }
        let original_id = metadata.id.clone();
        let mut collision_written = false;
        for suffix in 0..32 {
            let migrated_id = if suffix == 0 {
                format!("{original_id}-migrated")
            } else {
                format!("{original_id}-migrated-{suffix}")
            };
            let migrated_path = target_dir.join(format!("{migrated_id}.json"));
            match fs::symlink_metadata(&migrated_path) {
                Ok(file_type) if storage::file_type_is_link(file_type.file_type()) => {
                    return Err(SessionTmpError::UnsafeManagedPath(migrated_path));
                }
                Ok(_) => {
                    let existing = storage::read_metadata(&migrated_path)?;
                    if metadata_matches_without_id(&existing, &metadata) {
                        collision_written = true;
                        break;
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    metadata.id = migrated_id;
                    target_path = migrated_path;
                    collision_written = true;
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        if !collision_written {
            return Err(std::io::Error::new(
                ErrorKind::AlreadyExists,
                "unable to allocate a stable migration metadata collision path",
            )
            .into());
        }
        if target_path.exists() {
            continue;
        }
        storage::write_json_atomically(&target_path, &metadata)?;
    }
    if items.next().transpose()?.is_some() {
        return Err(migration_limit_error("too many legacy metadata entries"));
    }
    Ok(())
}

fn metadata_dir_contains(
    directory: &Path,
    expected: &EntryMetadata,
) -> Result<bool, SessionTmpError> {
    let mut entries = fs::read_dir(directory)?;
    for _ in 0..MAX_MIGRATION_ENTRIES {
        let Some(item) = entries.next() else {
            return Ok(false);
        };
        let path = item?.path();
        if storage::read_metadata(&path)
            .is_ok_and(|existing| metadata_matches_without_id(&existing, expected))
        {
            return Ok(true);
        }
    }
    if entries.next().transpose()?.is_some() {
        return Err(migration_limit_error("too many target metadata entries"));
    }
    Ok(false)
}

fn metadata_matches_without_id(left: &EntryMetadata, right: &EntryMetadata) -> bool {
    left.session_id == right.session_id
        && left.thread_id == right.thread_id
        && left.path == right.path
        && left.purpose == right.purpose
        && left.retention == right.retention
        && left.created_at == right.created_at
        && left.expires_at == right.expires_at
}

pub(super) fn merge_stale_leases(
    source: &ControlState,
    target: &ControlState,
    session_id: &str,
) -> Result<(), SessionTmpError> {
    let source_dir = source.state_session_dir(session_id).join(LEASES_DIR);
    storage::ensure_directory_not_symlink(&source_dir)?;
    if !source_dir.is_dir() {
        return Ok(());
    }
    let target_dir = target.state_session_dir(session_id).join(LEASES_DIR);
    storage::ensure_directory_not_symlink(&target_dir)?;
    fs::create_dir_all(&target_dir)?;
    storage::set_private_directory(&target_dir)?;
    let mut items = fs::read_dir(source_dir)?;
    for _ in 0..MAX_MIGRATION_ENTRIES {
        let Some(item) = items.next() else {
            break;
        };
        let source_path = item?.path();
        let Some(name) = source_path.file_name() else {
            continue;
        };
        let target_path = target_dir.join(name);
        if let Ok(target_metadata) = fs::symlink_metadata(&target_path)
            && storage::file_type_is_link(target_metadata.file_type())
        {
            return Err(SessionTmpError::UnsafeManagedPath(target_path));
        }
        if target_path.exists() {
            continue;
        }
        if let Ok(record) = storage::read_lease_record(&source_path)
            && record.schema_version == 1
            && record.session_id == session_id
            && storage::validate_component(&record.thread_id).is_ok()
            && !storage::lease_is_fresh(&source_path, storage::LEASE_STALE_AFTER)
        {
            storage::write_json_atomically(&target_path, &record)?;
        }
    }
    if items.next().transpose()?.is_some() {
        return Err(migration_limit_error("too many legacy lease entries"));
    }
    Ok(())
}

pub(super) fn valid_metadata_path(metadata: &EntryMetadata) -> bool {
    let mut components = metadata.path.components();
    components
        .next()
        .is_some_and(|component| component.as_os_str() == AGENTS_DIR)
        && components
            .next()
            .is_some_and(|component| component.as_os_str() == metadata.thread_id.as_str())
        && components
            .next()
            .is_some_and(|component| matches!(component, std::path::Component::Normal(_)))
        && components.all(|component| matches!(component, std::path::Component::Normal(_)))
}

pub(super) fn valid_manifest_path(path: &Path) -> bool {
    let mut components = path.components();
    components
        .next()
        .is_some_and(|component| component.as_os_str() == AGENTS_DIR)
        && matches!(components.next(), Some(std::path::Component::Normal(_)))
        && matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.all(|component| matches!(component, std::path::Component::Normal(_)))
}

pub(super) fn map_relative_path(path: &Path, moved_paths: &[(PathBuf, PathBuf)]) -> PathBuf {
    moved_paths
        .iter()
        .filter(|(source, _)| path == source || path.starts_with(source))
        .max_by_key(|(source, _)| source.components().count())
        .map(|(source, target)| {
            let suffix = path.strip_prefix(source).unwrap_or_else(|_| Path::new(""));
            if suffix.as_os_str().is_empty() {
                target.clone()
            } else {
                target.join(suffix)
            }
        })
        .unwrap_or_else(|| path.to_path_buf())
}

pub(super) fn collision_path(source: &Path, path: &Path) -> Result<PathBuf, SessionTmpError> {
    let name = path
        .file_name()
        .ok_or_else(|| SessionTmpError::PathOutsideAgent(path.to_path_buf()))?
        .to_string_lossy();
    let prefix = format!("{name}-migrated-");
    let digest = stable_collision_digest(source, path);
    for suffix in 0..8 {
        let suffix = if suffix == 0 {
            digest.clone()
        } else {
            format!("{digest}-{suffix}")
        };
        let candidate = path.with_file_name(format!("{prefix}{suffix}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(SessionTmpError::Io(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "unable to allocate a collision-safe migration path",
    )))
}

fn stable_collision_digest(source: &Path, target: &Path) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in source
        .to_string_lossy()
        .bytes()
        .chain(target.to_string_lossy().bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
