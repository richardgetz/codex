//! Durable handoff receipts and transferability checks for daemon upgrades.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol::JSONRPCMessage;
use serde::Deserialize;
use serde::Serialize;
use tokio::fs;
use tokio::io::AsyncWriteExt;

pub(crate) const MAX_HANDOFF_RESOLUTIONS: usize = 64;

pub(crate) fn retain_latest_handoff_resolutions(resolutions: &mut Vec<HandoffResolution>) {
    let excess = resolutions.len().saturating_sub(MAX_HANDOFF_RESOLUTIONS);
    if excess > 0 {
        resolutions.drain(..excess);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ApplyStatus {
    Applied,
    InProgress,
    NeedsAttention,
    /// The daemon is configured, but is stopped. The selected launcher was saved for its next
    /// start without starting a server as a side effect of applying an update.
    Deferred,
    /// No managed daemon has been configured in this CODEX_HOME.
    NotConfigured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyOutput {
    pub status: ApplyStatus,
    pub handoff_id: Option<String>,
    pub state: Option<String>,
    pub runtime_version: Option<String>,
    pub created_at: Option<i64>,
    pub nodes: Vec<serde_json::Value>,
    pub managed_codex_path: Option<PathBuf>,
    pub managed_codex_version: Option<String>,
    pub running_managed_codex_version: Option<String>,
    pub socket_path: PathBuf,
    pub app_server_version: Option<String>,
    /// Whether an operator explicitly quarantined this unresolved handoff after durable pause.
    pub quarantined: bool,
    /// Whether the saved failure is proven to have stopped before backend replacement and may
    /// be retried by starting a fresh apply attempt.
    pub can_retry: bool,
    /// Whether this unresolved receipt can be sent through the explicit durable quarantine flow.
    pub can_quarantine: bool,
    /// Latest handoff resolution, retained as a compatibility alias for older consumers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_resolution: Option<HandoffResolution>,
    /// Handoffs reconciled during this selected-launcher apply, in completion order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub handoff_resolutions: Vec<HandoffResolution>,
    /// Structured reason a pending handoff prevents automatic apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<ApplyFailureKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffResolution {
    pub handoff_id: String,
    pub outcome: HandoffResolutionOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HandoffResolutionOutcome {
    Recovered,
    RetiredIdleOrphan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ApplyFailureKind {
    HandoffJournalMissing,
    HandoffStorageMismatch,
    HandoffWorkPending,
    RunningLauncherMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ApplyPhase {
    Prepared,
    Starting,
    Recovering,
    Applied,
    NeedsAttention,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HandoffReceipt {
    pub(crate) handoff_id: String,
    pub(crate) state: String,
    pub(crate) runtime_version: String,
    pub(crate) created_at: i64,
    #[serde(default)]
    pub(crate) quarantined: bool,
    /// Whether the coordinator crossed its durable drain boundary. Legacy receipts omit this
    /// marker and remain conservative during automatic recovery.
    #[serde(default)]
    pub(crate) transfer_started: Option<bool>,
    #[serde(default)]
    pub(crate) nodes: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApplyAttemptReceipt {
    pub(crate) handoff: HandoffReceipt,
    pub(crate) phase: ApplyPhase,
    pub(crate) managed_codex_path: PathBuf,
    pub(crate) managed_codex_version: Option<String>,
    /// Server home captured from `initialize` when the handoff was prepared. Older receipts omit
    /// it and must be validated against the live server before they are reconciled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) origin_codex_home: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) handoff_resolution: Option<HandoffResolution>,
    /// Handoffs resolved in this launcher-update operation. Older receipts omit it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) handoff_resolutions: Vec<HandoffResolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) failure_kind: Option<ApplyFailureKind>,
    /// Whether the old backend stop was durably entered. `None` means this receipt predates
    /// the marker and must remain conservative until explicitly resolved.
    #[serde(default)]
    pub(crate) stop_started: Option<bool>,
    /// Whether stopping the previous managed backend completed successfully. Missing or false
    /// values are retried idempotently before replacement starts.
    #[serde(default)]
    pub(crate) stop_completed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failure: Option<String>,
}

impl ApplyAttemptReceipt {
    pub(crate) fn new(
        handoff: HandoffReceipt,
        managed_codex_path: PathBuf,
        managed_codex_version: Option<String>,
    ) -> Self {
        Self {
            handoff,
            phase: ApplyPhase::Prepared,
            managed_codex_path,
            managed_codex_version,
            origin_codex_home: None,
            handoff_resolution: None,
            handoff_resolutions: Vec::new(),
            failure_kind: None,
            stop_started: Some(false),
            stop_completed: Some(false),
            failure: None,
        }
    }

    pub(crate) async fn load(path: &Path) -> Result<Option<Self>> {
        let contents = match fs::read_to_string(path).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read apply receipt {}", path.display()));
            }
        };
        let receipt: Self = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse apply receipt {}", path.display()))?;
        Ok(Some(receipt))
    }

    pub(crate) async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.with_context(|| {
                format!(
                    "failed to create apply receipt directory {}",
                    parent.display()
                )
            })?;
        }
        let mut receipt = self.clone();
        receipt.handoff_resolutions = receipt.handoff_resolution_history();
        receipt.handoff_resolution = receipt.handoff_resolutions.last().cloned();
        let contents =
            serde_json::to_vec_pretty(&receipt).context("failed to serialize apply receipt")?;
        let temporary = path.with_extension("json.tmp");
        let mut temporary_file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .await
            .with_context(|| format!("failed to open apply receipt {}", temporary.display()))?;
        temporary_file
            .write_all(&contents)
            .await
            .with_context(|| format!("failed to write apply receipt {}", temporary.display()))?;
        temporary_file
            .sync_all()
            .await
            .with_context(|| format!("failed to sync apply receipt {}", temporary.display()))?;
        drop(temporary_file);
        fs::rename(&temporary, path)
            .await
            .with_context(|| format!("failed to publish apply receipt {}", path.display()))?;

        // The receipt is the only durable link to the coordinator handoff ID
        // after the old backend is stopped. Flush the containing directory so
        // a crash after the rename cannot lose that link.
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            fs::File::open(parent)
                .await
                .with_context(|| {
                    format!(
                        "failed to open apply receipt directory {}",
                        parent.display()
                    )
                })?
                .sync_all()
                .await
                .with_context(|| {
                    format!(
                        "failed to sync apply receipt directory {}",
                        parent.display()
                    )
                })?;
        }

        Ok(())
    }

    pub(crate) fn handoff_resolution_history(&self) -> Vec<HandoffResolution> {
        let mut history = if self.handoff_resolutions.is_empty() {
            self.handoff_resolution.clone().into_iter().collect()
        } else {
            self.handoff_resolutions.clone()
        };
        retain_latest_handoff_resolutions(&mut history);
        history
    }

    pub(crate) fn is_resolved(&self) -> bool {
        self.phase == ApplyPhase::Applied
    }

    pub(crate) fn should_hold_legacy_owner(&self) -> bool {
        self.phase == ApplyPhase::NeedsAttention
            && self.handoff.state == "needsAttention"
            && self.handoff.transfer_started.is_none()
            && self.stop_completed != Some(true)
            && self.blocks_new_apply()
    }

    pub(crate) fn blocks_new_apply(&self) -> bool {
        if self.is_resolved() {
            return false;
        }
        if self.phase == ApplyPhase::NeedsAttention
            && self.handoff.state == "needsAttention"
            && self.handoff.quarantined
            && self.handoff.is_quarantine_shape()
        {
            return false;
        }
        if self.phase != ApplyPhase::NeedsAttention {
            return true;
        }
        if self.handoff.is_empty_post_transfer_noop() {
            return self.stop_completed != Some(true);
        }
        match self.handoff.transfer_started {
            Some(false) => !self.handoff.is_preparation_failure_shape(),
            Some(true) | None => !self.handoff.is_failed_preparation_shape(),
        }
    }

    pub(crate) fn can_reconcile_empty_orphan(&self) -> bool {
        self.phase != ApplyPhase::Applied
            && self.stop_started == Some(true)
            && self.stop_completed == Some(true)
            && self.handoff.is_empty_post_transfer_noop()
    }

    pub(crate) fn can_retry_preparation(&self) -> bool {
        self.phase == ApplyPhase::NeedsAttention
            && self.handoff.state == "needsAttention"
            && self.handoff.transfer_started == Some(false)
            && !self.handoff.quarantined
            && !self.blocks_new_apply()
    }

    pub(crate) fn output(
        &self,
        socket_path: &Path,
        app_server_version: Option<String>,
        error: Option<String>,
    ) -> ApplyOutput {
        self.output_with_running_version(
            socket_path,
            app_server_version,
            error,
            /*running_managed_codex_version*/ None,
        )
    }

    pub(crate) fn output_with_running_version(
        &self,
        socket_path: &Path,
        app_server_version: Option<String>,
        error: Option<String>,
        running_managed_codex_version: Option<String>,
    ) -> ApplyOutput {
        let handoff_resolutions = self.handoff_resolution_history();
        ApplyOutput {
            status: match self.phase {
                ApplyPhase::Applied => ApplyStatus::Applied,
                ApplyPhase::NeedsAttention => ApplyStatus::NeedsAttention,
                ApplyPhase::Prepared | ApplyPhase::Starting | ApplyPhase::Recovering => {
                    ApplyStatus::InProgress
                }
            },
            handoff_id: Some(self.handoff.handoff_id.clone()),
            state: Some(self.handoff.state.clone()),
            runtime_version: Some(self.handoff.runtime_version.clone()),
            created_at: Some(self.handoff.created_at),
            nodes: self.handoff.nodes.clone(),
            managed_codex_path: Some(self.managed_codex_path.clone()),
            managed_codex_version: self.managed_codex_version.clone(),
            running_managed_codex_version,
            socket_path: socket_path.to_path_buf(),
            app_server_version,
            quarantined: self.handoff.quarantined,
            can_retry: self.phase == ApplyPhase::NeedsAttention && !self.blocks_new_apply(),
            can_quarantine: self.phase == ApplyPhase::NeedsAttention
                && self.handoff.state == "needsAttention"
                && !self.handoff.quarantined
                && self.handoff.can_quarantine()
                && self.blocks_new_apply(),
            handoff_resolution: handoff_resolutions.last().cloned(),
            handoff_resolutions,
            failure_kind: self.failure_kind,
            error: error.or_else(|| self.failure.clone()),
        }
    }
}

impl ApplyOutput {
    pub(crate) fn without_handoff(
        status: ApplyStatus,
        managed_codex_path: Option<PathBuf>,
        managed_codex_version: Option<String>,
        socket_path: &Path,
        error: Option<String>,
    ) -> Self {
        Self {
            status,
            handoff_id: None,
            state: None,
            runtime_version: None,
            created_at: None,
            nodes: Vec::new(),
            managed_codex_path,
            managed_codex_version,
            running_managed_codex_version: None,
            socket_path: socket_path.to_path_buf(),
            app_server_version: None,
            quarantined: false,
            can_retry: false,
            can_quarantine: false,
            handoff_resolution: None,
            handoff_resolutions: Vec::new(),
            failure_kind: None,
            error,
        }
    }
}

impl HandoffReceipt {
    pub(crate) fn is_empty_post_transfer_noop(&self) -> bool {
        self.state == "needsAttention"
            && self.transfer_started == Some(true)
            && self.nodes.is_empty()
    }

    fn is_preparation_failure_shape(&self) -> bool {
        self.state == "needsAttention"
            && !self.nodes.is_empty()
            && self.nodes.iter().all(|node| {
                matches!(
                    node.get("state").and_then(serde_json::Value::as_str),
                    Some("planned" | "needsAttention")
                ) && has_structural_identity(node)
            })
    }

    fn is_failed_preparation_shape(&self) -> bool {
        if self.is_empty_post_transfer_noop() {
            return true;
        }
        self.state == "needsAttention"
            && !self.nodes.is_empty()
            && self.nodes.iter().all(|node| {
                let state = node.get("state").and_then(serde_json::Value::as_str);
                let turn_id = node.get("turnId").and_then(serde_json::Value::as_str);
                let was_running = node
                    .get("wasRunning")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                matches!(state, Some("planned" | "needsAttention"))
                    && has_structural_identity(node)
                    && turn_id.is_none()
                    && !was_running
            })
    }

    fn is_quarantine_shape(&self) -> bool {
        self.state == "needsAttention"
            && !self.nodes.is_empty()
            && self.nodes.iter().all(|node| {
                has_structural_identity(node)
                    && matches!(
                        node.get("state").and_then(serde_json::Value::as_str),
                        Some(
                            "planned"
                                | "suspending"
                                | "suspended"
                                | "recovering"
                                | "restored"
                                | "paused"
                                | "needsAttention"
                                | "notActive"
                        )
                    )
            })
    }

    fn can_quarantine(&self) -> bool {
        self.is_quarantine_shape()
    }
}

fn has_structural_identity(node: &serde_json::Value) -> bool {
    ["threadId", "rootThreadId"].iter().all(|field| {
        node.get(*field)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    }) && node
        .get("parentThreadId")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|value| !value.trim().is_empty())
}

#[derive(Debug)]
pub(crate) struct HandoffRpcError {
    pub(crate) method: String,
    pub(crate) message: String,
    pub(crate) receipt: Option<HandoffReceipt>,
    pub(crate) server_codex_home: Option<PathBuf>,
    pub(crate) codex_home_mismatch: bool,
}

impl std::fmt::Display for HandoffRpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} failed: {}", self.method, self.message)
    }
}

impl HandoffRpcError {
    pub(crate) fn is_codex_home_mismatch(&self) -> bool {
        self.codex_home_mismatch
    }

    pub(crate) fn is_unknown_handoff(&self) -> bool {
        matches!(
            self.method.as_str(),
            "thread/handoff/status" | "thread/handoff/recover"
        ) && self.message.starts_with("unknown handoff id ")
    }
}

impl std::error::Error for HandoffRpcError {}

fn parse_handoff_receipt(value: serde_json::Value) -> Result<HandoffReceipt> {
    let value = value.get("receipt").cloned().unwrap_or(value);
    serde_json::from_value(value).context("handoff response omitted receipt")
}

pub(crate) fn parse_handoff_response(
    message: JSONRPCMessage,
    method: &str,
) -> std::result::Result<HandoffReceipt, HandoffRpcError> {
    match message {
        JSONRPCMessage::Response(response) => {
            parse_handoff_receipt(response.result).map_err(|error| HandoffRpcError {
                method: method.to_string(),
                message: error.to_string(),
                receipt: None,
                server_codex_home: None,
                codex_home_mismatch: false,
            })
        }
        JSONRPCMessage::Error(error) => {
            let receipt = error
                .error
                .data
                .and_then(|data| parse_handoff_receipt(data).ok());
            Err(HandoffRpcError {
                method: method.to_string(),
                message: error.error.message,
                receipt,
                server_codex_home: None,
                codex_home_mismatch: false,
            })
        }
        _ => Err(HandoffRpcError {
            method: method.to_string(),
            message: "unexpected JSON-RPC message".to_string(),
            receipt: None,
            server_codex_home: None,
            codex_home_mismatch: false,
        }),
    }
}

pub(crate) fn ensure_transferable_handoff(receipt: &HandoffReceipt) -> Result<()> {
    anyhow::ensure!(
        receipt.state == "suspended",
        "handoff {} is {}, so the running graph is not safely suspended",
        receipt.handoff_id,
        receipt.state
    );
    for node in &receipt.nodes {
        let thread_id = node
            .get("threadId")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let state = node
            .get("state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let turn_id = node.get("turnId").and_then(serde_json::Value::as_str);
        match state {
            "suspended" if turn_id.is_some_and(|turn_id| !turn_id.is_empty()) => {}
            "notActive" if turn_id.is_none() => {}
            "suspended" => {
                anyhow::bail!("handoff node {thread_id} is suspended without an exact turn id")
            }
            "notActive" => {
                anyhow::bail!("handoff node {thread_id} is notActive but still has a turn id")
            }
            _ => anyhow::bail!("handoff node {thread_id} has non-transferable state {state}"),
        }
    }
    Ok(())
}

pub(crate) fn ensure_handoff_id(expected_id: &str, receipt: &HandoffReceipt) -> Result<()> {
    anyhow::ensure!(
        receipt.handoff_id == expected_id,
        "requested handoff {expected_id}, but the app server returned {}",
        receipt.handoff_id
    );
    Ok(())
}

pub(crate) fn sanitize_failure(error: &str) -> String {
    error
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(512)
        .collect()
}

#[cfg(test)]
#[path = "apply_receipt_tests.rs"]
mod tests;
