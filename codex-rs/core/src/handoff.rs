//! Durable state for replacing a shared app-server without replaying work.
//!
//! The handoff journal is intentionally separate from rollout history. Rollouts
//! preserve the model context and unfinished turn ID; this journal records the
//! process-local ownership and whether that turn was actually stopped cleanly.
//! A coordinator must persist `Prepared`/`Draining` before changing execution,
//! then persist each node after its writer closes. An incomplete or malformed
//! journal is a recovery warning, never permission to replay a tool call.

use crate::HandoffBlocker;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const HANDOFF_DIRECTORY: &str = "handoffs";
const HANDOFF_FILE_SUFFIX: &str = ".json";
const HANDOFF_SCHEMA_VERSION: u32 = 1;

/// Lifecycle of one durable handoff attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HandoffJournalState {
    /// The node graph and pre-handoff state have been recorded.
    Prepared,
    /// Admission is sealed and node suspension is in progress.
    Draining,
    /// Every node that can be transferred has closed its writer.
    Suspended,
    /// The replacement process has started loading and recovering nodes.
    Restoring,
    /// All recoverable nodes were restored; attention nodes remain visible in receipts.
    Completed,
    /// The attempt stopped before a safe transfer and must be reviewed manually.
    NeedsAttention,
}

/// Lifecycle of one thread entry within a handoff attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HandoffNodeState {
    Planned,
    Suspending,
    Suspended,
    Recovering,
    Restored,
    Paused,
    NeedsAttention,
}

/// Durable identity and recovery state for one loaded root or descendant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffNode {
    pub thread_id: String,
    pub root_thread_id: String,
    pub parent_thread_id: Option<String>,
    pub agent_path: Option<String>,
    /// The unfinished regular turn ID, when exact recovery is possible.
    pub turn_id: Option<String>,
    /// Rollout path captured before the old runtime closes its writer. Replacement uses this
    /// path to load the committed history without replaying an input or tool call.
    #[serde(default)]
    pub rollout_path: Option<String>,
    /// Whether a task owned this node when the handoff snapshot was taken.
    pub was_running: bool,
    /// Manual pause is process-local and must be re-applied before recovery.
    pub was_paused: bool,
    pub state: HandoffNodeState,
    pub blockers: Vec<HandoffBlocker>,
}

/// Durable handoff epoch shared by one root tree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffJournal {
    pub schema_version: u32,
    pub handoff_id: String,
    pub created_at_ms: i64,
    /// Build/runtime identity used for compatibility checks on restore.
    pub runtime_version: String,
    pub state: HandoffJournalState,
    pub nodes: Vec<HandoffNode>,
}

impl HandoffJournal {
    /// Create and durably publish a prepared handoff epoch.
    pub async fn begin(
        codex_home: &Path,
        runtime_version: impl Into<String>,
        nodes: Vec<HandoffNode>,
    ) -> io::Result<Self> {
        let journal = Self {
            schema_version: HANDOFF_SCHEMA_VERSION,
            handoff_id: Uuid::now_v7().to_string(),
            created_at_ms: current_time_ms(),
            runtime_version: runtime_version.into(),
            state: HandoffJournalState::Prepared,
            nodes,
        };
        journal.persist(codex_home).await?;
        Ok(journal)
    }

    /// Absolute path for this journal under the user's Codex home.
    pub fn path(&self, codex_home: &Path) -> PathBuf {
        journal_path(codex_home, &self.handoff_id)
    }

    /// Persist the current epoch atomically and force it to stable storage.
    ///
    /// The temporary file is created with a unique name and mode `0600` on
    /// Unix. Windows uses `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` so
    /// receipt updates replace an existing journal safely. A parent-directory
    /// sync follows the rename on Unix so a power loss cannot leave a caller
    /// believing that a receipt exists when it does not.
    pub async fn persist(&self, codex_home: &Path) -> io::Result<()> {
        validate_handoff_id(&self.handoff_id)?;
        let path = self.path(codex_home);
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(ErrorKind::InvalidInput, "handoff journal has no parent")
        })?;
        fs::create_dir_all(parent).await?;
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let temp_path = parent.join(format!(
            ".{}.{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    io::Error::new(ErrorKind::InvalidInput, "handoff journal has no filename")
                })?,
            Uuid::new_v4()
        ));
        let result = async {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp_path).await?;
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            drop(file);
            replace_handoff_file(&temp_path, &path).await?;
            sync_parent_directory(parent).await
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temp_path).await;
        }
        result
    }

    /// Change the top-level state before or after a side effect.
    pub fn set_state(&mut self, state: HandoffJournalState) {
        self.state = state;
    }

    /// Update one node's receipt without changing unrelated nodes.
    pub fn update_node(
        &mut self,
        thread_id: &str,
        state: HandoffNodeState,
        blockers: Vec<HandoffBlocker>,
        turn_id: Option<String>,
    ) -> bool {
        let Some(node) = self.nodes.iter_mut().find(|node| node.thread_id == thread_id) else {
            return false;
        };
        node.state = state;
        node.blockers = blockers;
        if turn_id.is_some() {
            node.turn_id = turn_id;
        }
        true
    }

    /// Clear a stale unfinished-turn identity when a node became idle before suspension.
    ///
    /// `update_node` keeps an existing ID when its optional turn argument is `None`, which lets
    /// callers update only state/blockers. Coordinators must call this explicit method after a
    /// `NotActive` result so a replacement never attempts to recover a completed turn.
    pub fn clear_node_turn_id(&mut self, thread_id: &str) -> bool {
        let Some(node) = self.nodes.iter_mut().find(|node| node.thread_id == thread_id) else {
            return false;
        };
        node.turn_id = None;
        true
    }

    /// Read all journals, including completed epochs, in deterministic order.
    pub async fn load_all(codex_home: &Path) -> io::Result<Vec<Self>> {
        let mut entries = match fs::read_dir(codex_home.join(HANDOFF_DIRECTORY)).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut paths = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str())
                == Some(HANDOFF_FILE_SUFFIX.trim_start_matches('.'))
            {
                paths.push(path);
            }
        }
        paths.sort();
        let mut journals = Vec::with_capacity(paths.len());
        for path in paths {
            let bytes = fs::read(path).await?;
            let journal: HandoffJournal = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if journal.schema_version != HANDOFF_SCHEMA_VERSION {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "unsupported handoff journal schema {}",
                        journal.schema_version
                    ),
                ));
            }
            journals.push(journal);
        }
        Ok(journals)
    }

    /// Read epochs that still require replacement-process attention.
    pub async fn load_pending(codex_home: &Path) -> io::Result<Vec<Self>> {
        Ok(Self::load_all(codex_home)
            .await?
            .into_iter()
            .filter(|journal| {
                matches!(
                    journal.state,
                    HandoffJournalState::Prepared
                        | HandoffJournalState::Draining
                        | HandoffJournalState::Suspended
                        | HandoffJournalState::Restoring
                        | HandoffJournalState::NeedsAttention
                )
            })
            .collect())
    }
}

fn journal_path(codex_home: &Path, handoff_id: &str) -> PathBuf {
    codex_home
        .join(HANDOFF_DIRECTORY)
        .join(format!("{handoff_id}{HANDOFF_FILE_SUFFIX}"))
}

fn validate_handoff_id(handoff_id: &str) -> io::Result<()> {
    if handoff_id.is_empty()
        || !handoff_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "handoff id contains unsupported path characters",
        ));
    }
    Ok(())
}

fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

async fn replace_handoff_file(temp_path: &Path, path: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(temp_path, path).await
    }
    #[cfg(windows)]
    {
        let temp_path = temp_path.to_path_buf();
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || replace_handoff_file_windows(&temp_path, &path))
            .await
            .map_err(io::Error::other)?
    }
}

#[cfg(windows)]
fn replace_handoff_file_windows(temp_path: &Path, path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let temp_path = temp_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both vectors are NUL-terminated UTF-16 paths that remain alive for the call, and
    // the flags request an atomic replacement with write-through semantics.
    let result = unsafe {
        MoveFileExW(
            temp_path.as_ptr(),
            path.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

async fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = parent.to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
            .await
            .map_err(io::Error::other)??;
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::HandoffJournal;
    use super::HandoffJournalState;
    use super::HandoffNode;
    use super::HandoffNodeState;
    use crate::HandoffBlocker;
    use tempfile::tempdir;

    fn node(thread_id: &str) -> HandoffNode {
        HandoffNode {
            thread_id: thread_id.to_string(),
            root_thread_id: "root".to_string(),
            parent_thread_id: None,
            agent_path: None,
            turn_id: Some("turn".to_string()),
            rollout_path: Some("/tmp/thread.jsonl".to_string()),
            was_running: true,
            was_paused: false,
            state: HandoffNodeState::Planned,
            blockers: Vec::new(),
        }
    }

    #[tokio::test]
    async fn begin_and_update_are_durable() {
        let home = tempdir().expect("temporary home");
        let mut journal = HandoffJournal::begin(home.path(), "test", vec![node("thread")])
            .await
            .expect("begin handoff");
        assert_eq!(HandoffJournal::load_pending(home.path()).await.unwrap().len(), 1);
        assert!(journal.update_node(
            "thread",
            HandoffNodeState::NeedsAttention,
            vec![HandoffBlocker::PendingApproval],
            None,
        ));
        assert!(journal.clear_node_turn_id("thread"));
        journal.set_state(HandoffJournalState::NeedsAttention);
        journal.persist(home.path()).await.expect("persist update");
        let restored = HandoffJournal::load_pending(home.path())
            .await
            .expect("load update");
        assert_eq!(restored[0], journal);
    }

    #[tokio::test]
    async fn completed_epochs_are_not_pending() {
        let home = tempdir().expect("temporary home");
        let mut journal = HandoffJournal::begin(home.path(), "test", vec![node("thread")])
            .await
            .expect("begin handoff");
        journal.set_state(HandoffJournalState::Completed);
        journal.persist(home.path()).await.expect("persist complete");
        assert!(HandoffJournal::load_pending(home.path())
            .await
            .expect("load pending")
            .is_empty());
    }
}
