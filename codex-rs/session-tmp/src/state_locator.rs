//! Stable selection of the external state base for a payload root.

use super::STATE_DIR;
use super::STATE_LOCATORS_DIR;
use super::STATE_SESSION_TMP_DIR;
use super::SessionTmpError;
use super::identity;
use super::storage;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

const STATE_LOCATOR_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Deserialize, Serialize)]
struct StateLocator {
    schema_version: u8,
    root_id: String,
    canonical_payload_root: PathBuf,
    state_base: PathBuf,
}

/// Resolve the durable state base for one payload identity. A tiny locator in
/// the Codex-home default state tree remembers an explicitly configured state
/// base after the configuration changes, so existing leases keep one lock
/// domain instead of silently forking into a new state tree.
pub(crate) fn resolve_state_base(
    default_root: &Path,
    configured_state_base: Option<&Path>,
    root_id: &str,
    canonical_payload_root: &Path,
) -> Result<PathBuf, SessionTmpError> {
    let default_state_base = default_root.join(STATE_DIR).join(STATE_SESSION_TMP_DIR);
    let configured_state_base = configured_state_base.unwrap_or(&default_state_base);
    validate_state_base(&default_state_base, canonical_payload_root)?;

    storage::ensure_directory_not_symlink(&default_state_base)?;
    fs::create_dir_all(&default_state_base)?;
    storage::set_private_directory(&default_state_base)?;
    let locator_dir = default_state_base.join(STATE_LOCATORS_DIR);
    storage::ensure_directory_not_symlink(&locator_dir)?;
    fs::create_dir_all(&locator_dir)?;
    storage::set_private_directory(&locator_dir)?;
    let locator_path = locator_dir.join(format!("{root_id}.json"));
    match fs::symlink_metadata(&locator_path) {
        Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
            Err(SessionTmpError::UnsafeManagedPath(locator_path))
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            Err(SessionTmpError::UnsafeManagedPath(locator_path))
        }
        Ok(_) => {
            let locator = read_state_locator(&locator_path)?;
            if locator.root_id != root_id
                || locator.canonical_payload_root != canonical_payload_root
            {
                return Err(SessionTmpError::RootNotManaged(locator_path));
            }
            validate_state_base(&locator.state_base, canonical_payload_root)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let default_state_base = identity::canonicalize_for_identity(&default_state_base)?;
            let default_state_root = default_state_base.join(root_id);
            if identity::inspect_state_root(&default_state_root, root_id, canonical_payload_root)?
                .is_some()
            {
                Ok(default_state_base)
            } else {
                validate_state_base(configured_state_base, canonical_payload_root)
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_state_base(
    state_base: &Path,
    canonical_payload_root: &Path,
) -> Result<PathBuf, SessionTmpError> {
    if !state_base.is_absolute() {
        return Err(SessionTmpError::RootNotAbsolute(state_base.to_path_buf()));
    }
    identity::ensure_existing_ancestors_for_runtime(state_base)?;
    let canonical_state_base = identity::canonicalize_for_identity(state_base)?;
    if identity::paths_overlap(canonical_payload_root, &canonical_state_base) {
        return Err(SessionTmpError::UnsafeManagedPath(state_base.to_path_buf()));
    }
    Ok(canonical_state_base)
}

/// Finds an already-enrolled state base without creating or modifying any
/// directories. The locator is kept in the default Codex-home state tree so
/// recovery discovery can follow an explicit state-root override even when
/// the payload root is currently markerless.
pub(crate) fn existing_state_base_for_identity(
    default_root: &Path,
    root_id: &str,
    canonical_payload_root: &Path,
) -> Result<PathBuf, SessionTmpError> {
    let default_state_base = default_root.join(STATE_DIR).join(STATE_SESSION_TMP_DIR);
    validate_state_base(&default_state_base, canonical_payload_root)?;
    super::identity::ensure_existing_ancestors_for_runtime(&default_state_base)?;

    let locator_path = default_state_base
        .join(STATE_LOCATORS_DIR)
        .join(format!("{root_id}.json"));
    if let Some(locator_parent) = locator_path.parent() {
        super::identity::ensure_existing_ancestors_for_runtime(locator_parent)?;
    }
    match fs::symlink_metadata(&locator_path) {
        Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
            Err(SessionTmpError::UnsafeManagedPath(locator_path))
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            Err(SessionTmpError::UnsafeManagedPath(locator_path))
        }
        Ok(_) => {
            let locator = read_state_locator(&locator_path)?;
            if locator.root_id != root_id
                || locator.canonical_payload_root != canonical_payload_root
            {
                return Err(SessionTmpError::RootNotManaged(locator_path));
            }
            validate_state_base(&locator.state_base, canonical_payload_root)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            Ok(identity::canonicalize_for_identity(&default_state_base)?)
        }
        Err(error) => Err(error.into()),
    }
}

fn read_state_locator(path: &Path) -> Result<StateLocator, SessionTmpError> {
    let metadata = fs::symlink_metadata(path)?;
    if storage::file_type_is_link(metadata.file_type()) || !metadata.file_type().is_file() {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    let locator: StateLocator = serde_json::from_slice(&fs::read(path)?).map_err(|source| {
        SessionTmpError::InvalidMetadata {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if locator.schema_version != STATE_LOCATOR_SCHEMA_VERSION
        || !locator.state_base.is_absolute()
        || !locator.canonical_payload_root.is_absolute()
    {
        return Err(SessionTmpError::RootNotManaged(path.to_path_buf()));
    }
    Ok(locator)
}

pub(super) fn write_state_locator(
    default_root: &Path,
    root_id: &str,
    canonical_payload_root: &Path,
    state_base: &Path,
) -> Result<(), SessionTmpError> {
    let default_state_base = default_root.join(STATE_DIR).join(STATE_SESSION_TMP_DIR);
    validate_state_base(&default_state_base, canonical_payload_root)?;
    let state_base_identity = validate_state_base(state_base, canonical_payload_root)?;
    storage::ensure_directory_not_symlink(&default_state_base)?;
    fs::create_dir_all(&default_state_base)?;
    storage::set_private_directory(&default_state_base)?;
    let locator_dir = default_state_base.join(STATE_LOCATORS_DIR);
    storage::ensure_directory_not_symlink(&locator_dir)?;
    fs::create_dir_all(&locator_dir)?;
    storage::set_private_directory(&locator_dir)?;
    let locator_path = locator_dir.join(format!("{root_id}.json"));
    let locator = StateLocator {
        schema_version: STATE_LOCATOR_SCHEMA_VERSION,
        root_id: root_id.to_string(),
        canonical_payload_root: canonical_payload_root.to_path_buf(),
        state_base: state_base_identity,
    };
    match fs::symlink_metadata(&locator_path) {
        Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
            return Err(SessionTmpError::UnsafeManagedPath(locator_path));
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(SessionTmpError::UnsafeManagedPath(locator_path));
        }
        Ok(_) => {
            ensure_locator_matches(&locator_path, &locator)?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            match write_locator_create_new(&locator_path, &locator) {
                Ok(()) => {}
                Err(SessionTmpError::Io(error)) if error.kind() == ErrorKind::AlreadyExists => {
                    ensure_locator_matches(&locator_path, &locator)?;
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            ensure_locator_matches(&locator_path, &locator)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn ensure_locator_matches(path: &Path, expected: &StateLocator) -> Result<(), SessionTmpError> {
    let existing = read_state_locator(path)?;
    if existing.root_id != expected.root_id
        || existing.canonical_payload_root != expected.canonical_payload_root
        || existing.state_base != expected.state_base
    {
        return Err(SessionTmpError::RootNotManaged(path.to_path_buf()));
    }
    Ok(())
}

/// Publish a locator without ever replacing a file that appeared concurrently.
/// A temporary file is fully written and synced before a hard link claims the
/// final name; `rename` would permit a late foreign writer to be overwritten.
fn write_locator_create_new(path: &Path, locator: &StateLocator) -> Result<(), SessionTmpError> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "locator has no parent"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "locator has no name"))?
        .to_string_lossy();
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec_pretty(locator)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    storage::set_private_file(&temporary)?;
    drop(file);

    match fs::hard_link(&temporary, path) {
        Ok(()) => {
            fs::remove_file(&temporary)?;
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temporary);
            Err(error.into())
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error.into())
        }
    }
}
