use super::AGENTS_DIR;
use super::ENTRY_METADATA_DIR;
use super::SESSION_METADATA_FILE;
use super::SESSIONS_DIR;
use super::SessionTmpManager;
use super::storage;
use super::storage::file_type_is_link;
use super::storage::now_seconds;
use super::storage::read_metadata;
use super::storage::read_session_record;
use super::storage::reap_sessions;
use super::storage::remove_path;
use super::storage::remove_untracked_paths;
use super::storage::set_private_directory;
use super::storage::write_json_atomically;
use super::types::CleanupReport;
use super::types::EntryMetadata;
use super::types::ReapMode;
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
        self.retry_recovery_migration()?;
        self.clean_paths()
    }

    /// Explicitly clears every path in the current session, including manual
    /// retention entries. The session directory is recreated for continued use.
    pub fn clear(&self) -> Result<CleanupReport, SessionTmpError> {
        if !self.is_root_session {
            return Err(SessionTmpError::CleanupNotOwned);
        }
        self.retry_recovery_migration()?;
        self.ensure_session_layout()?;
        let _session_lock = storage::lock_session(&self.state_session_dir)?;
        let mut report = CleanupReport::default();
        let metadata_dir = self.state_session_dir.join(ENTRY_METADATA_DIR);
        super::storage::ensure_directory_not_symlink(&metadata_dir)?;
        let active_agent_threads = self.active_agent_threads()?;
        let preserved_directories = active_agent_threads
            .iter()
            .map(|thread_id| self.session_dir.join(AGENTS_DIR).join(thread_id))
            .collect::<HashSet<_>>();
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
                if active_agent_threads.contains(&metadata.thread_id)
                    && metadata.thread_id != self.thread_id
                {
                    report.preserved_paths += 1;
                    continue;
                }
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
            &preserved_directories,
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
        self.reap_with_mode(ReapMode::OlderThan(max_age))
    }

    /// Force-cleans every other session that can be proven inactive by the
    /// managed lock and lease checks, regardless of heartbeat age.
    pub fn reap_with_mode(&self, mode: ReapMode) -> Result<CleanupReport, SessionTmpError> {
        if !self.is_root_session {
            return Err(SessionTmpError::CleanupNotOwned);
        }
        self.retry_recovery_migration()?;
        self.ensure_root_identity()?;
        reap_sessions(&self.state, mode, Some(&self.session_id))
    }

    fn retry_recovery_migration(&self) -> Result<(), SessionTmpError> {
        let default_root = self.state.default_root();
        super::migration::consolidate_recovery(&self.state, &default_root)
    }

    fn clean_paths(&self) -> Result<CleanupReport, SessionTmpError> {
        self.ensure_session_layout()?;
        let _session_lock = storage::lock_session(&self.state_session_dir)?;
        let metadata_dir = self.state_session_dir.join(ENTRY_METADATA_DIR);
        super::storage::ensure_directory_not_symlink(&metadata_dir)?;
        super::storage::ensure_directory_not_symlink(&self.session_dir.join(AGENTS_DIR))?;
        let mut report = CleanupReport::default();
        let mut preserved_paths = HashSet::new();
        let active_agent_threads = self.active_agent_threads()?;
        let mut metadata_entries = Vec::new();
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
                metadata_entries.push((metadata_path, metadata, absolute_path));
            }
        }
        for (_, metadata, absolute_path) in &metadata_entries {
            if active_agent_threads.contains(&metadata.thread_id)
                && metadata.thread_id != self.thread_id
            {
                preserved_paths.insert(absolute_path.clone());
            } else if !metadata
                .retention
                .eligible_for_cleanup(now_seconds(), metadata.created_at)
            {
                preserved_paths.insert(absolute_path.clone());
            }
        }
        for (metadata_path, metadata, absolute_path) in metadata_entries {
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
                let has_preserved_descendant = preserved_paths.iter().any(|preserved| {
                    *preserved != absolute_path && preserved.starts_with(&absolute_path)
                });
                if has_preserved_descendant {
                    report.preserved_paths += 1;
                    continue;
                }
                if remove_path(&absolute_path)? {
                    report.removed_paths += 1;
                }
                fs::remove_file(metadata_path)?;
            } else {
                report.preserved_paths += 1;
                preserved_paths.insert(absolute_path);
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
        let legacy_transition_active = self.state.legacy_transition_active()?;
        let mut threads = HashSet::new();
        if agents_dir.is_dir() {
            for item in fs::read_dir(&agents_dir)? {
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
                    && (storage::lease_is_fresh_for_thread(&self.state_session_dir, thread_id)
                        || (legacy_transition_active
                            && storage::lease_is_fresh_for_thread(
                                &self.session_dir,
                                thread_id,
                            )))
                {
                    threads.insert(thread_id.to_string());
                }
            }
        }
        add_fresh_lease_threads(
            &self.state_session_dir.join(super::storage::LEASES_DIR),
            &self.session_id,
            &self.thread_id,
            &mut threads,
        )?;
        if legacy_transition_active {
            add_fresh_lease_threads(
                &self
                    .state
                    .legacy_session_dir(&self.session_id)
                    .join(super::storage::LEASES_DIR),
                &self.session_id,
                &self.thread_id,
                &mut threads,
            )?;
        }
        Ok(threads)
    }

    pub(super) fn write_entry_metadata(
        &self,
        metadata: EntryMetadata,
    ) -> Result<TempEntry, SessionTmpError> {
        self.ensure_session_layout()?;
        let metadata_dir = self.state_session_dir.join(ENTRY_METADATA_DIR);
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
        let path = self.state_session_dir.join(SESSION_METADATA_FILE);
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
        match fs::symlink_metadata(&agent_dir) {
            Ok(metadata) if file_type_is_link(metadata.file_type()) => {
                return Err(SessionTmpError::UnsafeManagedPath(agent_dir));
            }
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(SessionTmpError::UnsafeManagedPath(agent_dir));
            }
            Ok(_) => self.ensure_entry_parent(&absolute_path, &agent_dir)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A manually deleted payload may remove another agent's
                // directory while its external metadata remains. The path is
                // component-validated above, and no existing descendant can
                // escape when the agent root itself is absent.
            }
            Err(error) => return Err(error.into()),
        }
        Ok(absolute_path)
    }

    pub(super) fn ensure_root_identity(&self) -> Result<(), SessionTmpError> {
        self.state.ensure_identity()?;
        super::state::ensure_existing_ancestors_for_runtime(&self.root)?;
        super::storage::ensure_directory_not_symlink(&self.root)?;
        let canonical_root = super::state::canonicalize_for_identity(&self.root)?;
        if canonical_root != self.canonical_root {
            return Err(SessionTmpError::UnsafeManagedPath(self.root.clone()));
        }
        Ok(())
    }

    pub(super) fn ensure_session_layout(&self) -> Result<(), SessionTmpError> {
        self.ensure_root_identity()?;
        let payload_namespace = &self.payload_namespace;
        super::storage::ensure_directory_not_symlink(payload_namespace)?;
        fs::create_dir_all(payload_namespace)?;
        super::storage::set_private_directory(payload_namespace)?;
        let sessions_dir = payload_namespace.join(SESSIONS_DIR);
        super::storage::ensure_directory_not_symlink(&sessions_dir)?;
        fs::create_dir_all(&sessions_dir)?;
        super::storage::set_private_directory(&sessions_dir)?;
        super::storage::ensure_directory_not_symlink(&self.session_dir)?;
        fs::create_dir_all(&self.session_dir)?;
        super::storage::set_private_directory(&self.session_dir)?;
        let agents_dir = self.session_dir.join(AGENTS_DIR);
        super::storage::ensure_directory_not_symlink(&agents_dir)?;
        fs::create_dir_all(&agents_dir)?;
        super::storage::set_private_directory(&agents_dir)?;
        super::storage::ensure_directory_not_symlink(&self.state_session_dir)?;
        fs::create_dir_all(&self.state_session_dir)?;
        super::storage::set_private_directory(&self.state_session_dir)?;
        super::storage::ensure_directory_not_symlink(&self.agent_dir)?;
        fs::create_dir_all(&self.agent_dir)?;
        super::storage::set_private_directory(&self.agent_dir)?;
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

fn add_fresh_lease_threads(
    leases_dir: &Path,
    session_id: &str,
    current_thread_id: &str,
    threads: &mut HashSet<String>,
) -> Result<(), SessionTmpError> {
    match fs::symlink_metadata(leases_dir) {
        Ok(metadata) if file_type_is_link(metadata.file_type()) || !metadata.file_type().is_dir() => {
            return Err(SessionTmpError::UnsafeManagedPath(leases_dir.to_path_buf()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    for item in fs::read_dir(leases_dir)? {
        let path = item?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if file_type_is_link(metadata.file_type()) || !metadata.file_type().is_file() {
            return Err(SessionTmpError::UnsafeManagedPath(path));
        }
        let Ok(record) = storage::read_lease_record(&path) else {
            continue;
        };
        let Some(thread_name) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if record.schema_version == 1
            && record.session_id == session_id
            && path.extension().and_then(|extension| extension.to_str()) == Some("json")
            && thread_name == record.thread_id
            && record.thread_id != current_thread_id
            && storage::validate_component(&record.thread_id).is_ok()
            && storage::lease_is_fresh(&path, storage::LEASE_STALE_AFTER)
        {
            threads.insert(record.thread_id);
        }
    }
    Ok(())
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
