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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ApplyStatus {
    Applied,
    InProgress,
    NeedsAttention,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyOutput {
    pub status: ApplyStatus,
    pub handoff_id: String,
    pub state: String,
    pub runtime_version: String,
    pub created_at: i64,
    pub nodes: Vec<serde_json::Value>,
    pub managed_codex_path: PathBuf,
    pub managed_codex_version: Option<String>,
    pub socket_path: PathBuf,
    pub app_server_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
    pub(crate) nodes: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApplyAttemptReceipt {
    pub(crate) handoff: HandoffReceipt,
    pub(crate) phase: ApplyPhase,
    pub(crate) managed_codex_path: PathBuf,
    pub(crate) managed_codex_version: Option<String>,
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
                format!("failed to create apply receipt directory {}", parent.display())
            })?;
        }
        let contents =
            serde_json::to_vec_pretty(self).context("failed to serialize apply receipt")?;
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
                    format!("failed to open apply receipt directory {}", parent.display())
                })?
                .sync_all()
                .await
                .with_context(|| {
                    format!("failed to sync apply receipt directory {}", parent.display())
                })?;
        }

        Ok(())
    }

    pub(crate) fn is_resolved(&self) -> bool {
        self.phase == ApplyPhase::Applied
    }

    pub(crate) fn output(
        &self,
        socket_path: &Path,
        app_server_version: Option<String>,
        error: Option<String>,
    ) -> ApplyOutput {
        ApplyOutput {
            status: match self.phase {
                ApplyPhase::Applied => ApplyStatus::Applied,
                ApplyPhase::NeedsAttention => ApplyStatus::NeedsAttention,
                ApplyPhase::Prepared | ApplyPhase::Starting | ApplyPhase::Recovering => {
                    ApplyStatus::InProgress
                }
            },
            handoff_id: self.handoff.handoff_id.clone(),
            state: self.handoff.state.clone(),
            runtime_version: self.handoff.runtime_version.clone(),
            created_at: self.handoff.created_at,
            nodes: self.handoff.nodes.clone(),
            managed_codex_path: self.managed_codex_path.clone(),
            managed_codex_version: self.managed_codex_version.clone(),
            socket_path: socket_path.to_path_buf(),
            app_server_version,
            error: error.or_else(|| self.failure.clone()),
        }
    }
}

#[derive(Debug)]
pub(crate) struct HandoffRpcError {
    pub(crate) method: String,
    pub(crate) message: String,
    pub(crate) receipt: Option<HandoffReceipt>,
}

impl std::fmt::Display for HandoffRpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} failed: {}", self.method, self.message)
    }
}

impl std::error::Error for HandoffRpcError {}

fn parse_handoff_receipt(value: serde_json::Value) -> Result<HandoffReceipt> {
    let value = value
        .get("receipt")
        .cloned()
        .unwrap_or(value);
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
            })
        }
        _ => Err(HandoffRpcError {
            method: method.to_string(),
            message: "unexpected JSON-RPC message".to_string(),
            receipt: None,
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
            "suspended" => anyhow::bail!(
                "handoff node {thread_id} is suspended without an exact turn id"
            ),
            "notActive" => anyhow::bail!(
                "handoff node {thread_id} is notActive but still has a turn id"
            ),
            _ => anyhow::bail!("handoff node {thread_id} has non-transferable state {state}"),
        }
    }
    Ok(())
}

pub(crate) fn sanitize_failure(error: &str) -> String {
    error
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .take(512)
        .collect()
}


#[cfg(test)]
#[path = "apply_receipt_tests.rs"]
mod tests;
