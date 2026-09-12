//! Safe, rerunnable migration from marker-managed roots.
//!
//! Older releases kept control files below the disposable payload root.  A
//! legacy root is copied into external state on open.  The historical recovery
//! root receives one additional consolidation pass into the normal default
//! root; sessions with old liveness state are left in place until a later
//! open observes that they have been released.

use super::state;
use super::storage;
use super::types::EntryMetadata;
use super::types::SessionTmpError;
use super::AGENTS_DIR;
use super::ENTRY_METADATA_DIR;
use super::LEASES_DIR;
use super::SESSION_METADATA_FILE;
use super::SESSIONS_DIR;
use super::state::ControlState;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

const MIGRATION_LOCK_FILE: &str = ".migration.lock";
const MIGRATION_MANIFEST_PREFIX: &str = ".migration-";
const MIGRATION_MANIFEST_SUFFIX: &str = ".json";
const MAX_MIGRATION_SESSIONS: usize = 2_000;
const MAX_MIGRATION_ENTRIES: usize = 4_000;

#[derive(Debug, Deserialize, Serialize)]
struct MigrationManifest {
    schema_version: u8,
    source_root_id: String,
    target_root_id: String,
    phase: String,
    updated_at: u64,
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
    if recovery_root == target.payload_root || !recovery_root.exists() {
        return Ok(());
    }
    if !recovery_is_enrolled(default_root) {
        return Ok(());
    }

    // A missing payload root with only external state is already retired. Do
    // not recreate it merely to discover whether it has work to migrate.
    if !recovery_root.exists() {
        return Ok(());
    }
    let source = match ControlState::open_for_migration(default_root, &recovery_root) {
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
    let (first, second) = if target.root_id < source.root_id {
        (target, &source)
    } else {
        (&source, target)
    };
    let _first_lock = match lock_migration(first) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::debug!(error = %error, "deferring recovery-root migration lock");
            return Ok(());
        }
    };
    let _second_lock = match lock_migration(second) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::debug!(error = %error, "deferring recovery-root migration lock");
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
    let mut moved_paths = read_manifest(&manifest_path)
        .filter(|manifest| {
            manifest.schema_version == 1
                && manifest.source_root_id == source.root_id
                && manifest.target_root_id == target.root_id
        })
        .map(|manifest| {
            manifest
                .moved_paths
                .into_iter()
                .map(|path| (path.source, path.target))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    write_manifest(
        &manifest_path,
        &source,
        target,
        "in_progress",
        &moved_paths,
    )?;

    let session_ids = match collect_session_ids(&source) {
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
        let source_is_live = match session_is_live(&source, &session_id) {
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
        let (merged, session_moves) = match merge_session(&source, target, &session_id) {
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
        if !merged {
            deferred = true;
        } else {
            moved_paths.extend(session_moves);
            write_manifest(
                &manifest_path,
                &source,
                target,
                "in_progress",
                &moved_paths,
            )?;
        }
    }
    if !deferred {
        // Move only leftovers that are outside the validated session/control
        // set to a private quarantine below the target namespace. This keeps
        // the recovery tree removable without adopting or deleting arbitrary
        // files that happened to be stored there.
        write_manifest(
            &manifest_path,
            &source,
            target,
            "quarantine",
            &moved_paths,
        )?;
        if let Err(error) = quarantine_unknown(&source, target) {
            tracing::debug!(
                error = %error,
                recovery_root = %source.payload_root().display(),
                "deferring recovery-root quarantine"
            );
            return Ok(());
        }
    }
    // An old binary can release a lease between the initial check and the
    // merge. Recheck before deleting any source controls or root directory.
    if !deferred && source_root_is_retirable(&source).unwrap_or(false) {
        drop(_second_lock);
        drop(_first_lock);
        retire_source(&source)?;
        write_manifest(
            &manifest_path,
            &source,
            target,
            "complete",
            &moved_paths,
        )?;
    }
    Ok(())
}

fn quarantine_unknown(
    source: &ControlState,
    target: &ControlState,
) -> Result<(), SessionTmpError> {
    let payload_quarantine = target
        .payload_namespace()
        .join(".codex-session-tmp-migration")
        .join(source.root_id());
    let state_quarantine = target
        .state_root()
        .join(".migration-quarantine")
        .join(source.root_id());
    storage::ensure_directory_not_symlink(target.payload_namespace())?;
    storage::ensure_directory_not_symlink(target.state_root())?;
    for directory in [&payload_quarantine, &state_quarantine] {
        if let Some(parent) = directory.parent() {
            storage::ensure_directory_not_symlink(parent)?;
            fs::create_dir_all(parent)?;
            storage::set_private_directory(parent)?;
        }
        storage::ensure_directory_not_symlink(directory)?;
        fs::create_dir_all(directory)?;
        storage::set_private_directory(directory)?;
    }

    quarantine_payload_root(source, &payload_quarantine)?;
    quarantine_state_root(source, &state_quarantine)?;
    Ok(())
}

fn quarantine_payload_root(
    source: &ControlState,
    destination: &Path,
) -> Result<(), SessionTmpError> {
    let root = source.payload_root();
    storage::ensure_directory_not_symlink(root)?;
    if !root.is_dir() {
        return Ok(());
    }
    for item in fs::read_dir(root)? {
        let path = item?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == state::LEGACY_MARKER {
            continue;
        }
        if name == SESSIONS_DIR {
            quarantine_payload_sessions(source, &path, &destination.join(SESSIONS_DIR))?;
        } else {
            move_to_quarantine(&path, &destination.join(name))?;
        }
    }
    Ok(())
}

fn quarantine_payload_sessions(
    source: &ControlState,
    sessions_dir: &Path,
    destination: &Path,
) -> Result<(), SessionTmpError> {
    storage::ensure_directory_not_symlink(sessions_dir)?;
    if !sessions_dir.is_dir() {
        return Ok(());
    }
    for item in fs::read_dir(sessions_dir)? {
        let path = item?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == ".locks" {
            storage::ensure_directory_not_symlink(&path)?;
            if !path.is_dir() {
                move_to_quarantine(&path, &destination.join(name))?;
                continue;
            }
            for lock in fs::read_dir(&path)? {
                let lock_path = lock?.path();
                let Some(lock_name) = lock_path.file_stem().and_then(|name| name.to_str()) else {
                    continue;
                };
                if storage::validate_component(lock_name).is_err() {
                    return Err(SessionTmpError::RootNotManaged(lock_path));
                }
                let can_move = match storage::try_lock_legacy_session(source.payload_root(), lock_name)? {
                    storage::LegacyLock::Held(guard) => {
                        drop(guard);
                        true
                    }
                    storage::LegacyLock::Absent => true,
                    storage::LegacyLock::Unavailable => {
                        return Err(SessionTmpError::SessionAlreadyOwned(lock_name.to_string()));
                    }
                };
                if can_move {
                    move_to_quarantine(
                        &lock_path,
                        &destination.join(name).join(lock_path.file_name().unwrap_or_default()),
                    )?;
                }
            }
            if fs::read_dir(&path)?.next().transpose()?.is_none() {
                fs::remove_dir(&path)?;
            }
            continue;
        }
        if storage::validate_component(name).is_ok() {
            if session_is_live(source, name)? {
                return Err(SessionTmpError::SessionAlreadyOwned(name.to_string()));
            }
            match storage::try_lock_legacy_session(source.payload_root(), name)? {
                storage::LegacyLock::Held(_guard) | storage::LegacyLock::Absent => {}
                storage::LegacyLock::Unavailable => {
                    return Err(SessionTmpError::SessionAlreadyOwned(name.to_string()));
                }
            }
        }
        move_to_quarantine(&path, &destination.join(name))?;
    }
    if fs::read_dir(sessions_dir)?.next().transpose()?.is_none() {
        fs::remove_dir(sessions_dir)?;
    }
    Ok(())
}

fn quarantine_state_root(
    source: &ControlState,
    destination: &Path,
) -> Result<(), SessionTmpError> {
    let root = source.state_root();
    storage::ensure_directory_not_symlink(root)?;
    if !root.is_dir() {
        return Ok(());
    }
    for item in fs::read_dir(root)? {
        let path = item?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let managed = name == state::STATE_MARKER
            || name == state::STATE_ROOT_RECORD
            || name == MIGRATION_LOCK_FILE;
        let is_manifest = name.starts_with(MIGRATION_MANIFEST_PREFIX)
            && name.ends_with(MIGRATION_MANIFEST_SUFFIX)
            && read_manifest(&path).is_some_and(|manifest| manifest.schema_version == 1);
        if name == state::STATE_SESSIONS_DIR {
            quarantine_state_sessions(source, &path, &destination.join(name))?;
        } else if name == state::STATE_LOCKS_DIR {
            quarantine_state_locks(source, &path, &destination.join(name))?;
        } else if !managed && !is_manifest {
            move_to_quarantine(&path, &destination.join(name))?;
        }
    }
    Ok(())
}

fn quarantine_state_sessions(
    source: &ControlState,
    root: &Path,
    destination: &Path,
) -> Result<(), SessionTmpError> {
    storage::ensure_directory_not_symlink(root)?;
    if !root.is_dir() {
        return Ok(());
    }
    for item in fs::read_dir(root)? {
        let path = item?.path();
        let Some(name) = path.file_name() else {
            continue;
        };
        let Some(session_id) = name.to_str() else {
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
        if record.schema_version != 1 || record.session_id != session_id {
            continue;
        }
        if session_is_live(source, session_id)? {
            return Err(SessionTmpError::SessionAlreadyOwned(session_id.to_string()));
        }
        match storage::try_lock_existing_session(&path)? {
            storage::ExistingSessionLock::Held(lock) => drop(lock),
            storage::ExistingSessionLock::Absent => {}
            storage::ExistingSessionLock::Unavailable => {
                return Err(SessionTmpError::SessionAlreadyOwned(session_id.to_string()));
            }
        }
        move_to_quarantine(&path, &destination.join(name))?;
    }
    Ok(())
}

fn quarantine_state_locks(
    source: &ControlState,
    root: &Path,
    destination: &Path,
) -> Result<(), SessionTmpError> {
    storage::ensure_directory_not_symlink(root)?;
    if !root.is_dir() {
        return Ok(());
    }
    for item in fs::read_dir(root)? {
        let path = item?.path();
        let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if storage::validate_component(name).is_err() {
            continue;
        }
        let session_dir = source.state_session_dir(name);
        if session_dir.is_dir() && session_is_live(source, name)? {
            return Err(SessionTmpError::SessionAlreadyOwned(name.to_string()));
        }
        if session_dir.is_dir() {
            match storage::try_lock_existing_session(&session_dir)? {
                storage::ExistingSessionLock::Held(lock) => drop(lock),
                storage::ExistingSessionLock::Absent => {}
                storage::ExistingSessionLock::Unavailable => {
                    return Err(SessionTmpError::SessionAlreadyOwned(name.to_string()));
                }
            }
        }
        move_to_quarantine(&path, &destination.join(path.file_name().unwrap_or_default()))?;
    }
    Ok(())
}

fn move_to_quarantine(source: &Path, destination: &Path) -> Result<(), SessionTmpError> {
    let destination = if fs::symlink_metadata(destination).is_ok() {
        collision_path(destination)?
    } else {
        destination.to_path_buf()
    };
    if let Some(parent) = destination.parent() {
        state::ensure_existing_ancestors_for_runtime(parent)?;
        storage::ensure_directory_not_symlink(parent)?;
        fs::create_dir_all(parent)?;
    }
    fs::rename(source, destination)?;
    Ok(())
}

fn collect_session_ids(source: &ControlState) -> Result<Vec<String>, SessionTmpError> {
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

fn migration_limit_error(message: &str) -> SessionTmpError {
    SessionTmpError::Io(std::io::Error::new(ErrorKind::InvalidData, message))
}

fn session_is_live(source: &ControlState, session_id: &str) -> Result<bool, SessionTmpError> {
    let state_session = source.state_session_dir(session_id);
    let _state_lock = match storage::try_lock_existing_session(&state_session)? {
        storage::ExistingSessionLock::Held(lock) => Some(lock),
        storage::ExistingSessionLock::Absent => None,
        storage::ExistingSessionLock::Unavailable => return Ok(true),
    };
    let _legacy_lock = match storage::try_lock_legacy_session(source.payload_root(), session_id)? {
        storage::LegacyLock::Held(lock) => Some(lock),
        storage::LegacyLock::Absent => None,
        storage::LegacyLock::Unavailable => return Ok(true),
    };
    has_live_leases(source, session_id)
}

fn merge_session(
    source: &ControlState,
    target: &ControlState,
    session_id: &str,
) -> Result<(bool, Vec<(PathBuf, PathBuf)>), SessionTmpError> {
    if session_is_live(source, session_id)? {
        return Ok((false, Vec::new()));
    }
    // Never merge into a target session that another process currently owns;
    // a later open can retry once its external or legacy lease is gone.
    let target_session = target.payload_session_dir(session_id);
    if let Ok(metadata) = fs::symlink_metadata(&target_session) {
        if storage::file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir() {
            return Err(SessionTmpError::UnsafeManagedPath(target_session));
        }
        if session_is_live(target, session_id)? {
            return Ok((false, Vec::new()));
        }
    }
    let _source_legacy_lock = match storage::try_lock_legacy_session(source.payload_root(), session_id)? {
        storage::LegacyLock::Held(lock) => Some(lock),
        storage::LegacyLock::Absent => None,
        storage::LegacyLock::Unavailable => return Ok((false, Vec::new())),
    };
    let _target_legacy_lock = match storage::try_lock_legacy_session(target.payload_root(), session_id)? {
        storage::LegacyLock::Held(lock) => Some(lock),
        storage::LegacyLock::Absent => None,
        storage::LegacyLock::Unavailable => return Ok((false, Vec::new())),
    };
    let (source_state_lock, target_state_lock) = if source.root_id() < target.root_id() {
        (
            storage::lock_session(&source.state_session_dir(session_id))?,
            storage::lock_session(&target.state_session_dir(session_id))?,
        )
    } else {
        (
            storage::lock_session(&target.state_session_dir(session_id))?,
            storage::lock_session(&source.state_session_dir(session_id))?,
        )
    };
    if has_live_leases(source, session_id)? || has_live_leases(target, session_id)? {
        drop(source_state_lock);
        drop(target_state_lock);
        return Ok((false, Vec::new()));
    }
    let source_session = source.payload_session_dir(session_id);
    let mut moved_paths = Vec::new();
    if source_session.is_dir() {
        storage::ensure_directory_not_symlink(&source_session)?;
        let source_agents = source_session.join(AGENTS_DIR);
        let target_agents = target_session.join(AGENTS_DIR);
        storage::ensure_directory_not_symlink(&source_agents)?;
        if source_agents.is_dir() {
            storage::ensure_directory_not_symlink(&target_session)?;
            fs::create_dir_all(&target_session)?;
            storage::set_private_directory(&target_session)?;
            storage::ensure_directory_not_symlink(&target_agents)?;
            fs::create_dir_all(&target_agents)?;
            storage::set_private_directory(&target_agents)?;
            merge_agents(
                &source_agents,
                &target_agents,
                Path::new(AGENTS_DIR),
                &mut moved_paths,
            )?;
        }
    }
    merge_session_record(source, target, session_id)?;
    merge_metadata(source, target, session_id, &moved_paths)?;
    merge_stale_leases(source, target, session_id)?;
    if has_live_leases(source, session_id)? || has_live_leases(target, session_id)? {
        drop(source_state_lock);
        drop(target_state_lock);
        return Ok((false, moved_paths));
    }
    retire_source_session(source, session_id)?;
    drop(source_state_lock);
    drop(target_state_lock);
    storage::remove_session_lock(&source.state_sessions_dir(), session_id)?;
    Ok((true, moved_paths))
}

fn has_live_leases(state: &ControlState, session_id: &str) -> Result<bool, SessionTmpError> {
    Ok(storage::has_fresh_lease(
        &state.state_session_dir(session_id).join(LEASES_DIR),
        storage::LEASE_STALE_AFTER,
    )? || storage::has_fresh_lease(
        &state.legacy_session_dir(session_id).join(LEASES_DIR),
        storage::LEASE_STALE_AFTER,
    )?)
}

fn merge_agents(
    source_agents: &Path,
    target_agents: &Path,
    source_rel: &Path,
    moved_paths: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), SessionTmpError> {
    for item in fs::read_dir(source_agents)? {
        let source_path = item?.path();
        let Some(name) = source_path.file_name() else {
            continue;
        };
        let target_path = target_agents.join(name);
        let source_relative = source_rel.join(name);
        let target_relative = source_rel.join(name);
        merge_payload_path(
            &source_path,
            &target_path,
            &source_relative,
            &target_relative,
            moved_paths,
        )?;
    }
    Ok(())
}

fn merge_payload_path(
    source: &Path,
    target: &Path,
    source_relative: &Path,
    target_relative: &Path,
    moved_paths: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), SessionTmpError> {
    let source_type = fs::symlink_metadata(source)?.file_type();
    if storage::file_type_is_link(source_type) {
        let destination = if fs::symlink_metadata(target).is_ok() {
            collision_path(target)?
        } else {
            target.to_path_buf()
        };
        if let Some(parent) = destination.parent() {
            storage::ensure_directory_not_symlink(parent)?;
            fs::create_dir_all(parent)?;
        }
        fs::rename(source, &destination)?;
        let destination_relative = destination
            .file_name()
            .map_or_else(|| target_relative.to_path_buf(), |name| target_relative.with_file_name(name));
        moved_paths.push((source_relative.to_path_buf(), destination_relative));
        return Ok(());
    }
    if fs::symlink_metadata(target).is_ok() {
        let target_type = fs::symlink_metadata(target)?.file_type();
        if source_type.is_dir() && target_type.is_dir() && !storage::file_type_is_link(target_type) {
            for item in fs::read_dir(source)? {
                let child = item?.path();
                let Some(name) = child.file_name() else {
                    continue;
                };
                merge_payload_path(
                    &child,
                    &target.join(name),
                    &source_relative.join(name),
                    &target_relative.join(name),
                    moved_paths,
                )?;
            }
            if fs::read_dir(source)?.next().transpose()?.is_none() {
                fs::remove_dir(source)?;
            }
            return Ok(());
        }
        if source_type.is_file() && target_type.is_file() && files_equal(source, target)? {
            fs::remove_file(source)?;
            moved_paths.push((source_relative.to_path_buf(), target_relative.to_path_buf()));
            return Ok(());
        }
        // Never overwrite a differing payload. A unique sibling keeps both
        // paths addressable and gives metadata a stable destination mapping.
        let target = collision_path(target)?;
        fs::rename(source, &target)?;
        moved_paths.push((
            source_relative.to_path_buf(),
            target_relative.with_file_name(target.file_name().unwrap_or_default()),
        ));
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        storage::ensure_directory_not_symlink(parent)?;
        fs::create_dir_all(parent)?;
    }
    fs::rename(source, target)?;
    moved_paths.push((source_relative.to_path_buf(), target_relative.to_path_buf()));
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

fn merge_session_record(
    source: &ControlState,
    target: &ControlState,
    session_id: &str,
) -> Result<(), SessionTmpError> {
    let source_path = source.state_session_dir(session_id).join(SESSION_METADATA_FILE);
    let target_dir = target.state_session_dir(session_id);
    let target_path = target_dir.join(SESSION_METADATA_FILE);
    let source_record = match storage::read_session_record(&source_path) {
        Ok(record) => record,
        Err(_) => return Ok(()),
    };
    storage::ensure_directory_not_symlink(&target_dir)?;
    fs::create_dir_all(&target_dir)?;
    storage::set_private_directory(&target_dir)?;
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

fn merge_metadata(
    source: &ControlState,
    target: &ControlState,
    session_id: &str,
    moved_paths: &[(PathBuf, PathBuf)],
) -> Result<(), SessionTmpError> {
    let source_dir = source.state_session_dir(session_id).join(ENTRY_METADATA_DIR);
    storage::ensure_directory_not_symlink(&source_dir)?;
    if !source_dir.is_dir() {
        return Ok(());
    }
    let target_dir = target.state_session_dir(session_id).join(ENTRY_METADATA_DIR);
    storage::ensure_directory_not_symlink(&target_dir)?;
    fs::create_dir_all(&target_dir)?;
    storage::set_private_directory(&target_dir)?;
    let mut items = fs::read_dir(source_dir)?;
    for _ in 0..MAX_MIGRATION_ENTRIES {
        let Some(item) = items.next() else {
            break;
        };
        let source_path = item?.path();
        if source_path.extension().and_then(|extension| extension.to_str()) != Some("json") {
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
        if metadata.path == original_path
            && !source
                .payload_session_dir(session_id)
                .join(&original_path)
                .exists()
        {
            metadata.path = recover_moved_path(target, session_id, &original_path);
        }
        if metadata_dir_contains(&target_dir, &metadata)? {
            continue;
        }
        let mut target_path = target_dir.join(format!("{}.json", metadata.id));
        if let Ok(target_metadata) = fs::symlink_metadata(&target_path)
            && storage::file_type_is_link(target_metadata.file_type())
        {
            return Err(SessionTmpError::UnsafeManagedPath(target_path));
        }
        if fs::symlink_metadata(&target_path).is_ok() {
            if let Ok(existing) = storage::read_metadata(&target_path)
                && (existing == metadata || metadata_matches_without_id(&existing, &metadata))
            {
                continue;
            }
            metadata.id = Uuid::new_v4().simple().to_string();
            target_path = target_dir.join(format!("{}.json", metadata.id));
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

fn merge_stale_leases(
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
        if let Ok(record) = storage::read_lease_record(&source_path) {
            if record.schema_version != 1
                || record.session_id != session_id
                || storage::validate_component(&record.thread_id).is_err()
                || storage::lease_is_fresh(&source_path, storage::LEASE_STALE_AFTER)
            {
                continue;
            }
            storage::write_json_atomically(&target_path, &record)?;
        }
    }
    if items.next().transpose()?.is_some() {
        return Err(migration_limit_error("too many legacy lease entries"));
    }
    Ok(())
}

fn retire_source_session(source: &ControlState, session_id: &str) -> Result<(), SessionTmpError> {
    let payload_session = source.payload_session_dir(session_id);
    remove_known_control_files(&payload_session, session_id)?;
    let state_session = source.state_session_dir(session_id);
    remove_known_control_files(&state_session, session_id)?;
    Ok(())
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
                storage::read_metadata(&child)
                    .is_ok_and(|metadata| {
                        metadata.session_id == session_id
                            && child_name == Some(metadata.id.as_str())
                            && valid_metadata_path(&metadata)
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

fn source_root_is_retirable(source: &ControlState) -> Result<bool, SessionTmpError> {
    for session_id in collect_session_ids(source)? {
        if session_is_live(source, &session_id)? {
            return Ok(false);
        }
    }
    root_has_only_managed_entries(source.payload_root(), true)
        && root_has_only_managed_entries(source.state_root(), false)
}

fn retire_source(source: &ControlState) -> Result<(), SessionTmpError> {
    if root_has_only_managed_entries(source.payload_root(), true) {
        storage::remove_path(source.payload_root())?;
    }
    if root_has_only_managed_entries(source.state_root(), false) {
        storage::remove_path(source.state_root())?;
    }
    Ok(())
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
        if payload && name == SESSIONS_DIR {
            if !directory_empty(&path) {
                return false;
            }
        }
        if !payload && (name == state::STATE_SESSIONS_DIR || name == state::STATE_LOCKS_DIR) {
            if !directory_empty(&path) {
                return false;
            }
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

fn lock_migration(state: &ControlState) -> Result<File, SessionTmpError> {
    let path = state.state_root.join(MIGRATION_LOCK_FILE);
    storage::ensure_directory_not_symlink(state.state_root())?;
    if fs::symlink_metadata(&path)
        .map(|metadata| storage::file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(path));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)?;
    storage::set_private_file(&path)?;
    file.lock()?;
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

fn collision_path(path: &Path) -> Result<PathBuf, SessionTmpError> {
    let name = path
        .file_name()
        .ok_or_else(|| SessionTmpError::PathOutsideAgent(path.to_path_buf()))?
        .to_string_lossy();
    for _ in 0..8 {
        let candidate = path.with_file_name(format!("{name}-migrated-{}", Uuid::new_v4().simple()));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(SessionTmpError::Io(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "unable to allocate a collision-safe migration path",
    )))
}

fn valid_metadata_path(metadata: &EntryMetadata) -> bool {
    let mut components = metadata.path.components();
    components.next().is_some_and(|component| component.as_os_str() == AGENTS_DIR)
        && components
            .next()
            .is_some_and(|component| component.as_os_str() == metadata.thread_id.as_str())
        && components
            .next()
            .is_some_and(|component| matches!(component, std::path::Component::Normal(_)))
        && components.all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn map_relative_path(path: &Path, moved_paths: &[(PathBuf, PathBuf)]) -> PathBuf {
    moved_paths
        .iter()
        .filter(|(source, _)| path == source || path.starts_with(source))
        .max_by_key(|(source, _)| source.components().count())
        .map(|(source, target)| {
            target.join(path.strip_prefix(source).unwrap_or_else(|_| Path::new("")))
        })
        .unwrap_or_else(|| path.to_path_buf())
}

fn recover_moved_path(target: &ControlState, session_id: &str, path: &Path) -> PathBuf {
    let mut components = path.components();
    if components.next().is_none_or(|component| component.as_os_str() != AGENTS_DIR) {
        return path.to_path_buf();
    }
    let Some(std::path::Component::Normal(thread)) = components.next() else {
        return path.to_path_buf();
    };
    let components = components.collect::<Vec<_>>();
    let Some(std::path::Component::Normal(name)) = components.last() else {
        return path.to_path_buf();
    };
    if components
        .iter()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return path.to_path_buf();
    }
    let parent_relative = Path::new(AGENTS_DIR)
        .join(thread)
        .join(components.iter().take(components.len().saturating_sub(1)).fold(
            PathBuf::new(),
            |mut parent, component| {
                parent.push(component.as_os_str());
                parent
            },
        ));
    let parent_dir = target
        .payload_session_dir(session_id)
        .join(&parent_relative);
    let prefix = format!("{}-migrated-", name.to_string_lossy());
    let mut candidates = fs::read_dir(parent_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let candidate = entry.path();
            let candidate_name = candidate.file_name()?.to_str()?;
            candidate_name
                .starts_with(&prefix)
                .then_some(candidate_name.to_string())
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates
        .first()
        .map(|candidate| parent_relative.join(candidate))
        .unwrap_or_else(|| path.to_path_buf())
}
