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
use std::path::Path;
use std::path::PathBuf;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const HANDOFF_DIRECTORY: &str = "handoffs";
const HANDOFF_FILE_SUFFIX: &str = ".json";
const HANDOFF_SCHEMA_VERSION: u32 = 1;

/// Which metadata-only queue caused a `pendingMailbox` handoff blocker.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PendingMailboxBlockerSource {
    /// Inter-agent messages waiting outside the rollout.
    InterAgentMailbox,
    /// A synthetic scheduler deadline wake waiting outside the rollout.
    LeadOversightWake,
    /// A manager-generated summary of buffered Lead progress waiting for delivery.
    LeadProgressSummary,
    /// A bounded batch of manager-only Worker completions awaiting delivery.
    ManagerCompletionBatch,
}

/// Current, in-memory pending-mailbox measurements captured during handoff preflight.
///
/// This intentionally contains no message body, author, recipient, or message ID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingMailboxBlockerDetail {
    pub thread_id: String,
    pub source: PendingMailboxBlockerSource,
    /// Number of queued entries, or one pending completion batch.
    pub count: u64,
    /// Age of the oldest queued message or the oldest item in the pending batch.
    pub oldest_age_ms: Option<u64>,
}

/// Durable metadata-only observation of a pending-mailbox handoff blocker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffBlockerDiagnostic {
    pub thread_id: String,
    pub blocker: HandoffBlocker,
    pub source: PendingMailboxBlockerSource,
    pub count: u64,
    pub oldest_age_ms: Option<u64>,
    pub first_observed_at_ms: i64,
    pub last_observed_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by_handoff_id: Option<String>,
}

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
    /// Whether the coordinator crossed the durable drain boundary. `None` is retained for
    /// journals written before this marker existed and must be classified conservatively.
    #[serde(default)]
    pub transfer_started: Option<bool>,
    /// Whether an operator explicitly quarantined this unresolved receipt after durable pause.
    /// Quarantined journals retain their diagnostics but never admit automatic recovery.
    #[serde(default)]
    pub quarantined: bool,
    pub nodes: Vec<HandoffNode>,
    /// Pending-mailbox blocker metadata, kept separate from the node's safety blockers so later
    /// observations can record resolution without changing transfer or recovery admission.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocker_diagnostics: Vec<HandoffBlockerDiagnostic>,
}

impl HandoffJournal {
    /// Create and durably publish a prepared handoff epoch.
    pub async fn begin(
        codex_home: &Path,
        runtime_version: impl Into<String>,
        nodes: Vec<HandoffNode>,
    ) -> io::Result<Self> {
        Self::begin_with_pending_mailbox_diagnostics(codex_home, runtime_version, nodes, Vec::new())
            .await
    }

    /// Create a prepared handoff while durably attaching its initial pending-mailbox evidence.
    pub async fn begin_with_pending_mailbox_diagnostics(
        codex_home: &Path,
        runtime_version: impl Into<String>,
        nodes: Vec<HandoffNode>,
        pending_mailbox_diagnostics: Vec<PendingMailboxBlockerDetail>,
    ) -> io::Result<Self> {
        let created_at_ms = current_time_ms();
        let journal = Self {
            schema_version: HANDOFF_SCHEMA_VERSION,
            handoff_id: Uuid::now_v7().to_string(),
            created_at_ms,
            runtime_version: runtime_version.into(),
            state: HandoffJournalState::Prepared,
            transfer_started: Some(false),
            quarantined: false,
            nodes,
            blocker_diagnostics: pending_mailbox_diagnostics
                .into_iter()
                .filter(|diagnostic| diagnostic.count > 0)
                .map(|diagnostic| HandoffBlockerDiagnostic {
                    thread_id: diagnostic.thread_id,
                    blocker: HandoffBlocker::PendingMailbox,
                    source: diagnostic.source,
                    count: diagnostic.count,
                    oldest_age_ms: diagnostic.oldest_age_ms,
                    first_observed_at_ms: created_at_ms,
                    last_observed_at_ms: created_at_ms,
                    resolved_at_ms: None,
                    resolved_by_handoff_id: None,
                })
                .collect(),
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

    /// Mark the point after which a replacement may have to recover an interrupted transfer.
    pub fn mark_transfer_started(&mut self) {
        self.transfer_started = Some(true);
    }

    /// Mark an unresolved receipt as explicitly quarantined after its affected roots were
    /// durably paused. This never changes the original state or node diagnostics.
    pub fn mark_quarantined(&mut self) {
        self.quarantined = true;
    }

    /// Return whether this journal still fences ordinary startup writes.
    ///
    /// A `NeedsAttention` journal written by current runtimes is terminal when preparation
    /// failed before the durable drain boundary. An empty journal with a positive transfer marker
    /// is also terminal: the complete snapshot proves that no turn was captured. Legacy journals
    /// have no marker, so an empty receipt remains pending. Any exact turn or transfer-state node
    /// remains fenced so its state cannot be lost.
    pub fn requires_recovery(&self) -> bool {
        match self.state {
            HandoffJournalState::Completed => false,
            HandoffJournalState::Prepared
            | HandoffJournalState::Draining
            | HandoffJournalState::Suspended
            | HandoffJournalState::Restoring => true,
            HandoffJournalState::NeedsAttention if self.quarantined => false,
            HandoffJournalState::NeedsAttention => match self.transfer_started {
                Some(false) => !self.is_preparation_failure_shape(),
                Some(true) | None => !self.is_failed_preparation_shape(),
            },
        }
    }

    fn is_preparation_failure_shape(&self) -> bool {
        !self.nodes.is_empty()
            && self.nodes.iter().all(|node| {
                has_structural_identity(node)
                    && matches!(
                        node.state,
                        HandoffNodeState::Planned | HandoffNodeState::NeedsAttention
                    )
            })
    }

    fn is_failed_preparation_shape(&self) -> bool {
        if self.nodes.is_empty() {
            return self.transfer_started == Some(true);
        }
        self.nodes.iter().all(|node| {
            has_structural_identity(node)
                && matches!(
                    node.state,
                    HandoffNodeState::Planned | HandoffNodeState::NeedsAttention
                )
                && node.turn_id.is_none()
                && !node.was_running
        })
    }

    /// Update one node's receipt without changing unrelated nodes.
    pub fn update_node(
        &mut self,
        thread_id: &str,
        state: HandoffNodeState,
        blockers: Vec<HandoffBlocker>,
        turn_id: Option<String>,
    ) -> bool {
        let Some(node) = self
            .nodes
            .iter_mut()
            .find(|node| node.thread_id == thread_id)
        else {
            return false;
        };
        node.state = state;
        node.blockers = blockers;
        if turn_id.is_some() {
            node.turn_id = turn_id;
        }
        true
    }

    /// Reconcile metadata-only pending-mailbox observations for a thread.
    ///
    /// A missing source closes its current observation, while a still-pending source refreshes
    /// its count and oldest age. This does not modify the node's blocker list or recovery state.
    pub fn record_pending_mailbox_diagnostics(
        &mut self,
        thread_id: &str,
        current: &[PendingMailboxBlockerDetail],
    ) -> bool {
        let current = current
            .iter()
            .filter(|diagnostic| diagnostic.thread_id == thread_id && diagnostic.count > 0)
            .collect::<Vec<_>>();
        let observed_at_ms = current_time_ms();
        let handoff_id = self.handoff_id.clone();
        let active_sources = current
            .iter()
            .map(|detail| detail.source)
            .collect::<Vec<_>>();
        let mut changed = self.resolve_pending_mailbox_diagnostics(
            thread_id,
            &active_sources,
            observed_at_ms,
            &handoff_id,
        );

        for detail in current {
            if let Some(diagnostic) = self
                .blocker_diagnostics
                .iter_mut()
                .rev()
                .find(|diagnostic| {
                    diagnostic.thread_id == thread_id
                        && diagnostic.source == detail.source
                        && diagnostic.resolved_at_ms.is_none()
                })
            {
                if diagnostic.count != detail.count
                    || diagnostic.oldest_age_ms != detail.oldest_age_ms
                    || diagnostic.last_observed_at_ms != observed_at_ms
                {
                    diagnostic.count = detail.count;
                    diagnostic.oldest_age_ms = detail.oldest_age_ms;
                    diagnostic.last_observed_at_ms = observed_at_ms;
                    changed = true;
                }
            } else {
                self.blocker_diagnostics.push(HandoffBlockerDiagnostic {
                    thread_id: thread_id.to_string(),
                    blocker: HandoffBlocker::PendingMailbox,
                    source: detail.source,
                    count: detail.count,
                    oldest_age_ms: detail.oldest_age_ms,
                    first_observed_at_ms: observed_at_ms,
                    last_observed_at_ms: observed_at_ms,
                    resolved_at_ms: None,
                    resolved_by_handoff_id: None,
                });
                changed = true;
            }
        }

        changed
    }

    /// Mark old observations resolved when a later handoff preflight sees those sources drained.
    ///
    /// This updates diagnostics only. It deliberately leaves the historical blocker list and
    /// handoff state untouched.
    pub fn resolve_pending_mailbox_diagnostics(
        &mut self,
        thread_id: &str,
        active_sources: &[PendingMailboxBlockerSource],
        resolved_at_ms: i64,
        resolved_by_handoff_id: &str,
    ) -> bool {
        let mut changed = false;
        for diagnostic in self.blocker_diagnostics.iter_mut().filter(|diagnostic| {
            diagnostic.thread_id == thread_id && diagnostic.resolved_at_ms.is_none()
        }) {
            if !active_sources.contains(&diagnostic.source) {
                diagnostic.resolved_at_ms = Some(resolved_at_ms);
                diagnostic.resolved_by_handoff_id = Some(resolved_by_handoff_id.to_string());
                changed = true;
            }
        }
        changed
    }

    /// Clear a stale unfinished-turn identity when a node became idle before suspension.
    ///
    /// `update_node` keeps an existing ID when its optional turn argument is `None`, which lets
    /// callers update only state/blockers. Coordinators must call this explicit method after a
    /// `NotActive` result so a replacement never attempts to recover a completed turn.
    pub fn clear_node_turn_id(&mut self, thread_id: &str) -> bool {
        let Some(node) = self
            .nodes
            .iter_mut()
            .find(|node| node.thread_id == thread_id)
        else {
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
            let journal: HandoffJournal =
                serde_json::from_slice(&bytes).map_err(io::Error::other)?;
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
            .filter(HandoffJournal::requires_recovery)
            .collect())
    }
}

fn has_structural_identity(node: &HandoffNode) -> bool {
    !node.thread_id.trim().is_empty()
        && !node.root_thread_id.trim().is_empty()
        && node
            .parent_thread_id
            .as_deref()
            .is_none_or(|parent_thread_id| !parent_thread_id.trim().is_empty())
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
    use super::PendingMailboxBlockerDetail;
    use super::PendingMailboxBlockerSource;
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
        assert_eq!(
            HandoffJournal::load_pending(home.path())
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(journal.update_node(
            "thread",
            HandoffNodeState::NeedsAttention,
            vec![HandoffBlocker::PendingApproval],
            None,
        ));
        assert!(journal.clear_node_turn_id("thread"));
        journal.set_state(HandoffJournalState::NeedsAttention);
        journal.persist(home.path()).await.expect("persist update");
        let restored = HandoffJournal::load_all(home.path())
            .await
            .expect("load update");
        assert_eq!(restored[0], journal);
        assert!(
            HandoffJournal::load_pending(home.path())
                .await
                .expect("load pending")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn pending_mailbox_diagnostics_are_durable_and_record_resolution() {
        let home = tempdir().expect("temporary home");
        let mut journal = HandoffJournal::begin_with_pending_mailbox_diagnostics(
            home.path(),
            "test",
            vec![node("thread")],
            vec![PendingMailboxBlockerDetail {
                thread_id: "thread".to_string(),
                source: PendingMailboxBlockerSource::InterAgentMailbox,
                count: 2,
                oldest_age_ms: Some(750),
            }],
        )
        .await
        .expect("begin handoff with metadata-only diagnostics");

        assert_eq!(journal.blocker_diagnostics.len(), 1);
        let observation = &journal.blocker_diagnostics[0];
        assert_eq!(observation.blocker, HandoffBlocker::PendingMailbox);
        assert_eq!(
            observation.source,
            PendingMailboxBlockerSource::InterAgentMailbox
        );
        assert_eq!(observation.count, 2);
        assert_eq!(observation.oldest_age_ms, Some(750));
        assert_eq!(observation.first_observed_at_ms, journal.created_at_ms);
        assert_eq!(observation.last_observed_at_ms, journal.created_at_ms);
        assert!(observation.resolved_at_ms.is_none());

        assert!(journal.record_pending_mailbox_diagnostics("thread", &[]));
        let observation = &journal.blocker_diagnostics[0];
        assert!(observation.resolved_at_ms.is_some());
        assert_eq!(
            observation.resolved_by_handoff_id.as_deref(),
            Some(journal.handoff_id.as_str())
        );

        journal
            .persist(home.path())
            .await
            .expect("persist resolution");
        let restored = HandoffJournal::load_all(home.path())
            .await
            .expect("load handoff diagnostics");
        assert_eq!(restored, vec![journal]);
    }

    #[tokio::test]
    async fn later_preflight_resolution_keeps_prior_handoff_blockers_unchanged() {
        let home = tempdir().expect("temporary home");
        let mut journal = HandoffJournal::begin_with_pending_mailbox_diagnostics(
            home.path(),
            "test",
            vec![node("thread")],
            vec![PendingMailboxBlockerDetail {
                thread_id: "thread".to_string(),
                source: PendingMailboxBlockerSource::ManagerCompletionBatch,
                count: 1,
                oldest_age_ms: Some(20),
            }],
        )
        .await
        .expect("begin blocked handoff");
        journal.update_node(
            "thread",
            HandoffNodeState::NeedsAttention,
            vec![HandoffBlocker::PendingMailbox],
            None,
        );
        journal.set_state(HandoffJournalState::NeedsAttention);
        let requires_recovery = journal.requires_recovery();
        assert!(!requires_recovery);

        assert!(journal.resolve_pending_mailbox_diagnostics(
            "thread",
            &[],
            journal.created_at_ms + 1,
            "later-handoff-id",
        ));

        assert_eq!(journal.state, HandoffJournalState::NeedsAttention);
        assert_eq!(journal.requires_recovery(), requires_recovery);
        assert_eq!(
            journal.nodes[0].blockers,
            vec![HandoffBlocker::PendingMailbox]
        );
        assert_eq!(
            journal.blocker_diagnostics[0]
                .resolved_by_handoff_id
                .as_deref(),
            Some("later-handoff-id")
        );
        assert_eq!(
            journal.blocker_diagnostics[0].resolved_at_ms,
            Some(journal.created_at_ms + 1)
        );
        journal
            .persist(home.path())
            .await
            .expect("persist resolution");
        let restored = HandoffJournal::load_all(home.path())
            .await
            .expect("load resolved prior handoff");
        assert_eq!(restored, vec![journal]);
    }

    #[tokio::test]
    async fn completed_epochs_are_not_pending() {
        let home = tempdir().expect("temporary home");
        let mut journal = HandoffJournal::begin(home.path(), "test", vec![node("thread")])
            .await
            .expect("begin handoff");
        journal.set_state(HandoffJournalState::Completed);
        journal
            .persist(home.path())
            .await
            .expect("persist complete");
        assert!(
            HandoffJournal::load_pending(home.path())
                .await
                .expect("load pending")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn needs_attention_epoch_does_not_block_fresh_startup() {
        let home = tempdir().expect("temporary CODEX_HOME");
        let mut journal = HandoffJournal::begin(home.path(), "test", vec![node("thread")])
            .await
            .expect("begin handoff");
        journal.set_state(HandoffJournalState::NeedsAttention);
        journal
            .persist(home.path())
            .await
            .expect("persist needs-attention handoff");

        assert!(
            HandoffJournal::load_pending(home.path())
                .await
                .expect("load pending handoffs")
                .is_empty(),
            "a terminal needs-attention receipt must not fence unrelated startup writes"
        );
    }

    #[tokio::test]
    async fn quarantined_needs_attention_is_retained_without_fencing_startup() {
        let home = tempdir().expect("temporary home");
        let mut journal = HandoffJournal::begin(home.path(), "test", vec![node("thread")])
            .await
            .expect("begin handoff");
        journal.set_state(HandoffJournalState::NeedsAttention);
        journal.mark_quarantined();
        journal
            .persist(home.path())
            .await
            .expect("persist quarantine");

        let retained = HandoffJournal::load_all(home.path())
            .await
            .expect("load all");
        assert_eq!(retained.len(), 1);
        assert!(retained[0].quarantined);
        assert!(
            HandoffJournal::load_pending(home.path())
                .await
                .expect("load pending")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn empty_needs_attention_epoch_remains_pending_without_positive_evidence() {
        let home = tempdir().expect("temporary CODEX_HOME");
        let mut journal = HandoffJournal::begin(home.path(), "test", Vec::new())
            .await
            .expect("begin empty handoff");
        journal.transfer_started = None;
        journal.set_state(HandoffJournalState::NeedsAttention);
        journal
            .persist(home.path())
            .await
            .expect("persist malformed needs-attention handoff");

        assert_eq!(
            HandoffJournal::load_pending(home.path())
                .await
                .expect("load pending handoffs")
                .len(),
            1,
            "an empty legacy receipt has no evidence that startup can be unfenced"
        );
    }

    #[tokio::test]
    async fn empty_post_transfer_needs_attention_does_not_fence_startup() {
        let home = tempdir().expect("temporary home");
        let mut journal = HandoffJournal::begin(home.path(), "test", Vec::new())
            .await
            .expect("begin empty handoff");
        journal.mark_transfer_started();
        journal.set_state(HandoffJournalState::NeedsAttention);
        journal
            .persist(home.path())
            .await
            .expect("persist empty post-transfer handoff");

        assert!(
            HandoffJournal::load_pending(home.path())
                .await
                .expect("load pending handoffs")
                .is_empty(),
            "a proven empty post-transfer receipt has no work to fence"
        );
    }

    #[tokio::test]
    async fn malformed_needs_attention_identity_remains_pending() {
        let home = tempdir().expect("temporary CODEX_HOME");
        let mut journal = HandoffJournal::begin(
            home.path(),
            "test",
            vec![HandoffNode {
                thread_id: " ".to_string(),
                root_thread_id: String::new(),
                parent_thread_id: Some(String::new()),
                ..node("ignored")
            }],
        )
        .await
        .expect("begin malformed handoff");
        journal.transfer_started = Some(false);
        journal.set_state(HandoffJournalState::NeedsAttention);
        journal
            .persist(home.path())
            .await
            .expect("persist malformed identity");

        assert_eq!(
            HandoffJournal::load_pending(home.path())
                .await
                .expect("load pending handoffs")
                .len(),
            1,
            "malformed identities do not prove that startup can be unfenced"
        );
    }

    #[tokio::test]
    async fn legacy_failed_preparation_shapes_are_terminal_but_retained() {
        let home = tempdir().expect("temporary home");
        let mut persistence = HandoffJournal::begin(home.path(), "test", vec![node("thread")])
            .await
            .expect("begin handoff");
        persistence.transfer_started = None;
        persistence.nodes[0].turn_id = None;
        persistence.nodes[0].was_running = false;
        persistence.nodes[0].state = HandoffNodeState::NeedsAttention;
        persistence.nodes[0].blockers = vec![HandoffBlocker::Persistence];
        persistence.set_state(HandoffJournalState::NeedsAttention);
        persistence
            .persist(home.path())
            .await
            .expect("persist persistence failure");

        let mut parent_unavailable = HandoffJournal::begin(
            home.path(),
            "test",
            vec![HandoffNode {
                parent_thread_id: Some("missing-parent".to_string()),
                ..node("child")
            }],
        )
        .await
        .expect("begin parent handoff");
        parent_unavailable.transfer_started = None;
        parent_unavailable.nodes[0].turn_id = None;
        parent_unavailable.nodes[0].was_running = false;
        parent_unavailable.nodes[0].state = HandoffNodeState::NeedsAttention;
        parent_unavailable.nodes[0].blockers = vec![HandoffBlocker::ParentUnavailable];
        parent_unavailable.set_state(HandoffJournalState::NeedsAttention);
        parent_unavailable
            .persist(home.path())
            .await
            .expect("persist parent failure");

        let all = HandoffJournal::load_all(home.path())
            .await
            .expect("load all");
        assert_eq!(all.len(), 2);
        assert!(
            HandoffJournal::load_pending(home.path())
                .await
                .expect("load pending")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn transfer_started_or_ambiguous_needs_attention_remains_pending() {
        let home = tempdir().expect("temporary home");
        let mut transfer = HandoffJournal::begin(
            home.path(),
            "test",
            vec![HandoffNode {
                state: HandoffNodeState::Suspended,
                turn_id: Some("turn".to_string()),
                ..node("suspended")
            }],
        )
        .await
        .expect("begin transfer");
        transfer.mark_transfer_started();
        transfer.set_state(HandoffJournalState::NeedsAttention);
        transfer
            .persist(home.path())
            .await
            .expect("persist transfer");

        let mut ambiguous = HandoffJournal::begin(
            home.path(),
            "test",
            vec![HandoffNode {
                was_running: true,
                ..node("ambiguous")
            }],
        )
        .await
        .expect("begin ambiguous");
        ambiguous.transfer_started = None;
        ambiguous.set_state(HandoffJournalState::NeedsAttention);
        ambiguous
            .persist(home.path())
            .await
            .expect("persist ambiguous");

        let pending = HandoffJournal::load_pending(home.path())
            .await
            .expect("load pending");
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(HandoffJournal::requires_recovery));
    }

    #[tokio::test]
    async fn transfer_started_without_transfer_evidence_is_terminal() {
        let home = tempdir().expect("temporary home");
        let mut journal = HandoffJournal::begin(
            home.path(),
            "test",
            vec![HandoffNode {
                state: HandoffNodeState::NeedsAttention,
                turn_id: None,
                was_running: false,
                ..node("blocked")
            }],
        )
        .await
        .expect("begin handoff");
        journal.mark_transfer_started();
        journal.set_state(HandoffJournalState::NeedsAttention);
        journal.persist(home.path()).await.expect("persist handoff");

        assert!(
            HandoffJournal::load_pending(home.path())
                .await
                .expect("load pending")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn contradictory_preflight_marker_remains_pending() {
        let home = tempdir().expect("temporary home");
        let mut journal = HandoffJournal::begin(
            home.path(),
            "test",
            vec![HandoffNode {
                state: HandoffNodeState::Suspended,
                turn_id: Some("turn".to_string()),
                ..node("contradictory")
            }],
        )
        .await
        .expect("begin handoff");
        journal.transfer_started = Some(false);
        journal.set_state(HandoffJournalState::NeedsAttention);
        journal
            .persist(home.path())
            .await
            .expect("persist contradictory handoff");

        assert_eq!(
            HandoffJournal::load_pending(home.path())
                .await
                .expect("load pending")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn multiple_journals_are_sorted_and_completed_journals_are_harmless() {
        let home = tempdir().expect("temporary home");
        let mut completed = HandoffJournal::begin(home.path(), "test", vec![node("completed")])
            .await
            .expect("begin completed");
        completed.set_state(HandoffJournalState::Completed);
        completed
            .persist(home.path())
            .await
            .expect("persist completed");

        let mut active = HandoffJournal::begin(home.path(), "test", vec![node("active")])
            .await
            .expect("begin active");
        active.mark_transfer_started();
        active.set_state(HandoffJournalState::NeedsAttention);
        active.persist(home.path()).await.expect("persist active");

        let pending = HandoffJournal::load_pending(home.path())
            .await
            .expect("load pending");
        assert_eq!(pending, vec![active]);
        assert!(
            HandoffJournal::load_all(home.path())
                .await
                .expect("load all")
                .iter()
                .any(|journal| journal.state == HandoffJournalState::Completed)
        );
    }
}
