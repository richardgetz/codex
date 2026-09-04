//! Coordinates maintenance jobs that replace local rollout files.
//!
//! Rollout compression and legacy rollout migration both publish by renaming a replacement over an
//! existing rollout path. They must not do that at the same time for one Codex home, so they share
//! this process-scoped, nonblocking file lock.
//!
//! This is separate from per-thread writer locks, which protect live rollout appenders. It is also
//! separate from compression's durable run marker, which throttles how often compression scans.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;

const ROLLOUT_MAINTENANCE_LOCK: &str = "rollout-maintenance.lock";

/// Holds exclusive ownership of operations that replace local rollout files.
pub struct RolloutMaintenanceGuard {
    _file: File,
}

/// Holds shared ownership while a caller reads rollout files that maintenance may replace.
#[derive(Debug)]
pub struct RolloutMaintenanceReadGuard {
    _file: File,
}

/// Try to exclude rollout compression and migration for one Codex home.
pub fn try_acquire_rollout_maintenance_lock(
    codex_home: &Path,
) -> io::Result<Option<RolloutMaintenanceGuard>> {
    let file = open_maintenance_lock_file(codex_home)?;

    match file.try_lock() {
        Ok(()) => Ok(Some(RolloutMaintenanceGuard { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Try to share rollout maintenance ownership for one Codex home.
pub fn try_acquire_rollout_maintenance_read_lock(
    codex_home: &Path,
) -> io::Result<Option<RolloutMaintenanceReadGuard>> {
    let file = open_maintenance_lock_file(codex_home)?;

    match file.try_lock_shared() {
        Ok(()) => Ok(Some(RolloutMaintenanceReadGuard { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

fn open_maintenance_lock_file(codex_home: &Path) -> io::Result<File> {
    let directory = codex_home.join(".tmp");
    fs::create_dir_all(&directory)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(ROLLOUT_MAINTENANCE_LOCK))
}
