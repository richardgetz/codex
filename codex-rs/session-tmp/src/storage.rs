use super::AGENTS_DIR;
use super::CleanupReport;
use super::EntryMetadata;
use super::MANAGED_ROOT_MARKER;
use super::MANAGED_ROOT_MARKER_CONTENT;
use super::SESSION_METADATA_FILE;
use super::SESSIONS_DIR;
use super::SessionTmpError;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::ErrorKind;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use uuid::Uuid;

pub(super) const LEASES_DIR: &str = "leases";
const SESSION_LOCKS_DIR: &str = ".locks";
pub(super) const LEASE_STALE_AFTER: Duration = Duration::from_secs(90);

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct SessionRecord {
    pub(super) schema_version: u8,
    pub(super) session_id: String,
    pub(super) created_at: u64,
    pub(super) updated_at: u64,
    pub(super) status: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct LeaseRecord {
    pub(super) schema_version: u8,
    pub(super) session_id: String,
    pub(super) thread_id: String,
    pub(super) process_id: u32,
    pub(super) updated_at: u64,
}

pub(super) fn ensure_managed_root(root: &Path) -> Result<(), SessionTmpError> {
    fs::create_dir_all(root)?;
    ensure_directory_not_symlink(root)?;
    set_private_directory(root)?;
    let marker = root.join(MANAGED_ROOT_MARKER);
    if fs::symlink_metadata(&marker)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(marker));
    }
    if marker.exists() {
        let content = fs::read_to_string(&marker)?;
        if content != MANAGED_ROOT_MARKER_CONTENT {
            return Err(SessionTmpError::RootNotManaged(root.to_path_buf()));
        }
    } else {
        let mut entries = fs::read_dir(root)?;
        if entries.next().transpose()?.is_some() {
            return Err(SessionTmpError::RootNotManaged(root.to_path_buf()));
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(MANAGED_ROOT_MARKER_CONTENT.as_bytes())?;
                file.sync_all()?;
                set_private_file(&marker)?;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let content = fs::read_to_string(&marker)?;
                if content != MANAGED_ROOT_MARKER_CONTENT {
                    return Err(SessionTmpError::RootNotManaged(root.to_path_buf()));
                }
                set_private_file(&marker)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let sessions_dir = root.join(SESSIONS_DIR);
    ensure_directory_not_symlink(&sessions_dir)?;
    fs::create_dir_all(&sessions_dir)?;
    set_private_directory(&sessions_dir)?;
    Ok(())
}

pub(super) fn reap_stale_sessions(
    root: &Path,
    max_age: std::time::Duration,
    excluded_session_id: Option<&str>,
) -> Result<CleanupReport, SessionTmpError> {
    let sessions_dir = root.join(SESSIONS_DIR);
    ensure_directory_not_symlink(&sessions_dir)?;
    let now = now_seconds();
    let max_age = max_age.as_secs();
    let mut report = CleanupReport::default();
    if max_age == 0 || !sessions_dir.is_dir() {
        return Ok(report);
    }
    for item in fs::read_dir(sessions_dir)? {
        let session_dir = item?.path();
        let is_real_directory = fs::symlink_metadata(&session_dir)
            .map(|metadata| {
                metadata.file_type().is_dir() && !file_type_is_link(metadata.file_type())
            })
            .unwrap_or(false);
        if !is_real_directory
            || excluded_session_id.is_some_and(|session_id| {
                session_dir.file_name().and_then(|name| name.to_str()) == Some(session_id)
            })
        {
            continue;
        }
        let Some(directory_session_id) = session_dir.file_name().and_then(|name| name.to_str())
        else {
            continue;
        };
        let record_path = session_dir.join(super::SESSION_METADATA_FILE);
        let Ok(record) = read_session_record(&record_path) else {
            continue;
        };
        if record.schema_version != 1
            || record.session_id != directory_session_id
            || now.saturating_sub(record.updated_at) < max_age
        {
            continue;
        }
        let Some(_session_lock) = (match try_lock_session(&session_dir) {
            Ok(lock) => lock,
            Err(error) if is_skippable_reap_error(&error) => {
                tracing::debug!(
                    error = %error,
                    session_dir = %session_dir.display(),
                    "skipping stale session with inaccessible cleanup state"
                );
                continue;
            }
            Err(error) => return Err(error),
        }) else {
            continue;
        };
        let Ok(record) = read_session_record(&record_path) else {
            continue;
        };
        let fresh_lease = match has_fresh_lease(&session_dir.join(LEASES_DIR), LEASE_STALE_AFTER) {
            Ok(fresh_lease) => fresh_lease,
            Err(error) if is_skippable_reap_error(&error) => {
                tracing::debug!(
                    error = %error,
                    session_dir = %session_dir.display(),
                    "skipping stale session with inaccessible lease state"
                );
                continue;
            }
            Err(error) => return Err(error),
        };
        if record.schema_version != 1
            || record.session_id != directory_session_id
            || now.saturating_sub(record.updated_at) < max_age
            || fresh_lease
        {
            continue;
        }
        match remove_path(&session_dir) {
            Ok(true) => report.removed_sessions += 1,
            Ok(false) => {}
            Err(error) if is_skippable_reap_error(&error) => {
                tracing::debug!(
                    error = %error,
                    session_dir = %session_dir.display(),
                    "skipping stale session that could not be removed"
                );
            }
            Err(error) => return Err(error),
        }
    }
    Ok(report)
}

fn is_skippable_reap_error(error: &SessionTmpError) -> bool {
    matches!(error, SessionTmpError::Io(_))
}

pub(super) fn resolve_user_session_id(
    root: &Path,
    candidate_session_id: &str,
    thread_id: &str,
) -> Result<String, SessionTmpError> {
    let sessions_dir = root.join(SESSIONS_DIR);
    ensure_directory_not_symlink(&sessions_dir)?;
    let candidate_dir = sessions_dir.join(candidate_session_id);
    ensure_directory_not_symlink(&candidate_dir)?;
    if candidate_dir.is_dir() {
        return Ok(candidate_session_id.to_string());
    }

    for item in fs::read_dir(&sessions_dir)? {
        let session_dir = item?.path();
        if !fs::symlink_metadata(&session_dir)
            .map(|metadata| {
                metadata.file_type().is_dir() && !file_type_is_link(metadata.file_type())
            })
            .unwrap_or(false)
        {
            continue;
        }
        let Some(session_id) = session_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if validate_component(session_id).is_err() {
            continue;
        }
        let Ok(record) = read_session_record(&session_dir.join(SESSION_METADATA_FILE)) else {
            continue;
        };
        if record.session_id != session_id {
            continue;
        }
        let agent_dir = session_dir.join(AGENTS_DIR).join(thread_id);
        if fs::symlink_metadata(&agent_dir)
            .map(|metadata| {
                metadata.file_type().is_dir() && !file_type_is_link(metadata.file_type())
            })
            .unwrap_or(false)
        {
            return Ok(session_id.to_string());
        }
    }

    Ok(candidate_session_id.to_string())
}

pub(super) fn validate_component(value: &str) -> Result<(), SessionTmpError> {
    let mut components = Path::new(value).components();
    let is_single_normal_component = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(component)), None) if component == OsStr::new(value)
    );
    if !is_single_normal_component {
        return Err(SessionTmpError::InvalidComponent(value.to_string()));
    }
    Ok(())
}

pub(super) fn validate_name(value: &str) -> Result<String, SessionTmpError> {
    validate_component(value)?;
    Ok(value.to_string())
}

pub(super) fn read_metadata(path: &Path) -> Result<EntryMetadata, SessionTmpError> {
    if fs::symlink_metadata(path)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|source| SessionTmpError::InvalidMetadata {
        path: path.to_path_buf(),
        source,
    })
}

pub(super) fn read_session_record(path: &Path) -> Result<SessionRecord, SessionTmpError> {
    if fs::symlink_metadata(path)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) fn lease_is_fresh(path: &Path, max_age: Duration) -> bool {
    let Some(updated_at) = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<LeaseRecord>(&bytes).ok())
        .map(|record| record.updated_at)
        .or_else(|| {
            fs::metadata(path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
        })
    else {
        return false;
    };
    now_seconds().saturating_sub(updated_at) < max_age.as_secs()
}

fn has_fresh_lease(leases_dir: &Path, max_age: Duration) -> Result<bool, SessionTmpError> {
    ensure_directory_not_symlink(leases_dir)?;
    if !leases_dir.is_dir() {
        return Ok(false);
    }
    for item in fs::read_dir(leases_dir)? {
        let path = item?.path();
        if fs::symlink_metadata(&path)
            .map(|metadata| file_type_is_link(metadata.file_type()))
            .unwrap_or(false)
        {
            return Err(SessionTmpError::UnsafeManagedPath(path));
        }
        if lease_is_fresh(&path, max_age) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn lease_is_fresh_for_thread(session_dir: &Path, thread_id: &str) -> bool {
    let path = session_dir
        .join(LEASES_DIR)
        .join(format!("{thread_id}.json"));
    if fs::symlink_metadata(&path)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        // A tampered lease must not make the root session delete a live agent
        // directory. The stricter stale-session reaper rejects this case.
        return true;
    }
    lease_is_fresh(&path, LEASE_STALE_AFTER)
}

fn open_session_lock(session_dir: &Path) -> Result<File, SessionTmpError> {
    ensure_directory_not_symlink(session_dir)?;
    let sessions_dir = session_dir
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "session path has no parent"))?;
    let locks_dir = sessions_dir.join(SESSION_LOCKS_DIR);
    ensure_directory_not_symlink(&locks_dir)?;
    fs::create_dir_all(&locks_dir)?;
    set_private_directory(&locks_dir)?;
    let session_id = session_dir
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "session path has no name"))?;
    let lock_path = locks_dir.join(format!("{}.lock", session_id.to_string_lossy()));
    if fs::symlink_metadata(&lock_path)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(lock_path));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)?;
    set_private_file(&lock_path)?;
    Ok(file)
}

pub(super) fn lock_session(session_dir: &Path) -> Result<File, SessionTmpError> {
    let file = open_session_lock(session_dir)?;
    file.lock()?;
    Ok(file)
}

pub(super) fn try_lock_session(session_dir: &Path) -> Result<Option<File>, SessionTmpError> {
    let file = open_session_lock(session_dir)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

pub(super) fn write_json_atomically<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), SessionTmpError> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "metadata path has no parent directory",
        )
    })?;
    ensure_directory_not_symlink(parent)?;
    fs::create_dir_all(parent)?;
    ensure_directory_not_symlink(parent)?;
    if fs::symlink_metadata(path)
        .map(|metadata| file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "metadata path has no file name"))?;
    let file_name = file_name.to_string_lossy();
    let suffix = Uuid::new_v4().simple();
    let temporary = parent.join(format!(".{file_name}.{suffix}.tmp"));
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(&temporary, bytes)?;
    set_private_file(&temporary)?;
    match fs::rename(&temporary, path) {
        Ok(()) => {
            set_private_file(path)?;
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            fs::remove_file(path)?;
            fs::rename(temporary, path)?;
            set_private_file(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn ensure_directory_not_symlink(path: &Path) -> Result<(), SessionTmpError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if file_type_is_link(metadata.file_type()) => {
            Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()))
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn collect_paths(
    directory: &Path,
    tracked_paths: &[PathBuf],
    output: &mut Vec<PathBuf>,
    limit: usize,
) -> Result<(), SessionTmpError> {
    ensure_directory_not_symlink(directory)?;
    if output.len() >= limit || !directory.is_dir() {
        return Ok(());
    }
    for item in fs::read_dir(directory)? {
        let path = item?.path();
        let file_type = fs::symlink_metadata(&path)?.file_type();
        if tracked_paths
            .iter()
            .any(|tracked| path == *tracked || path.starts_with(tracked))
        {
            continue;
        }
        if file_type.is_dir() && !file_type_is_link(file_type) {
            collect_paths(&path, tracked_paths, output, limit)?;
        } else {
            output.push(path);
        }
        if output.len() >= limit {
            break;
        }
    }
    Ok(())
}

pub(super) fn remove_untracked_paths(
    directory: &Path,
    preserved_paths: &HashSet<PathBuf>,
    preserved_directories: &HashSet<PathBuf>,
    protected_directories: &HashSet<PathBuf>,
    report: &mut CleanupReport,
) -> Result<(), SessionTmpError> {
    ensure_directory_not_symlink(directory)?;
    if !directory.is_dir() {
        return Ok(());
    }
    for item in fs::read_dir(directory)? {
        let path = item?.path();
        let file_type = fs::symlink_metadata(&path)?.file_type();
        if preserved_directories.contains(&path) {
            continue;
        }
        let is_preserved = preserved_paths
            .iter()
            .any(|preserved| *preserved == path || preserved.starts_with(&path));
        let is_preserved_root = preserved_paths.iter().any(|preserved| *preserved == path);
        let is_protected_directory = protected_directories.contains(&path)
            && file_type.is_dir()
            && !file_type_is_link(file_type);
        let should_recurse = (is_preserved
            && !is_preserved_root
            && file_type.is_dir()
            && !file_type_is_link(file_type))
            || is_protected_directory;
        if should_recurse {
            remove_untracked_paths(
                &path,
                preserved_paths,
                preserved_directories,
                protected_directories,
                report,
            )?;
        } else if !is_preserved && remove_path(&path)? {
            report.removed_paths += 1;
        }
    }
    Ok(())
}

pub(super) fn remove_path(path: &Path) -> Result<bool, SessionTmpError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if file_type_is_link(metadata.file_type()) {
        fs::remove_file(path)?;
        Ok(true)
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)?;
        Ok(true)
    } else {
        fs::remove_file(path)?;
        Ok(true)
    }
}

pub(super) fn file_type_is_link(file_type: fs::FileType) -> bool {
    if file_type.is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        return file_type.is_symlink_dir() || file_type.is_symlink_file();
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub(super) fn set_private_directory(path: &Path) -> Result<(), SessionTmpError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path)?;
        if file_type_is_link(metadata.file_type()) {
            return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

pub(super) fn set_private_file(path: &Path) -> Result<(), SessionTmpError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path)?;
        if file_type_is_link(metadata.file_type()) {
            return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

pub(super) fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
