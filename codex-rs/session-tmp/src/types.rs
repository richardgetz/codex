use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// The default number of seconds after which an abandoned session is eligible
/// for forced stale-session cleanup.
pub const DEFAULT_STALE_AFTER_SECONDS: u64 = 7 * 24 * 60 * 60;
pub const MAX_PURPOSE_BYTES: usize = 1024;

/// Runtime settings for managed session temporary storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTmpConfig {
    /// Whether managed temporary storage is enabled.
    pub enabled: bool,
    /// An optional absolute parent directory. The default is supplied by the
    /// caller, normally `<codex_home>/session-tmp`.
    pub root: Option<PathBuf>,
    /// Age after which another session may be force-cleaned as abandoned.
    pub stale_after: Duration,
}

impl Default for SessionTmpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            root: None,
            stale_after: Duration::from_secs(DEFAULT_STALE_AFTER_SECONDS),
        }
    }
}

/// Retention policy attached to a tracked path.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "seconds")]
#[derive(Default)]
pub enum Retention {
    /// Remove the path when the owning session ends.
    #[default]
    Session,
    /// Keep the path until an explicit user cleanup or stale-session reap.
    Manual,
    /// Remove the path after the given number of seconds.
    Ttl(u64),
}

impl Retention {
    /// Parse the stable model-facing forms `session`, `manual`, and
    /// `ttl:<seconds>`.
    pub fn parse(value: &str) -> Result<Self, SessionTmpError> {
        match value {
            "session" => Ok(Self::Session),
            "manual" => Ok(Self::Manual),
            value => value
                .strip_prefix("ttl:")
                .ok_or_else(|| SessionTmpError::InvalidRetention(value.to_string()))?
                .parse::<u64>()
                .map(Self::Ttl)
                .map_err(|_| SessionTmpError::InvalidRetention(value.to_string())),
        }
    }

    pub(super) fn expires_at(self, created_at: u64) -> Option<u64> {
        match self {
            Self::Ttl(seconds) => Some(created_at.saturating_add(seconds)),
            Self::Session | Self::Manual => None,
        }
    }

    pub(super) fn eligible_for_cleanup(self, now: u64, created_at: u64) -> bool {
        match self {
            Self::Session => true,
            Self::Manual => false,
            Self::Ttl(seconds) => now >= created_at.saturating_add(seconds),
        }
    }
}

/// Whether a tracked item starts as a file or directory.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TempKind {
    File,
    #[default]
    Directory,
}

/// Identifies whether a manager belongs to the root session or to a spawned
/// agent. Only the root session may perform cleanup operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTmpOwner {
    /// The interactive/root session. Its session-retained paths are cleaned
    /// when the owning manager is dropped.
    RootSession,
    /// A spawned agent. Its paths are isolated to its agent directory, while
    /// cleanup remains owned by the root session or the user.
    Agent,
}

/// Durable metadata for one path created or registered by an agent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EntryMetadata {
    pub id: String,
    pub session_id: String,
    pub thread_id: String,
    pub path: PathBuf,
    pub purpose: String,
    pub retention: Retention,
    pub created_at: u64,
    pub expires_at: Option<u64>,
}

/// One model- or user-visible tracked temporary path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TempEntry {
    pub metadata: EntryMetadata,
    pub absolute_path: PathBuf,
    pub exists: bool,
}

/// A bounded view of the current session's managed temporary storage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SessionTmpListing {
    pub session_id: String,
    pub agent_id: String,
    pub agent_root: PathBuf,
    pub entries: Vec<TempEntry>,
    pub untracked_paths: Vec<PathBuf>,
}

/// Counts from a cleanup operation.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CleanupReport {
    pub removed_paths: usize,
    pub removed_sessions: usize,
    pub preserved_paths: usize,
}

/// Errors returned by managed session temporary storage.
#[derive(Debug, thiserror::Error)]
pub enum SessionTmpError {
    #[error("managed session temporary storage is disabled")]
    Disabled,
    #[error("managed session temporary storage root must be absolute: {0}")]
    RootNotAbsolute(PathBuf),
    #[error("managed session temporary storage root is not marker-protected: {0}")]
    RootNotManaged(PathBuf),
    #[error("managed session temporary storage contains an unsafe path component: {0}")]
    UnsafeManagedPath(PathBuf),
    #[error("invalid session temporary storage component: {0}")]
    InvalidComponent(String),
    #[error("invalid session temporary retention: {0}")]
    InvalidRetention(String),
    #[error("session temporary entry purpose must not be empty")]
    InvalidPurpose,
    #[error("session temporary entry purpose exceeds {MAX_PURPOSE_BYTES} bytes")]
    PurposeTooLong,
    #[error("path is outside the current agent's managed temporary directory: {0}")]
    PathOutsideAgent(PathBuf),
    #[error("session temporary entry is not owned by the current agent: {0}")]
    EntryNotOwned(String),
    #[error("session temporary cleanup is only available to the owning root session")]
    CleanupNotOwned,
    #[error("session temporary storage is already owned by another live thread: {0}")]
    SessionAlreadyOwned(String),
    #[error("invalid persisted session temporary metadata at {path}: {source}")]
    InvalidMetadata {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
