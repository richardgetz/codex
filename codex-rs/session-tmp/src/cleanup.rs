use super::AGENTS_DIR;
use super::ENTRY_METADATA_DIR;
use super::LEASES_DIR;
use super::MANAGED_ROOT_MARKER;
use super::MANAGED_ROOT_MARKER_CONTENT;
use super::SESSION_METADATA_FILE;
use super::SESSIONS_DIR;
use super::SessionTmpManager;
use super::storage;
use super::storage::file_type_is_link;
use super::storage::now_seconds;
use super::storage::read_metadata;
use super::storage::read_session_record;
use super::storage::reap_stale_sessions;
use super::storage::remove_path;
use super::storage::remove_untracked_paths;
use super::storage::set_private_directory;
use super::storage::write_json_atomically;
use super::types::CleanupReport;
use super::types::EntryMetadata;
use super::types::SessionTmpError;
use super::types::TempEntry;
use std::collections::HashSet;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tracing::debug;

impl SessionTmpManager {
    /// Removes session-retained and expired paths, while leaving manual paths
    /// and live child-agent paths in place. Unregistered files are treated as
    /// session-retained files.
    pub fn clean(&self) -> Result<CleanupReport, SessionTmpError> {
        if !self.is_root_session {
            return Err(SessionTmpError::CleanupNotOwned);
        }
        self.clean_paths()
    }

    /// Explicitly clears every path in the current session, including manual
    /// retention entries. The session directory is recreated for continued use.
    pub fn clear(&self) -> Result<CleanupReport, SessionTmpError> {
        if !self.is_root_session {
            return Err(SessionTmpError::CleanupNotOwned);
        }
        self.ensure_session_layout()?;
        let mut report = CleanupReport::default();
        let metadata_dir = self.session_dir.join(ENTRY_METADATA_DIR);
        super::storage::ensure_directory_not_symlink(&metadata_dir)?;
        if metadata_dir.is_dir() {
            for item in fs::read_dir(&metadata_dir)? {
                let metadata_path = item?.path();
                if metadata_path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    != Some("json")
                {
                    continue;
                }
                let metadata = read_metadata(&metadata_path)?;
                let absolute_path = self.entry_path(&metadata)?;
                if remove_path(&absolute_path)? {
                    report.removed_paths += 1;
                }
                fs::remove_file(metadata_path)?;
            }
        }
        let protected_directories = self.agent_directories()?;
        remove_untracked_paths(
            &self.session_dir.join(AGENTS_DIR),
            &HashSet::new(),
            &HashSet::new(),
            &protected_directories,
            &mut report,
        )?;
        fs::create_dir_all(&self.agent_dir)?;
        set_private_directory(&self.agent_dir)?;
        self.write_session_record("active")?;
        Ok(report)
    }

    /// Force-cleans other session directories whose heartbeat is older than
    /// `max_age`. Only a root session may invoke this operation.
    pub fn reap(&self, max_age: Duration) -> Result<CleanupReport, SessionTmpError> {
        if !self.is_root_session {
            return Err(SessionTmpError::CleanupNotOwned);
        }
        self.ensure_root_identity()?;
        reap_stale_sessions(&self.root, max_age, Some(&self.session_id))
    }

    fn clean_paths(&self) -> Result<CleanupReport, SessionTmpError> {
        self.ensure_session_layout()?;
        let metadata_dir = self.session_dir.join(ENTRY_METADATA_DIR);
        super::storage::ensure_directory_not_symlink(&metadata_dir)?;
        super::storage::ensure_directory_not_symlink(&self.session_dir.join(AGENTS_DIR))?;
        let mut report = CleanupReport::default();
        let mut preserved_paths = HashSet::new();
        let active_agent_threads = self.active_agent_threads()?;
        if metadata_dir.is_dir() {
            for item in fs::read_dir(&metadata_dir)? {
                let metadata_path = item?.path();
                if metadata_path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    != Some("json")
                {
                    continue;
                }
                let metadata = read_metadata(&metadata_path)?;
                let absolute_path = self.entry_path(&metadata)?;
                if active_agent_threads.contains(&metadata.thread_id)
                    && metadata.thread_id != self.thread_id
                {
                    report.preserved_paths += 1;
                    preserved_paths.insert(absolute_path);
                    continue;
                }
                if metadata
                    .retention
                    .eligible_for_cleanup(now_seconds(), metadata.created_at)
                {
                    if remove_path(&absolute_path)? {
                        report.removed_paths += 1;
                    }
                    fs::remove_file(metadata_path)?;
                } else {
                    report.preserved_paths += 1;
                    preserved_paths.insert(absolute_path);
                }
            }
        }
        let protected_directories = self.agent_directories()?;
        let preserved_directories = active_agent_threads
            .iter()
            .map(|thread_id| self.session_dir.join(AGENTS_DIR).join(thread_id))
            .collect::<HashSet<_>>();
        remove_untracked_paths(
            &self.session_dir.join(AGENTS_DIR),
            &preserved_paths,
            &preserved_directories,
            &protected_directories,
            &mut report,
        )?;
        self.write_session_record("active")?;
        Ok(report)
    }

    fn agent_directories(&self) -> Result<HashSet<PathBuf>, SessionTmpError> {
        let agents_dir = self.session_dir.join(AGENTS_DIR);
        super::storage::ensure_directory_not_symlink(&agents_dir)?;
        if !agents_dir.is_dir() {
            return Ok(HashSet::new());
        }
        let mut directories = HashSet::new();
        for item in fs::read_dir(agents_dir)? {
            let path = item?.path();
            if fs::symlink_metadata(&path)
                .map(|metadata| {
                    metadata.file_type().is_dir() && !file_type_is_link(metadata.file_type())
                })
                .unwrap_or(false)
            {
                directories.insert(path);
            }
        }
        Ok(directories)
    }

    fn active_agent_threads(&self) -> Result<HashSet<String>, SessionTmpError> {
        let agents_dir = self.session_dir.join(AGENTS_DIR);
        super::storage::ensure_directory_not_symlink(&agents_dir)?;
        if !agents_dir.is_dir() {
            return Ok(HashSet::new());
        }
        let mut threads = HashSet::new();
        for item in fs::read_dir(agents_dir)? {
            let path = item?.path();
            let Some(thread_id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if thread_id != self.thread_id
                && fs::symlink_metadata(&path)
                    .map(|metadata| {
                        metadata.file_type().is_dir() && !file_type_is_link(metadata.file_type())
                    })
                    .unwrap_or(false)
                && storage::lease_is_fresh_for_thread(&self.session_dir, thread_id)
            {
                threads.insert(thread_id.to_string());
            }
        }
        Ok(threads)
    }

    pub(super) fn write_entry_metadata(
        &self,
        metadata: EntryMetadata,
    ) -> Result<TempEntry, SessionTmpError> {
        self.ensure_session_layout()?;
        let metadata_dir = self.session_dir.join(ENTRY_METADATA_DIR);
        super::storage::ensure_directory_not_symlink(&metadata_dir)?;
        fs::create_dir_all(&metadata_dir)?;
        set_private_directory(&metadata_dir)?;
        let metadata_path = metadata_dir.join(format!("{}.json", metadata.id));
        write_json_atomically(&metadata_path, &metadata)?;
        self.write_session_record("active")?;
        let absolute_path = self.entry_path(&metadata)?;
        Ok(TempEntry {
            exists: absolute_path.exists(),
            metadata,
            absolute_path,
        })
    }

    pub(super) fn write_session_record(&self, status: &str) -> Result<(), SessionTmpError> {
        self.ensure_session_layout()?;
        let path = self.session_dir.join(SESSION_METADATA_FILE);
        let created_at = read_session_record(&path)
            .ok()
            .map(|record| record.created_at)
            .unwrap_or_else(now_seconds);
        write_json_atomically(
            &path,
            &super::storage::SessionRecord {
                schema_version: 1,
                session_id: self.session_id.clone(),
                created_at,
                updated_at: now_seconds(),
                status: status.to_string(),
            },
        )
    }

    pub(super) fn entry_path(&self, metadata: &EntryMetadata) -> Result<PathBuf, SessionTmpError> {
        if metadata.session_id != self.session_id {
            return Err(SessionTmpError::EntryNotOwned(metadata.id.clone()));
        }
        if !self.is_root_session && metadata.thread_id != self.thread_id {
            return Err(SessionTmpError::EntryNotOwned(metadata.id.clone()));
        }
        let mut components = metadata.path.components();
        let valid_path = components
            .next()
            .is_some_and(|component| component.as_os_str() == AGENTS_DIR)
            && components
                .next()
                .is_some_and(|component| component.as_os_str() == metadata.thread_id.as_str())
            && components
                .next()
                .is_some_and(|component| matches!(component, Component::Normal(_)))
            && components.all(|component| matches!(component, Component::Normal(_)));
        if !valid_path {
            return Err(SessionTmpError::PathOutsideAgent(metadata.path.clone()));
        }
        let absolute_path = self.session_dir.join(&metadata.path);
        let agent_dir = self.session_dir.join(AGENTS_DIR).join(&metadata.thread_id);
        if !absolute_path.starts_with(&agent_dir) {
            return Err(SessionTmpError::PathOutsideAgent(absolute_path));
        }
        self.ensure_entry_parent(&absolute_path, &agent_dir)?;
        Ok(absolute_path)
    }

    pub(super) fn ensure_root_identity(&self) -> Result<(), SessionTmpError> {
        super::storage::ensure_directory_not_symlink(&self.root)?;
        let canonical_root = fs::canonicalize(&self.root)?;
        if canonical_root != self.canonical_root {
            return Err(SessionTmpError::UnsafeManagedPath(self.root.clone()));
        }
        let marker = self.root.join(MANAGED_ROOT_MARKER);
        if fs::symlink_metadata(&marker)
            .map(|metadata| storage::file_type_is_link(metadata.file_type()))
            .unwrap_or(false)
        {
            return Err(SessionTmpError::UnsafeManagedPath(marker));
        }
        if fs::read_to_string(&marker)? != MANAGED_ROOT_MARKER_CONTENT {
            return Err(SessionTmpError::RootNotManaged(self.root.clone()));
        }
        Ok(())
    }

    pub(super) fn ensure_session_layout(&self) -> Result<(), SessionTmpError> {
        self.ensure_root_identity()?;
        let sessions_dir = self.root.join(SESSIONS_DIR);
        super::storage::ensure_directory_not_symlink(&sessions_dir)?;
        fs::create_dir_all(&sessions_dir)?;
        super::storage::ensure_directory_not_symlink(&self.session_dir)?;
        fs::create_dir_all(&self.session_dir)?;
        let agents_dir = self.session_dir.join(AGENTS_DIR);
        super::storage::ensure_directory_not_symlink(&agents_dir)?;
        fs::create_dir_all(&agents_dir)?;
        let leases_dir = self.session_dir.join(LEASES_DIR);
        super::storage::ensure_directory_not_symlink(&leases_dir)?;
        fs::create_dir_all(&leases_dir)?;
        super::storage::ensure_directory_not_symlink(&self.agent_dir)?;
        fs::create_dir_all(&self.agent_dir)?;
        Ok(())
    }

    pub(super) fn ensure_entry_parent(
        &self,
        path: &Path,
        agent_dir: &Path,
    ) -> Result<(), SessionTmpError> {
        let canonical_agent = fs::canonicalize(agent_dir)?;
        let mut candidate = path
            .parent()
            .ok_or_else(|| SessionTmpError::PathOutsideAgent(path.to_path_buf()))?;
        loop {
            match fs::symlink_metadata(candidate) {
                Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
                    return Err(SessionTmpError::UnsafeManagedPath(candidate.to_path_buf()));
                }
                Ok(_) => {
                    let canonical_candidate = fs::canonicalize(candidate)?;
                    if !canonical_candidate.starts_with(&canonical_agent) {
                        return Err(SessionTmpError::PathOutsideAgent(path.to_path_buf()));
                    }
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    candidate = candidate
                        .parent()
                        .ok_or_else(|| SessionTmpError::PathOutsideAgent(path.to_path_buf()))?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for SessionTmpManager {
    fn drop(&mut self) {
        if self.cleanup_on_drop
            && let Err(error) = self.clean_paths()
        {
            debug!(error = %error, session_id = %self.session_id, "session temporary cleanup deferred");
        }
    }
}
