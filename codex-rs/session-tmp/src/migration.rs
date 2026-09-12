//! Safe, rerunnable migration from marker-managed roots.
//!
//! Older releases kept control files below the disposable payload root.  A
//! legacy root is copied into external state on open.  The historical recovery
//! root receives one additional consolidation pass into the normal default
//! root; sessions with old liveness state are left in place until a later
//! open observes that they have been released.

use super::state;
use super::storage;
use super::types::SessionTmpError;
use super::state::ControlState;
use crate::SESSIONS_DIR;
use crate::SESSION_METADATA_FILE;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::path::PathBuf;

const MIGRATION_LOCK_FILE: &str = ".migration.lock";
const MIGRATION_MANIFEST_PREFIX: &str = ".migration-";
const MIGRATION_MANIFEST_SUFFIX: &str = ".json";
const MAX_MIGRATION_SESSIONS: usize = 2_000;
const MAX_MIGRATION_ENTRIES: usize = 4_000;

#[path = "migration_liveness.rs"]
mod liveness;
#[path = "migration_manifest.rs"]
mod manifest;
#[path = "migration_merge.rs"]
mod merge;
#[path = "migration_records.rs"]
mod records;
#[path = "migration_retire.rs"]
mod retire;

#[derive(Debug, Deserialize, Serialize)]
struct MigrationManifest {
    schema_version: u8,
    source_root_id: String,
    target_root_id: String,
    phase: String,
    updated_at: u64,
    #[serde(default)]
    source_payload_root: Option<PathBuf>,
    moved_paths: Vec<ManifestMove>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ManifestMove {
    source: PathBuf,
    target: PathBuf,
}

/// Returns whether the historical recovery root has enough validated identity
/// to be considered for consolidation.  This probe has no side effects.
pub(super) fn recovery_is_enrolled(default_root: &Path) -> bool {
    let recovery_root = default_root.join(state::LEGACY_RECOVERY_ROOT);
    if !recovery_root.exists() {
        return false;
    }
    match state::inspect_payload_root(&recovery_root) {
        Ok(true) => true,
        Ok(false) => state::payload_is_nonempty(&recovery_root)
            .ok()
            .unwrap_or(false)
            && state::external_identity_present(default_root, &recovery_root)
                .ok()
                .unwrap_or(false),
        Err(_) => false,
    }
}

/// Returns whether a validated recovery session still has a fresh legacy
/// lease. An active old-version writer keeps the normal payload namespace as
/// its compatibility destination; once released, the next open can use the
/// hidden migration namespace or complete an existing external enrollment.
pub(super) fn recovery_has_live_legacy_lease(
    default_root: &Path,
) -> Result<bool, SessionTmpError> {
    let recovery_root = default_root.join(state::LEGACY_RECOVERY_ROOT);
    let sessions_dir = recovery_root.join(SESSIONS_DIR);
    storage::ensure_directory_not_symlink(&sessions_dir)?;
    if !sessions_dir.is_dir() {
        return Ok(false);
    }
    let mut entries = fs::read_dir(&sessions_dir)?;
    for _ in 0..MAX_MIGRATION_SESSIONS {
        let Some(entry) = entries.next() else {
            return Ok(false);
        };
        let path = entry?.path();
        let Some(session_id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if storage::validate_component(session_id).is_err() || !state::is_real_directory(&path) {
            continue;
        }
        let Ok(record) = storage::read_session_record(&path.join(SESSION_METADATA_FILE)) else {
            continue;
        };
        if record.schema_version != 1 || record.session_id != session_id {
            continue;
        }
        if matches!(
            storage::try_lock_legacy_session(&recovery_root, session_id)?,
            storage::LegacyLock::Unavailable
        ) {
            return Ok(true);
        }
        if storage::has_fresh_lease(
            &path.join(storage::LEASES_DIR),
            storage::LEASE_STALE_AFTER,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Returns whether a durable target manifest still points at the historical
/// recovery path. This lets startup resume a state-only retirement after the
/// recovery payload directory has already disappeared.
pub(super) fn recovery_manifest_pending(default_root: &Path) -> bool {
    manifest::recovery_manifest_pending(default_root)
}

/// Consolidate a validated historical recovery root into the normal default
/// payload tree. Unknown files and live source sessions are preserved.
pub(super) fn consolidate_recovery(
    target: &ControlState,
    default_root: &Path,
) -> Result<(), SessionTmpError> {
    if target.payload_root != default_root.join("session-tmp") {
        return Ok(());
    }
    let recovery_root = default_root.join(state::LEGACY_RECOVERY_ROOT);
    if recovery_root == target.payload_root {
        return Ok(());
    }
    let pending_manifest = pending_recovery_manifest(target, &recovery_root)?;
    if !recovery_root.exists() && pending_manifest.is_none() {
        return Ok(());
    }
    let source_payload_root = pending_manifest
        .as_ref()
        .and_then(|(_, manifest)| manifest.source_payload_root.clone())
        .unwrap_or_else(|| recovery_root.clone());
    let canonical_source_payload = state::canonicalize_for_identity(&source_payload_root)?;
    let source_root_id = state::root_id(&canonical_source_payload);
    if source_root_id == target.root_id() {
        return Ok(());
    }
    // Acquire the per-root migration barriers before opening the source. This
    // closes the gap where a new manager could enroll a source root between
    // source discovery and lock creation. `open_for_migration` skips its
    // normal wait because this call already owns both barriers.
    let (first_lock, second_lock) = if target.root_id() < source_root_id.as_str() {
        (
            lock_migration(target)?,
            lock_migration_path(target.state_base(), &source_root_id)?,
        )
    } else {
        (
            lock_migration_path(target.state_base(), &source_root_id)?,
            lock_migration(target)?,
        )
    };
    let source_state_root = target.state_base().join(&source_root_id);
    let _first_lock = first_lock;
    let _second_lock = second_lock;
    // Hold both historical barriers across source discovery and initialization
    // in the same root-id order as the global barriers. This keeps old
    // marker-managed writers in both domains from racing consolidation.
    let (_first_state_migration_lock, _second_state_migration_lock) =
        if target.root_id() < source_root_id.as_str() {
            (
                storage::wait_for_migration_lock(
                    &target.state_root().join(MIGRATION_LOCK_FILE),
                )?,
                storage::wait_for_migration_lock(&source_state_root.join(MIGRATION_LOCK_FILE))?,
            )
        } else {
            (
                storage::wait_for_migration_lock(&source_state_root.join(MIGRATION_LOCK_FILE))?,
                storage::wait_for_migration_lock(
                    &target.state_root().join(MIGRATION_LOCK_FILE),
                )?,
            )
        };
    if !recovery_root.exists()
        && !source_state_root.exists()
        && let Some((_, manifest)) = pending_manifest.as_ref()
    {
        let manifest_path = target.state_root.join(format!(
            "{MIGRATION_MANIFEST_PREFIX}{}{MIGRATION_MANIFEST_SUFFIX}",
            manifest.source_root_id
        ));
        complete_manifest_without_source(&manifest_path, target, manifest)?;
        return Ok(());
    }
    if recovery_root.exists() && !recovery_is_enrolled(default_root) && pending_manifest.is_none() {
        return Ok(());
    }
    let source = match ControlState::open_for_migration(default_root, &source_payload_root) {
        Ok(source) => source,
        Err(error) => {
            tracing::debug!(
                error = %error,
                recovery_root = %recovery_root.display(),
                "deferring recovery-root consolidation"
            );
            return Ok(());
        }
    };
    if let Err(error) = source.import_legacy_control() {
        tracing::debug!(
            error = %error,
            recovery_root = %source.payload_root().display(),
            "deferring recovery-root control import"
        );
        return Ok(());
    }
    let manifest_path = target.state_root.join(format!(
        "{MIGRATION_MANIFEST_PREFIX}{}{MIGRATION_MANIFEST_SUFFIX}",
        source.root_id
    ));
    let mut moved_paths = match fs::symlink_metadata(&manifest_path) {
        Ok(metadata)
            if storage::file_type_is_link(metadata.file_type()) || !metadata.file_type().is_file() =>
        {
            return Err(SessionTmpError::UnsafeManagedPath(manifest_path));
        }
        Ok(_) => {
            let Some(manifest) = read_manifest(&manifest_path) else {
                // A corrupt manifest is treated as an operator-owned artifact;
                // never overwrite it while trying to resume migration.
                tracing::debug!(
                    path = %manifest_path.display(),
                    "deferring recovery-root migration with corrupt manifest"
                );
                return Ok(());
            };
            if manifest.schema_version != 1
                || manifest.source_root_id != source.root_id
                || manifest.target_root_id != target.root_id
            {
                tracing::debug!(
                    path = %manifest_path.display(),
                    "deferring recovery-root migration with mismatched manifest"
                );
                return Ok(());
            }
            if manifest.moved_paths.iter().any(|movement| {
                !records::valid_manifest_path(&movement.source)
                    || !records::valid_manifest_path(&movement.target)
            }) {
                tracing::debug!(
                    path = %manifest_path.display(),
                    "deferring recovery-root migration with unsafe manifest paths"
                );
                return Ok(());
            }
            manifest
                .moved_paths
                .into_iter()
                .map(|path| (path.source, path.target))
                .collect::<Vec<_>>()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    write_manifest(
        &manifest_path,
        &source,
        target,
        "in_progress",
        &moved_paths,
    )?;

    let session_ids = match liveness::collect_session_ids(&source) {
        Ok(session_ids) => session_ids,
        Err(error) => {
            tracing::debug!(
                error = %error,
                recovery_root = %source.payload_root().display(),
                "deferring recovery-root session discovery"
            );
            return Ok(());
        }
    };
    let mut deferred = false;
    for session_id in session_ids.into_iter().take(MAX_MIGRATION_SESSIONS) {
        let source_is_live = match liveness::session_is_live(&source, &session_id) {
            Ok(source_is_live) => source_is_live,
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    session_id = %session_id,
                    "deferring recovery-root session with unsafe liveness state"
                );
                deferred = true;
                continue;
            }
        };
        if source_is_live {
            deferred = true;
            continue;
        }
        let mut persist_moves = |session_moved_paths: &[(PathBuf, PathBuf)]| {
            let mut all_paths = moved_paths.clone();
            all_paths.extend(session_moved_paths.iter().cloned());
            write_manifest(
                &manifest_path,
                &source,
                target,
                "in_progress",
                &all_paths,
            )
        };
        let merge = match merge::merge_session(
            &source,
            target,
            &session_id,
            &moved_paths,
            &mut persist_moves,
        ) {
            Ok(result) => result,
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    session_id = %session_id,
                    "deferring recovery-root session merge"
                );
                deferred = true;
                continue;
            }
        };
        if !merge.merged {
            if !merge.moved_paths.is_empty() {
                moved_paths.extend(merge.moved_paths);
                write_manifest(
                    &manifest_path,
                    &source,
                    target,
                    "in_progress",
                    &moved_paths,
                )?;
            }
            deferred = true;
        } else {
            moved_paths.extend(merge.moved_paths.iter().cloned());
            write_manifest(
                &manifest_path,
                &source,
                target,
                "in_progress",
                &moved_paths,
            )?;
            if let Err(error) = merge.retire_source(&source, target, &session_id) {
                tracing::debug!(
                    error = %error,
                    session_id = %session_id,
                    "deferring recovery-root source-control retirement"
                );
                deferred = true;
            }
        }
    }
    // Unknown recovery content is deliberately left in its original tree and
    // excluded from managed cleanup. The source can be retired only after all
    // validated records are gone and no unrecognized content remains. Payload
    // copies stay in the source until their mappings are durable; keep
    // migration locks held through the exact source cleanup. Old binaries do
    // not honor these locks, so retirement itself must be non-recursive.
    let source_retirable = retire::source_root_is_retirable(&source).unwrap_or(false);
    if !deferred && source_retirable {
        // Retire control state first while the migration locks still exclude
        // new managers.  The payload root is removed only after that state is
        // gone, so a crash cannot strand an external source tree with no
        // recovery path.  The next open can still complete a legacy
        // payload-only retirement from the durable manifest.
        let state_retired = retire::retire_source_state(&source)?;
        let payload_retired = state_retired && retire::retire_source_payload(&source)?;
        if state_retired {
            drop(_second_lock);
            drop(_first_lock);
        }
        if payload_retired {
            write_manifest(
                &manifest_path,
                &source,
                target,
                "complete",
                &moved_paths,
            )?;
        }
    }
    Ok(())
}

fn lock_migration(state: &ControlState) -> Result<File, SessionTmpError> {
    lock_migration_path(state.state_base(), state.root_id())
}

/// Acquire the root barrier while a manager recreates its payload namespace
/// and initial session records. The guard is intentionally short-lived: once
/// the manager is returned, session and lease locks provide normal runtime
/// coordination while the persistent barrier pathname remains available to
/// future migrations.
pub(super) fn lock_for_open(state: &ControlState) -> Result<File, SessionTmpError> {
    lock_migration(state)
}

fn lock_migration_path(state_base: &Path, root_id: &str) -> Result<File, SessionTmpError> {
    let path = state_base
        .join(".migration-locks")
        .join(format!("{root_id}.lock"));
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "migration lock has no parent"))?;
    storage::ensure_directory_not_symlink(parent)?;
    fs::create_dir_all(parent)?;
    storage::set_private_directory(parent)?;
    if fs::symlink_metadata(&path)
        .map(|metadata| storage::file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(path));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)?;
    storage::set_private_file(&path)?;
    file.lock()?;
    let contents = fs::read(&path)?;
    if !contents.is_empty() && contents.as_slice() != storage::MIGRATION_LOCK_CONTENT {
        return Err(SessionTmpError::UnsafeManagedPath(path));
    }
    if contents.is_empty() {
        use std::io::Write;

        file.write_all(storage::MIGRATION_LOCK_CONTENT)?;
        file.sync_data()?;
    }
    Ok(file)
}

fn write_manifest(
    path: &Path,
    source: &ControlState,
    target: &ControlState,
    phase: &str,
    moved_paths: &[(PathBuf, PathBuf)],
) -> Result<(), SessionTmpError> {
    storage::write_json_atomically(
        path,
        &MigrationManifest {
            schema_version: 1,
            source_root_id: source.root_id.clone(),
            target_root_id: target.root_id.clone(),
            phase: phase.to_string(),
            updated_at: storage::now_seconds(),
            source_payload_root: Some(source.payload_root().to_path_buf()),
            moved_paths: moved_paths
                .iter()
                .map(|(source, target)| ManifestMove {
                    source: source.clone(),
                    target: target.clone(),
                })
                .collect(),
        },
    )
}

fn read_manifest(path: &Path) -> Option<MigrationManifest> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| !storage::file_type_is_link(metadata.file_type()))
        .and_then(|_| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn pending_recovery_manifest(
    target: &ControlState,
    recovery_root: &Path,
) -> Result<Option<(PathBuf, MigrationManifest)>, SessionTmpError> {
    let mut entries = fs::read_dir(target.state_root())?;
    for _ in 0..32 {
        let Some(entry) = entries.next() else {
            return Ok(None);
        };
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(MIGRATION_MANIFEST_PREFIX)
            || !name.ends_with(MIGRATION_MANIFEST_SUFFIX)
        {
            continue;
        }
        let Some(manifest) = read_manifest(&path) else {
            continue;
        };
        if manifest.schema_version != 1
            || manifest.target_root_id != target.root_id
            || manifest.phase == "complete"
            || storage::validate_component(&manifest.source_root_id).is_err()
        {
            continue;
        }
        let source_state_root = target.state_base().join(&manifest.source_root_id);
        match fs::symlink_metadata(&source_state_root) {
            Ok(metadata)
                if storage::file_type_is_link(metadata.file_type())
                    || !metadata.file_type().is_dir() =>
            {
                return Err(SessionTmpError::UnsafeManagedPath(source_state_root));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let Some(source_payload_root) = manifest
            .source_payload_root
            .clone()
            .or_else(|| state::payload_root_from_state_root(&source_state_root, &manifest.source_root_id).ok().flatten())
        else {
            continue;
        };
        let Ok(canonical_source) = state::canonicalize_for_identity(&source_payload_root) else {
            continue;
        };
        let Ok(canonical_recovery) = state::canonicalize_for_identity(recovery_root) else {
            continue;
        };
        if canonical_source == canonical_recovery {
            return Ok(Some((source_payload_root, manifest)));
        }
    }
    Ok(None)
}

fn complete_manifest_without_source(
    path: &Path,
    target: &ControlState,
    manifest: &MigrationManifest,
) -> Result<(), SessionTmpError> {
    let Some(current) = read_manifest(path) else {
        return Ok(());
    };
    if current.schema_version != 1
        || current.source_root_id != manifest.source_root_id
        || current.target_root_id != target.root_id
        || current.phase == "complete"
    {
        return Ok(());
    }
    storage::write_json_atomically(
        path,
        &MigrationManifest {
            schema_version: 1,
            source_root_id: current.source_root_id,
            target_root_id: current.target_root_id,
            phase: "complete".to_string(),
            updated_at: storage::now_seconds(),
            source_payload_root: current.source_payload_root,
            moved_paths: current.moved_paths,
        },
    )
}
