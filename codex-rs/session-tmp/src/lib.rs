//! Persistent, session-owned temporary storage for Codex processes.
//!
//! The managed root is deliberately opt-in and marker-protected. Every path
//! this crate removes must be below `<root>/sessions/<session-id>`, so cleanup
//! never treats an arbitrary user-selected directory as a temporary directory.

mod cleanup;
mod lease;
mod storage;
mod types;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

pub use types::CleanupReport;
pub use types::DEFAULT_STALE_AFTER_SECONDS;
pub use types::EntryMetadata;
pub use types::Retention;
pub use types::SessionTmpConfig;
pub use types::SessionTmpError;
pub use types::SessionTmpListing;
pub use types::SessionTmpOwner;
pub use types::TempEntry;
pub use types::TempKind;

use lease::SessionLease;
use storage::LEASES_DIR;
use storage::collect_paths;
use storage::ensure_directory_not_symlink;
use storage::ensure_managed_root;
use storage::now_seconds;
use storage::read_metadata;
use storage::reap_stale_sessions;
use storage::resolve_user_session_id;
use storage::set_private_directory;
use storage::set_private_file;
use storage::validate_component;
use storage::validate_name;
use storage::write_json_atomically;
use types::MAX_PURPOSE_BYTES;

const MANAGED_ROOT_MARKER: &str = ".codex-managed-session-tmp";
const MANAGED_ROOT_MARKER_CONTENT: &str =
    "codex managed session temporary storage\nschema_version=1\n";
const SESSIONS_DIR: &str = "sessions";
const SESSION_METADATA_FILE: &str = "session.json";
const ENTRY_METADATA_DIR: &str = "metadata";
const AGENTS_DIR: &str = "agents";
const MAX_LIST_ENTRIES: usize = 2_000;

/// The process-local handle for one session and one agent thread.
pub struct SessionTmpManager {
    root: PathBuf,
    canonical_root: PathBuf,
    session_id: String,
    thread_id: String,
    session_dir: PathBuf,
    agent_dir: PathBuf,
    is_root_session: bool,
    cleanup_on_drop: bool,
    _lease: Option<SessionLease>,
}

impl SessionTmpManager {
    /// Open the managed storage for one agent. A disabled config returns
    /// `Ok(None)` without touching the filesystem.
    pub fn open(
        config: &SessionTmpConfig,
        default_root: &Path,
        session_id: &str,
        thread_id: &str,
        owner: SessionTmpOwner,
    ) -> Result<Option<Self>, SessionTmpError> {
        Self::open_inner(
            config,
            default_root,
            session_id,
            thread_id,
            owner,
            CleanupPolicy::OnDrop,
        )
    }

    /// Opens a root-session view for user commands without making its drop
    /// behavior perform automatic cleanup.
    pub fn open_for_user(
        config: &SessionTmpConfig,
        default_root: &Path,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<Self>, SessionTmpError> {
        if !config.enabled {
            return Ok(None);
        }

        validate_component(session_id)?;
        validate_component(thread_id)?;
        let root = config
            .root
            .clone()
            .unwrap_or_else(|| default_root.join("session-tmp"));
        if !root.is_absolute() {
            return Err(SessionTmpError::RootNotAbsolute(root));
        }
        ensure_managed_root(&root)?;
        let session_id = resolve_user_session_id(&root, session_id, thread_id)?;
        Self::open_inner(
            config,
            default_root,
            &session_id,
            thread_id,
            SessionTmpOwner::RootSession,
            CleanupPolicy::ManualOnly,
        )
    }

    fn open_inner(
        config: &SessionTmpConfig,
        default_root: &Path,
        session_id: &str,
        thread_id: &str,
        owner: SessionTmpOwner,
        cleanup_policy: CleanupPolicy,
    ) -> Result<Option<Self>, SessionTmpError> {
        if !config.enabled {
            return Ok(None);
        }

        validate_component(session_id)?;
        validate_component(thread_id)?;
        let root = config
            .root
            .clone()
            .unwrap_or_else(|| default_root.join("session-tmp"));
        if !root.is_absolute() {
            return Err(SessionTmpError::RootNotAbsolute(root));
        }
        ensure_managed_root(&root)?;
        let canonical_root = fs::canonicalize(&root)?;
        let sessions_dir = root.join(SESSIONS_DIR);
        let session_dir = sessions_dir.join(session_id);
        let agent_dir = session_dir.join(AGENTS_DIR).join(thread_id);
        ensure_directory_not_symlink(&sessions_dir)?;
        fs::create_dir_all(&sessions_dir)?;
        set_private_directory(&sessions_dir)?;
        ensure_directory_not_symlink(&session_dir)?;
        fs::create_dir_all(&session_dir)?;
        set_private_directory(&session_dir)?;
        ensure_directory_not_symlink(&session_dir.join(AGENTS_DIR))?;
        fs::create_dir_all(session_dir.join(AGENTS_DIR))?;
        set_private_directory(&session_dir.join(AGENTS_DIR))?;
        ensure_directory_not_symlink(&session_dir.join(LEASES_DIR))?;
        fs::create_dir_all(session_dir.join(LEASES_DIR))?;
        set_private_directory(&session_dir.join(LEASES_DIR))?;
        let lease = (cleanup_policy == CleanupPolicy::OnDrop)
            .then(|| SessionLease::acquire(&session_dir, session_id, thread_id))
            .transpose()?;
        ensure_directory_not_symlink(&agent_dir)?;
        fs::create_dir_all(&agent_dir)?;
        set_private_directory(&agent_dir)?;
        let is_root_session = owner == SessionTmpOwner::RootSession;
        if is_root_session {
            reap_stale_sessions(&root, config.stale_after, Some(session_id))?;
        }
        let manager = Self {
            root,
            canonical_root,
            session_id: session_id.to_string(),
            thread_id: thread_id.to_string(),
            session_dir,
            agent_dir,
            is_root_session,
            cleanup_on_drop: is_root_session && cleanup_policy == CleanupPolicy::OnDrop,
            _lease: lease,
        };
        manager.write_session_record("active")?;
        Ok(Some(manager))
    }

    /// The configured, marker-protected parent root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory shared by the root and all agents in this session.
    pub fn session_root(&self) -> &Path {
        &self.session_dir
    }

    /// The directory assigned to this agent's shell and tools.
    pub fn agent_root(&self) -> &Path {
        &self.agent_dir
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Creates and durably registers an empty file or directory for the
    /// current agent.
    pub fn create(
        &self,
        name: Option<&str>,
        purpose: &str,
        retention: Retention,
        kind: TempKind,
    ) -> Result<TempEntry, SessionTmpError> {
        self.ensure_session_layout()?;
        if purpose.trim().is_empty() {
            return Err(SessionTmpError::InvalidPurpose);
        }
        if purpose.len() > MAX_PURPOSE_BYTES {
            return Err(SessionTmpError::PurposeTooLong);
        }
        ensure_directory_not_symlink(&self.agent_dir)?;
        let id = Uuid::new_v4().simple().to_string();
        let filename = name
            .map(validate_name)
            .transpose()?
            .map_or_else(|| id.clone(), |name| format!("{id}-{name}"));
        let path = self.agent_dir.join(filename);
        match kind {
            TempKind::File => {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                set_private_file(&path)?;
            }
            TempKind::Directory => {
                fs::create_dir(&path)?;
                set_private_directory(&path)?;
            }
        }
        self.write_entry_metadata(EntryMetadata {
            id,
            session_id: self.session_id.clone(),
            thread_id: self.thread_id.clone(),
            path: path
                .strip_prefix(&self.session_dir)
                .map_err(|_| SessionTmpError::PathOutsideAgent(path.clone()))?
                .to_path_buf(),
            purpose: purpose.to_string(),
            expires_at: retention.expires_at(now_seconds()),
            retention,
            created_at: now_seconds(),
        })
    }

    /// Registers an existing path created by a shell command, provided it is
    /// still below the current agent's directory and does not escape through a
    /// symlink.
    pub fn register(
        &self,
        path: &Path,
        purpose: &str,
        retention: Retention,
    ) -> Result<TempEntry, SessionTmpError> {
        self.ensure_session_layout()?;
        if purpose.trim().is_empty() {
            return Err(SessionTmpError::InvalidPurpose);
        }
        if purpose.len() > MAX_PURPOSE_BYTES {
            return Err(SessionTmpError::PurposeTooLong);
        }
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.agent_dir.join(path)
        };
        ensure_directory_not_symlink(&self.agent_dir)?;
        let canonical_session = fs::canonicalize(&self.session_dir)?;
        let canonical_agent = fs::canonicalize(&self.agent_dir)?;
        let canonical_path = fs::canonicalize(&path)?;
        if canonical_path == canonical_agent || !canonical_path.starts_with(&canonical_agent) {
            return Err(SessionTmpError::PathOutsideAgent(path));
        }
        self.ensure_entry_parent(&canonical_path, &canonical_agent)?;
        let id = Uuid::new_v4().simple().to_string();
        let metadata = EntryMetadata {
            id,
            session_id: self.session_id.clone(),
            thread_id: self.thread_id.clone(),
            path: canonical_path
                .strip_prefix(&canonical_session)
                .map_err(|_| SessionTmpError::PathOutsideAgent(path.clone()))?
                .to_path_buf(),
            purpose: purpose.to_string(),
            expires_at: retention.expires_at(now_seconds()),
            retention,
            created_at: now_seconds(),
        };
        self.write_entry_metadata(metadata)
    }

    /// Changes retention for an entry owned by the current agent.
    pub fn set_retention(
        &self,
        entry_id: &str,
        retention: Retention,
    ) -> Result<TempEntry, SessionTmpError> {
        self.ensure_session_layout()?;
        validate_component(entry_id)?;
        let metadata_path = self
            .session_dir
            .join(ENTRY_METADATA_DIR)
            .join(format!("{entry_id}.json"));
        let mut metadata = read_metadata(&metadata_path)?;
        if metadata.session_id != self.session_id || metadata.thread_id != self.thread_id {
            return Err(SessionTmpError::EntryNotOwned(entry_id.to_string()));
        }
        metadata.retention = retention;
        metadata.expires_at = retention.expires_at(metadata.created_at);
        write_json_atomically(&metadata_path, &metadata)?;
        self.write_session_record("active")?;
        let absolute_path = self.entry_path(&metadata)?;
        Ok(TempEntry {
            exists: absolute_path.exists(),
            metadata,
            absolute_path,
        })
    }

    /// Lists metadata plus unregistered files under this session's agent
    /// directories. The directory itself identifies the owning session even
    /// when an agent created a file directly with shell syntax.
    pub fn list(&self) -> Result<SessionTmpListing, SessionTmpError> {
        self.ensure_session_layout()?;
        let mut entries = Vec::new();
        let metadata_dir = self.session_dir.join(ENTRY_METADATA_DIR);
        ensure_directory_not_symlink(&metadata_dir)?;
        if metadata_dir.is_dir() {
            for item in fs::read_dir(&metadata_dir)? {
                let path = item?.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                    continue;
                }
                let metadata = read_metadata(&path)?;
                if !self.is_root_session && metadata.thread_id != self.thread_id {
                    continue;
                }
                let absolute_path = self.entry_path(&metadata)?;
                entries.push(TempEntry {
                    exists: absolute_path.exists(),
                    metadata,
                    absolute_path,
                });
                if entries.len() >= MAX_LIST_ENTRIES {
                    break;
                }
            }
        }
        entries.sort_by_key(|left| left.metadata.created_at);
        let tracked_paths = entries
            .iter()
            .map(|entry| entry.absolute_path.clone())
            .collect::<Vec<_>>();
        let mut untracked_paths = Vec::new();
        let paths_root = if self.is_root_session {
            self.session_dir.join(AGENTS_DIR)
        } else {
            self.agent_dir.clone()
        };
        collect_paths(
            &paths_root,
            &tracked_paths,
            &mut untracked_paths,
            MAX_LIST_ENTRIES,
        )?;
        Ok(SessionTmpListing {
            session_id: self.session_id.clone(),
            agent_id: self.thread_id.clone(),
            agent_root: self.agent_dir.clone(),
            entries,
            untracked_paths,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupPolicy {
    OnDrop,
    ManualOnly,
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
