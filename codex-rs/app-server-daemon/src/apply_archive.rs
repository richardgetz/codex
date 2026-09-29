//! Durable history and strict no-work validation for old apply receipts.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use serde::Deserialize;
use serde::Serialize;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::apply_receipt::ApplyAttemptReceipt;
use crate::apply_receipt::HandoffResolution;
use crate::apply_receipt::retain_latest_handoff_resolutions;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ApplyArchiveOutcome {
    Recovered,
    RetiredIdleOrphan,
    RetriedPreparation,
    Quarantined,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchivedApplyAttempt {
    archived_at_unix_ms: u128,
    outcome: ApplyArchiveOutcome,
    receipt: ApplyAttemptReceipt,
}

pub(crate) async fn archive_and_remove(
    history_dir: &Path,
    active_receipt_path: &Path,
    attempt: &ApplyAttemptReceipt,
    outcome: ApplyArchiveOutcome,
) -> Result<()> {
    save_archive_once(history_dir, attempt, outcome).await?;

    match ApplyAttemptReceipt::load(active_receipt_path).await? {
        Some(current) if current == *attempt => {
            fs::remove_file(active_receipt_path)
                .await
                .with_context(|| {
                    format!(
                        "failed to remove archived apply receipt {}",
                        active_receipt_path.display()
                    )
                })?;
            sync_parent(active_receipt_path).await?;
        }
        Some(_) => {
            return Err(anyhow!(
                "active apply receipt changed while archiving handoff {}",
                attempt.handoff.handoff_id
            ));
        }
        None => {}
    }
    Ok(())
}

pub(crate) async fn archive_and_resolve(
    history_dir: &Path,
    active_receipt_path: &Path,
    attempt: &ApplyAttemptReceipt,
    outcome: ApplyArchiveOutcome,
    resolution: HandoffResolution,
    managed_codex_path: &Path,
    managed_codex_version: Option<String>,
) -> Result<ApplyAttemptReceipt> {
    let mut handoff_resolutions = attempt.handoff_resolution_history();
    if let Some(existing) = handoff_resolutions
        .iter()
        .find(|existing| existing.handoff_id == resolution.handoff_id)
    {
        anyhow::ensure!(
            existing == &resolution,
            "handoff {} has conflicting apply resolutions",
            resolution.handoff_id
        );
    } else {
        handoff_resolutions.push(resolution.clone());
    }
    retain_latest_handoff_resolutions(&mut handoff_resolutions);
    if let Some(current) = ApplyAttemptReceipt::load(active_receipt_path).await?
        && current.is_resolved()
        && current.handoff.handoff_id == attempt.handoff.handoff_id
        && current.handoff_resolution.as_ref() == Some(&resolution)
        && current.handoff_resolutions == handoff_resolutions
    {
        let archive_path = history_dir.join(archive_file_name(&attempt.handoff.handoff_id)?);
        let existing = read_archive(&archive_path)
            .await?
            .context("resolved apply receipt is missing its history archive")?;
        anyhow::ensure!(
            existing.outcome == outcome,
            "apply history outcome changed for handoff {}",
            attempt.handoff.handoff_id
        );
        anyhow::ensure!(
            existing.receipt == *attempt
                && existing.receipt.handoff == current.handoff
                && existing.receipt.origin_codex_home == current.origin_codex_home
                && existing.receipt.stop_started == current.stop_started
                && existing.receipt.stop_completed == current.stop_completed,
            "apply history does not match the resolved receipt for handoff {}",
            attempt.handoff.handoff_id
        );
        return Ok(current);
    }

    save_archive_once(history_dir, attempt, outcome).await?;
    let mut resolved = attempt.clone();
    resolved.phase = crate::apply_receipt::ApplyPhase::Applied;
    resolved.handoff_resolution = Some(resolution);
    resolved.handoff_resolutions = handoff_resolutions;
    resolved.managed_codex_path = managed_codex_path.to_path_buf();
    resolved.managed_codex_version = managed_codex_version;
    resolved.failure = None;
    resolved.failure_kind = None;
    resolved.save(active_receipt_path).await?;
    Ok(resolved)
}

async fn save_archive_once(
    history_dir: &Path,
    attempt: &ApplyAttemptReceipt,
    outcome: ApplyArchiveOutcome,
) -> Result<()> {
    let archive_path = history_dir.join(archive_file_name(&attempt.handoff.handoff_id)?);
    let archived = ArchivedApplyAttempt {
        archived_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before Unix epoch")?
            .as_millis(),
        outcome,
        receipt: attempt.clone(),
    };
    if let Some(existing) = read_archive(&archive_path).await? {
        anyhow::ensure!(
            existing.outcome == archived.outcome && existing.receipt == archived.receipt,
            "apply history already contains a different receipt for handoff {}",
            attempt.handoff.handoff_id
        );
    } else {
        save_archive(&archive_path, &archived).await?;
    }
    Ok(())
}

pub(crate) async fn is_proven_idle_orphan(attempt: &ApplyAttemptReceipt) -> Result<bool> {
    if !matches!(
        attempt.phase,
        crate::apply_receipt::ApplyPhase::NeedsAttention
            | crate::apply_receipt::ApplyPhase::Recovering
    ) || attempt.handoff.state != "needsAttention"
        || attempt.handoff.quarantined
        || attempt.handoff.transfer_started != Some(true)
        || attempt.stop_started != Some(true)
        || attempt.stop_completed != Some(true)
    {
        return Ok(false);
    }
    if attempt.handoff.nodes.is_empty() {
        return Ok(true);
    }

    let mut nodes = HashMap::new();
    let mut rollout_paths = Vec::new();
    for node in &attempt.handoff.nodes {
        let Some(thread_id) = nonempty_string(node, "threadId") else {
            return Ok(false);
        };
        let Some(root_thread_id) = nonempty_string(node, "rootThreadId") else {
            return Ok(false);
        };
        let Some(parent) = node.get("parentThreadId") else {
            return Ok(false);
        };
        let parent_thread_id = match parent {
            serde_json::Value::Null => None,
            serde_json::Value::String(parent) if !parent.trim().is_empty() => Some(parent.as_str()),
            _ => return Ok(false),
        };
        if node.get("turnId") != Some(&serde_json::Value::Null)
            || node.get("wasRunning") != Some(&serde_json::Value::Bool(false))
            || node.get("wasPaused") != Some(&serde_json::Value::Bool(false))
        {
            return Ok(false);
        }
        let Some(state) = node.get("state").and_then(serde_json::Value::as_str) else {
            return Ok(false);
        };
        let Some(blockers) = node.get("blockers").and_then(serde_json::Value::as_array) else {
            return Ok(false);
        };
        let blocker_values = blockers
            .iter()
            .map(serde_json::Value::as_str)
            .collect::<Option<Vec<_>>>();
        let Some(blocker_values) = blocker_values else {
            return Ok(false);
        };
        let is_root = parent_thread_id.is_none();
        match (is_root, state, blocker_values.as_slice()) {
            (true, "notActive", []) | (false, "notActive", []) => {}
            (false, "needsAttention", ["parentUnavailable"]) => {}
            _ => return Ok(false),
        }
        let Some(rollout_path) = nonempty_string(node, "rolloutPath") else {
            return Ok(false);
        };
        let rollout_path = PathBuf::from(rollout_path);
        if !rollout_path.is_absolute() {
            return Ok(false);
        }
        if nodes
            .insert(
                thread_id.to_string(),
                (
                    root_thread_id.to_string(),
                    parent_thread_id.map(str::to_string),
                ),
            )
            .is_some()
        {
            return Ok(false);
        }
        rollout_paths.push(rollout_path);
    }

    if !has_closed_rooted_graph(&nodes) {
        return Ok(false);
    }
    for path in rollout_paths {
        let file = match fs::File::open(&path).await {
            Ok(file) => file,
            Err(_) => return Ok(false),
        };
        if !file
            .metadata()
            .await
            .is_ok_and(|metadata| metadata.is_file())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn has_closed_rooted_graph(nodes: &HashMap<String, (String, Option<String>)>) -> bool {
    nodes.iter().all(|(thread_id, (root_id, parent_id))| {
        let Some((root_node_root, root_parent)) = nodes.get(root_id) else {
            return false;
        };
        if root_node_root != root_id || root_parent.is_some() {
            return false;
        }
        let mut current = thread_id.as_str();
        let mut seen = HashSet::new();
        while current != root_id {
            if !seen.insert(current) {
                return false;
            }
            let Some((current_root, Some(parent))) = nodes.get(current) else {
                return false;
            };
            if current_root != root_id {
                return false;
            }
            current = parent;
        }
        if thread_id == root_id {
            parent_id.is_none()
        } else {
            parent_id.is_some()
        }
    })
}

fn nonempty_string<'a>(value: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn archive_file_name(handoff_id: &str) -> Result<String> {
    anyhow::ensure!(
        !handoff_id.is_empty()
            && handoff_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "invalid handoff ID for apply history"
    );
    Ok(format!("{handoff_id}.json"))
}

async fn read_archive(path: &Path) -> Result<Option<ArchivedApplyAttempt>> {
    let contents = match fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read apply history {}", path.display()));
        }
    };
    serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse apply history {}", path.display()))
        .map(Some)
}

async fn save_archive(path: &Path, archived: &ArchivedApplyAttempt) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("apply history path has no parent directory"))?;
    fs::create_dir_all(parent)
        .await
        .with_context(|| format!("failed to create apply history {}", parent.display()))?;
    let contents =
        serde_json::to_vec_pretty(archived).context("failed to serialize apply history")?;
    let temporary = path.with_extension("json.tmp");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)
        .await
        .with_context(|| format!("failed to open apply history {}", temporary.display()))?;
    file.write_all(&contents)
        .await
        .with_context(|| format!("failed to write apply history {}", temporary.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("failed to sync apply history {}", temporary.display()))?;
    drop(file);
    fs::rename(&temporary, path)
        .await
        .with_context(|| format!("failed to publish apply history {}", path.display()))?;
    sync_parent(path).await
}

async fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)
            .await
            .with_context(|| {
                format!(
                    "failed to open apply history directory {}",
                    parent.display()
                )
            })?
            .sync_all()
            .await
            .with_context(|| {
                format!(
                    "failed to sync apply history directory {}",
                    parent.display()
                )
            })?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "apply_archive_tests.rs"]
mod tests;
