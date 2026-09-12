//! Payload and session merge operations for recovery migration.

use super::ControlState;
use super::Path;
use super::PathBuf;
use super::SessionTmpError;
use super::liveness::has_live_leases;
use super::liveness::session_is_live;
use super::records::collision_path;
use super::records::merge_metadata;
use super::records::merge_session_record;
use super::records::merge_stale_leases;
use super::retire::retire_source_session;
use super::storage;
use crate::AGENTS_DIR;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;

pub(super) struct SessionMerge {
    pub(super) merged: bool,
    pub(super) moved_paths: Vec<(PathBuf, PathBuf)>,
    source_state_lock: Option<File>,
    target_state_lock: Option<File>,
    source_legacy_lock: Option<File>,
    target_legacy_lock: Option<File>,
}

impl SessionMerge {
    fn deferred(moved_paths: Vec<(PathBuf, PathBuf)>) -> Self {
        Self {
            merged: false,
            moved_paths,
            source_state_lock: None,
            target_state_lock: None,
            source_legacy_lock: None,
            target_legacy_lock: None,
        }
    }

    /// Retire source controls after the caller has durably recorded the
    /// payload moves in the migration manifest.  The source and target locks
    /// remain held across that commit so a source writer cannot race the
    /// exact-control cleanup.
    pub(super) fn retire_source(
        mut self,
        source: &ControlState,
        target: &ControlState,
        session_id: &str,
    ) -> Result<(), SessionTmpError> {
        retire_source_session(source, target, session_id, &self.moved_paths)?;
        self.source_legacy_lock.take();
        self.target_legacy_lock.take();
        self.target_state_lock.take();
        self.source_state_lock.take();
        Ok(())
    }
}

pub(super) fn merge_session(
    source: &ControlState,
    target: &ControlState,
    session_id: &str,
    persisted_moved_paths: &[(PathBuf, PathBuf)],
    persist_moves: &mut dyn FnMut(&[(PathBuf, PathBuf)]) -> Result<(), SessionTmpError>,
) -> Result<SessionMerge, SessionTmpError> {
    if session_is_live(source, session_id)? {
        return Ok(SessionMerge::deferred(Vec::new()));
    }
    // Never merge into a target session that another process currently owns;
    // a later open can retry once its external or legacy lease is gone.
    let target_session = target.payload_session_dir(session_id);
    if let Ok(metadata) = fs::symlink_metadata(&target_session)
        && (storage::file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir())
    {
        return Err(SessionTmpError::UnsafeManagedPath(target_session));
    }
    if session_is_live(target, session_id)? {
        return Ok(SessionMerge::deferred(Vec::new()));
    }
    // Acquire external state locks before probing the legacy lock domain. New
    // managers take this same order; a migration never holds a legacy lock
    // while waiting on a blocking external lock.
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
    let source_legacy_lock = if source.legacy_transition_active()? {
        match storage::try_lock_legacy_session(source.payload_root(), session_id)? {
            storage::LegacyLock::Held(lock) => Some(lock),
            storage::LegacyLock::Absent => None,
            storage::LegacyLock::Unavailable => {
                drop(source_state_lock);
                drop(target_state_lock);
                return Ok(SessionMerge::deferred(Vec::new()));
            }
        }
    } else {
        None
    };
    let target_legacy_lock = if target.legacy_transition_active()? {
        match storage::try_lock_legacy_session(target.payload_root(), session_id)? {
            storage::LegacyLock::Held(lock) => Some(lock),
            storage::LegacyLock::Absent => None,
            storage::LegacyLock::Unavailable => {
                drop(source_state_lock);
                drop(target_state_lock);
                return Ok(SessionMerge::deferred(Vec::new()));
            }
        }
    } else {
        None
    };
    if has_live_leases(source, session_id)? || has_live_leases(target, session_id)? {
        drop(source_state_lock);
        drop(target_state_lock);
        return Ok(SessionMerge::deferred(Vec::new()));
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
                &target_session,
                Path::new(AGENTS_DIR),
                persisted_moved_paths,
                persist_moves,
                &mut moved_paths,
            )?;
        }
    }
    merge_session_record(source, target, session_id)?;
    let mut metadata_moved_paths = persisted_moved_paths.to_vec();
    metadata_moved_paths.extend(moved_paths.iter().cloned());
    merge_metadata(source, target, session_id, &metadata_moved_paths)?;
    merge_stale_leases(source, target, session_id)?;
    if has_live_leases(source, session_id)? || has_live_leases(target, session_id)? {
        drop(source_state_lock);
        drop(target_state_lock);
        return Ok(SessionMerge::deferred(moved_paths));
    }
    Ok(SessionMerge {
        merged: true,
        moved_paths,
        source_state_lock: Some(source_state_lock),
        target_state_lock: Some(target_state_lock),
        source_legacy_lock,
        target_legacy_lock,
    })
}

fn merge_agents(
    source_agents: &Path,
    target_agents: &Path,
    target_session: &Path,
    source_rel: &Path,
    persisted_moved_paths: &[(PathBuf, PathBuf)],
    persist_moves: &mut dyn FnMut(&[(PathBuf, PathBuf)]) -> Result<(), SessionTmpError>,
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
            target_session,
            persisted_moved_paths,
            persist_moves,
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
    target_session: &Path,
    persisted_moved_paths: &[(PathBuf, PathBuf)],
    persist_moves: &mut dyn FnMut(&[(PathBuf, PathBuf)]) -> Result<(), SessionTmpError>,
    moved_paths: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), SessionTmpError> {
    let source_type = fs::symlink_metadata(source)?.file_type();
    let mapped_target = mapped_payload_target(
        target_session,
        source_relative,
        persisted_moved_paths,
        moved_paths,
    );
    if storage::file_type_is_link(source_type) {
        let target = mapped_target.as_deref().unwrap_or(target);
        if let Ok(target_type) = fs::symlink_metadata(target).map(|metadata| metadata.file_type())
            && storage::file_type_is_link(target_type)
            && fs::read_link(source)? == fs::read_link(target)?
        {
            record_move(
                source_relative,
                &destination_relative(target_session, target, target_relative),
                persisted_moved_paths,
                persist_moves,
                moved_paths,
            )?;
            return Ok(());
        }
        let destination = if fs::symlink_metadata(target).is_ok() {
            if mapped_target.is_some() {
                target.to_path_buf()
            } else {
                collision_path(source, target)?
            }
        } else {
            target.to_path_buf()
        };
        move_payload_path_no_replace(
            source,
            &destination,
            source_relative,
            target_relative,
            target_session,
            persisted_moved_paths,
            persist_moves,
            moved_paths,
        )?;
        return Ok(());
    }
    let target = mapped_target.as_deref().unwrap_or(target);
    if fs::symlink_metadata(target).is_ok() {
        let target_type = fs::symlink_metadata(target)?.file_type();
        if source_type.is_dir() && target_type.is_dir() && !storage::file_type_is_link(target_type)
        {
            merge_existing_directory(
                source,
                target,
                source_relative,
                target_relative,
                target_session,
                persisted_moved_paths,
                persist_moves,
                moved_paths,
            )?;
            return Ok(());
        }
        if source_type.is_file() && target_type.is_file() && files_equal(source, target)? {
            record_move(
                source_relative,
                &destination_relative(target_session, target, target_relative),
                persisted_moved_paths,
                persist_moves,
                moved_paths,
            )?;
            return Ok(());
        }
        // Never overwrite a differing payload. A unique sibling keeps both
        // paths addressable and gives metadata a stable destination mapping.
        let target = if mapped_target.is_some() {
            target.to_path_buf()
        } else {
            collision_path(source, target)?
        };
        move_payload_path_no_replace(
            source,
            &target,
            source_relative,
            target_relative,
            target_session,
            persisted_moved_paths,
            persist_moves,
            moved_paths,
        )?;
        return Ok(());
    }
    move_payload_path_no_replace(
        source,
        target,
        source_relative,
        target_relative,
        target_session,
        persisted_moved_paths,
        persist_moves,
        moved_paths,
    )
}

/// Copy one payload entry without allowing a concurrent writer to be
/// overwritten. The source remains until the caller has durably recorded the
/// mapping in the migration manifest. `rename` is intentionally avoided here:
/// on Unix it replaces a destination that appears after an existence check.
/// Files use `create_new`, directories use `create_dir`, and symlinks use an
/// atomic create operation; each retries with a fresh collision path on
/// `EEXIST`.
fn move_payload_path_no_replace(
    source: &Path,
    target: &Path,
    source_relative: &Path,
    target_relative: &Path,
    target_session: &Path,
    persisted_moved_paths: &[(PathBuf, PathBuf)],
    persist_moves: &mut dyn FnMut(&[(PathBuf, PathBuf)]) -> Result<(), SessionTmpError>,
    moved_paths: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), SessionTmpError> {
    let source_type = fs::symlink_metadata(source)?.file_type();
    let mut destination = target.to_path_buf();
    for _ in 0..8 {
        if let Ok(target_type) =
            fs::symlink_metadata(&destination).map(|metadata| metadata.file_type())
        {
            if storage::file_type_is_link(source_type)
                && storage::file_type_is_link(target_type)
                && fs::read_link(source)? == fs::read_link(&destination)?
            {
                record_move(
                    source_relative,
                    &destination_relative(target_session, &destination, target_relative),
                    persisted_moved_paths,
                    persist_moves,
                    moved_paths,
                )?;
                return Ok(());
            }
            if source_type.is_dir()
                && target_type.is_dir()
                && !storage::file_type_is_link(target_type)
            {
                merge_existing_directory(
                    source,
                    &destination,
                    source_relative,
                    target_relative,
                    target_session,
                    persisted_moved_paths,
                    persist_moves,
                    moved_paths,
                )?;
                return Ok(());
            }
            if source_type.is_file() && target_type.is_file() && files_equal(source, &destination)?
            {
                record_move(
                    source_relative,
                    &destination_relative(target_session, &destination, target_relative),
                    persisted_moved_paths,
                    persist_moves,
                    moved_paths,
                )?;
                return Ok(());
            }
            destination = collision_path(source, &destination)?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            storage::ensure_directory_not_symlink(parent)?;
            fs::create_dir_all(parent)?;
        }
        if storage::file_type_is_link(source_type) {
            match create_payload_symlink(source, &destination) {
                Ok(()) => {
                    record_move(
                        source_relative,
                        &destination_relative(target_session, &destination, target_relative),
                        persisted_moved_paths,
                        persist_moves,
                        moved_paths,
                    )?;
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    destination = collision_path(source, target)?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        } else if source_type.is_dir() {
            match fs::create_dir(&destination) {
                Ok(()) => {
                    storage::set_private_directory(&destination)?;
                    merge_existing_directory(
                        source,
                        &destination,
                        source_relative,
                        target_relative,
                        target_session,
                        persisted_moved_paths,
                        persist_moves,
                        moved_paths,
                    )?;
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    destination = collision_path(source, target)?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
            {
                Ok(mut target_file) => {
                    let mut source_file = File::open(source)?;
                    std::io::copy(&mut source_file, &mut target_file)?;
                    target_file.sync_all()?;
                    drop(target_file);
                    storage::set_private_file(&destination)?;
                    record_move(
                        source_relative,
                        &destination_relative(target_session, &destination, target_relative),
                        persisted_moved_paths,
                        persist_moves,
                        moved_paths,
                    )?;
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    destination = collision_path(source, target)?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Err(SessionTmpError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "unable to move migration payload without replacing a concurrent file",
    )))
}

fn merge_existing_directory(
    source: &Path,
    target: &Path,
    source_relative: &Path,
    target_relative: &Path,
    target_session: &Path,
    persisted_moved_paths: &[(PathBuf, PathBuf)],
    persist_moves: &mut dyn FnMut(&[(PathBuf, PathBuf)]) -> Result<(), SessionTmpError>,
    moved_paths: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), SessionTmpError> {
    let target_relative = destination_relative(target_session, target, target_relative);
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
            target_session,
            persisted_moved_paths,
            persist_moves,
            moved_paths,
        )?;
    }
    if fs::read_dir(source)?.next().transpose()?.is_none() {
        record_move(
            source_relative,
            &target_relative,
            persisted_moved_paths,
            persist_moves,
            moved_paths,
        )?;
    }
    Ok(())
}

fn mapped_payload_target(
    target_session: &Path,
    source_relative: &Path,
    persisted_moved_paths: &[(PathBuf, PathBuf)],
    moved_paths: &[(PathBuf, PathBuf)],
) -> Option<PathBuf> {
    let mapped = super::records::map_relative_path(source_relative, persisted_moved_paths);
    let mapped = if mapped == source_relative {
        super::records::map_relative_path(source_relative, moved_paths)
    } else {
        mapped
    };
    (mapped != source_relative).then(|| target_session.join(mapped))
}

fn record_move(
    source: &Path,
    target: &Path,
    persisted_moved_paths: &[(PathBuf, PathBuf)],
    persist_moves: &mut dyn FnMut(&[(PathBuf, PathBuf)]) -> Result<(), SessionTmpError>,
    moved_paths: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), SessionTmpError> {
    if moved_paths
        .iter()
        .any(|(existing_source, existing_target)| {
            existing_source == source && existing_target == target
        })
    {
        return Ok(());
    }
    moved_paths.push((source.to_path_buf(), target.to_path_buf()));
    if !persisted_moved_paths
        .iter()
        .any(|(existing_source, existing_target)| {
            existing_source == source && existing_target == target
        })
    {
        let mut all_paths = persisted_moved_paths.to_vec();
        all_paths.extend(moved_paths.iter().cloned());
        persist_moves(&all_paths)?;
    }
    Ok(())
}

fn destination_relative(target_session: &Path, destination: &Path, fallback: &Path) -> PathBuf {
    destination
        .strip_prefix(target_session)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| fallback.to_path_buf())
}

fn create_payload_symlink(source: &Path, destination: &Path) -> std::io::Result<()> {
    let target = fs::read_link(source)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, destination)
    }
    #[cfg(windows)]
    {
        if fs::metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
            std::os::windows::fs::symlink_dir(target, destination)
        } else {
            std::os::windows::fs::symlink_file(target, destination)
        }
    }
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
