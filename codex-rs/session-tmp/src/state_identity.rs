//! Path identity and state-layout helpers.

use super::LEGACY_MARKER;
use super::LEGACY_MARKER_CONTENT;
use super::RootRecord;
use super::STATE_DIR;
use super::STATE_MARKER;
use super::STATE_MARKER_CONTENT;
use super::STATE_ROOT_RECORD;
use super::STATE_SESSION_TMP_DIR;
use super::SessionTmpError;
use super::V2_PAYLOAD_NAMESPACE;
use super::storage;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn inspect_payload_root(root: &Path) -> Result<bool, SessionTmpError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
            return Err(SessionTmpError::UnsafeManagedPath(root.to_path_buf()));
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(SessionTmpError::UnsafeManagedPath(root.to_path_buf()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let marker = root.join(LEGACY_MARKER);
    if fs::symlink_metadata(&marker)
        .map(|metadata| storage::file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(marker));
    }
    if marker.exists() {
        let content = fs::read_to_string(&marker)?;
        if content != LEGACY_MARKER_CONTENT {
            return Err(SessionTmpError::RootNotManaged(root.to_path_buf()));
        }
        return Ok(true);
    }
    Ok(false)
}

pub(crate) fn payload_is_nonempty(root: &Path) -> Result<bool, SessionTmpError> {
    if !root.exists() {
        return Ok(false);
    }
    Ok(fs::read_dir(root)?.next().transpose()?.is_some())
}

pub(super) fn ensure_payload_root(root: &Path) -> Result<(), SessionTmpError> {
    storage::ensure_directory_not_symlink(root)?;
    fs::create_dir_all(root)?;
    storage::ensure_directory_not_symlink(root)?;
    storage::set_private_directory(root)?;
    Ok(())
}

pub(super) fn ensure_payload_namespace(
    namespace: &Path,
    canonical_payload_root: &Path,
) -> Result<(), SessionTmpError> {
    let canonical_namespace = canonicalize_for_identity(namespace)?;
    if !canonical_namespace.starts_with(canonical_payload_root) {
        return Err(SessionTmpError::UnsafeManagedPath(namespace.to_path_buf()));
    }
    storage::ensure_directory_not_symlink(namespace)?;
    fs::create_dir_all(namespace)?;
    storage::ensure_directory_not_symlink(namespace)?;
    storage::set_private_directory(namespace)?;
    let sessions = namespace.join("sessions");
    storage::ensure_directory_not_symlink(&sessions)?;
    fs::create_dir_all(&sessions)?;
    storage::set_private_directory(&sessions)?;
    Ok(())
}

pub(super) fn inspect_state_root(
    state_root: &Path,
    root_id: &str,
    canonical_payload_root: &Path,
) -> Result<Option<PathBuf>, SessionTmpError> {
    match fs::symlink_metadata(state_root) {
        Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
            return Err(SessionTmpError::UnsafeManagedPath(state_root.to_path_buf()));
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(SessionTmpError::UnsafeManagedPath(state_root.to_path_buf()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let marker = state_root.join(STATE_MARKER);
    if fs::symlink_metadata(&marker)
        .map(|metadata| storage::file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(marker));
    }
    if fs::read_to_string(&marker).ok().as_deref() != Some(STATE_MARKER_CONTENT) {
        return Err(SessionTmpError::RootNotManaged(state_root.to_path_buf()));
    }
    let record = read_root_record(&state_root.join(STATE_ROOT_RECORD))?;
    let namespace_is_safe = record.payload_namespace.is_absolute()
        && canonicalize_for_identity(&record.payload_namespace)
            .is_ok_and(|namespace| namespace.starts_with(canonical_payload_root));
    if record.schema_version != 1
        || record.root_id != root_id
        || record.canonical_payload_root != canonical_payload_root
        || !namespace_is_safe
    {
        return Err(SessionTmpError::RootNotManaged(state_root.to_path_buf()));
    }
    Ok(Some(record.payload_namespace))
}

pub(super) fn initialize_state_root(
    state_root: &Path,
    root_id: &str,
    payload_root: &Path,
    canonical_payload_root: &Path,
    use_v2_namespace: bool,
) -> Result<(), SessionTmpError> {
    fs::create_dir_all(state_root)?;
    storage::ensure_directory_not_symlink(state_root)?;
    storage::set_private_directory(state_root)?;
    let marker = state_root.join(STATE_MARKER);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(STATE_MARKER_CONTENT.as_bytes())?;
            file.sync_all()?;
            storage::set_private_file(&marker)?;
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if fs::read_to_string(&marker)? != STATE_MARKER_CONTENT {
                return Err(SessionTmpError::RootNotManaged(state_root.to_path_buf()));
            }
        }
        Err(error) => return Err(error.into()),
    }
    let record = RootRecord {
        schema_version: 1,
        root_id: root_id.to_string(),
        payload_root: payload_root.to_path_buf(),
        canonical_payload_root: canonical_payload_root.to_path_buf(),
        payload_namespace: if use_v2_namespace {
            payload_root.join(V2_PAYLOAD_NAMESPACE)
        } else {
            payload_root.to_path_buf()
        },
    };
    let record_path = state_root.join(STATE_ROOT_RECORD);
    if record_path.exists() {
        let existing = read_root_record(&record_path)?;
        if existing.root_id != record.root_id
            || existing.canonical_payload_root != record.canonical_payload_root
            || existing.payload_namespace != record.payload_namespace
        {
            return Err(SessionTmpError::RootNotManaged(state_root.to_path_buf()));
        }
    } else {
        storage::write_json_atomically(&record_path, &record)?;
    }
    Ok(())
}

pub(super) fn read_root_record(path: &Path) -> Result<RootRecord, SessionTmpError> {
    if fs::symlink_metadata(path)
        .map(|metadata| storage::file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(|source| SessionTmpError::InvalidMetadata {
        path: path.to_path_buf(),
        source,
    })
}

/// Returns the payload path recorded by a validated external state root.
/// Migration uses this to finish a source whose disposable root was removed
/// after its manifest was persisted. Invalid or incomplete state is ignored so
/// an arbitrary state directory cannot authorize adoption of a payload path.
pub(crate) fn payload_root_from_state_root(
    state_root: &Path,
    expected_root_id: &str,
) -> Result<Option<PathBuf>, SessionTmpError> {
    let metadata = match fs::symlink_metadata(state_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if storage::file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir() {
        return Err(SessionTmpError::UnsafeManagedPath(state_root.to_path_buf()));
    }
    if fs::read_to_string(state_root.join(STATE_MARKER))
        .ok()
        .as_deref()
        != Some(STATE_MARKER_CONTENT)
    {
        return Ok(None);
    }
    let Ok(record) = read_root_record(&state_root.join(STATE_ROOT_RECORD)) else {
        return Ok(None);
    };
    if record.schema_version != 1
        || record.root_id != expected_root_id
        || !record.payload_root.is_absolute()
        || !record.canonical_payload_root.is_absolute()
    {
        return Ok(None);
    }
    let Ok(canonical_payload_root) = canonicalize_for_identity(&record.payload_root) else {
        return Ok(None);
    };
    if root_id(&canonical_payload_root) != expected_root_id
        || canonical_payload_root != record.canonical_payload_root
    {
        return Ok(None);
    }
    let Ok(canonical_namespace) = canonicalize_for_identity(&record.payload_namespace) else {
        return Ok(None);
    };
    if !canonical_namespace.starts_with(&canonical_payload_root) {
        return Ok(None);
    }
    Ok(Some(record.payload_root))
}

pub(crate) fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| {
            metadata.file_type().is_dir() && !storage::file_type_is_link(metadata.file_type())
        })
        .unwrap_or(false)
}

pub(crate) fn canonicalize_for_identity(path: &Path) -> Result<PathBuf, SessionTmpError> {
    if path.exists() {
        return Ok(fs::canonicalize(path)?);
    }
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        let Some(name) = cursor.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        cursor = cursor
            .parent()
            .ok_or_else(|| SessionTmpError::RootNotAbsolute(path.to_path_buf()))?;
    }
    let mut canonical = fs::canonicalize(cursor)?;
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

pub(crate) fn root_id(canonical_payload_root: &Path) -> String {
    // FNV-1a is tiny, deterministic across processes/platforms, and keeps
    // the state directory name to one validated component without adding a
    // dependency solely for hashing a path identity.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in canonical_payload_root.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub(super) fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Returns whether an external identity already enrolls `payload_root`.
/// Unlike [`ControlState::open`], this probe never creates directories or
/// recreates a deleted payload root, so it is safe for migration discovery.
pub(crate) fn external_identity_present(
    default_root: &Path,
    payload_root: &Path,
) -> Result<bool, SessionTmpError> {
    if !default_root.is_absolute() || !payload_root.is_absolute() {
        return Ok(false);
    }
    let canonical_payload_root = canonicalize_for_identity(payload_root)?;
    let state_base = default_root.join(STATE_DIR).join(STATE_SESSION_TMP_DIR);
    let state_root = state_base.join(root_id(&canonical_payload_root));
    match fs::symlink_metadata(&state_root) {
        Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
            return Err(SessionTmpError::UnsafeManagedPath(state_root));
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(SessionTmpError::UnsafeManagedPath(state_root));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    Ok(inspect_state_root(
        &state_root,
        &root_id(&canonical_payload_root),
        &canonical_payload_root,
    )
    .is_ok_and(|namespace| namespace.is_some()))
}

pub(crate) fn ensure_existing_ancestors_for_runtime(path: &Path) -> Result<(), SessionTmpError> {
    let mut cursor = Some(path);
    while let Some(candidate) = cursor {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
                return Err(SessionTmpError::UnsafeManagedPath(candidate.to_path_buf()));
            }
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(SessionTmpError::UnsafeManagedPath(candidate.to_path_buf()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        cursor = candidate.parent();
    }
    Ok(())
}
