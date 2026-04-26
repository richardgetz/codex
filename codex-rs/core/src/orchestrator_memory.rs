use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_config::types::MemoriesScope;
use codex_config::types::OrchestratorMemoryConfig;
use codex_protocol::config_types::ModeKind;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use codex_utils_template::Template;
use std::sync::Arc;
use std::sync::LazyLock;
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tracing::warn;

#[path = "orchestrator_memory/classifier.rs"]
mod classifier;
#[path = "orchestrator_memory/heuristics.rs"]
mod heuristics;
#[path = "orchestrator_memory/live.rs"]
mod live;
#[path = "orchestrator_memory/model.rs"]
mod model;
#[path = "orchestrator_memory/types.rs"]
mod types;

static ORCHESTRATOR_MEMORY_DEVELOPER_INSTRUCTIONS_TEMPLATE: LazyLock<Template> =
    LazyLock::new(|| {
        Template::parse(include_str!(
            "../templates/orchestrator_memory/read_path.md"
        ))
        .unwrap_or_else(|err| {
            panic!("embedded template orchestrator_memory/read_path.md is invalid: {err}")
        })
    });

const ORCHESTRATOR_MEMORY_SUMMARY_TOKEN_LIMIT: usize = 3_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorMemoryPruneResult {
    pub removed_preference_events: usize,
    pub removed_summary_lines: usize,
    pub removed_profile_lines: usize,
}

pub(crate) fn root(codex_home: &AbsolutePathBuf) -> AbsolutePathBuf {
    codex_home.join("orchestrator_memory")
}

pub(crate) fn summary_path(codex_home: &AbsolutePathBuf) -> AbsolutePathBuf {
    root(codex_home).join("summary.md")
}

pub(crate) fn profile_path(codex_home: &AbsolutePathBuf) -> AbsolutePathBuf {
    root(codex_home).join("profile.md")
}

pub(crate) fn preferences_path(codex_home: &AbsolutePathBuf) -> AbsolutePathBuf {
    root(codex_home).join("preferences.jsonl")
}

pub(crate) fn diagnostics_path(codex_home: &AbsolutePathBuf) -> AbsolutePathBuf {
    root(codex_home).join("diagnostics.jsonl")
}

pub(crate) async fn ensure_layout(
    codex_home: &AbsolutePathBuf,
) -> std::io::Result<AbsolutePathBuf> {
    let root = root(codex_home);
    fs::create_dir_all(&root).await?;
    Ok(root)
}

pub(crate) fn should_use(config: &Config, mode: ModeKind) -> bool {
    config.orchestrator_memory.enabled
        && (config.orchestrator_memory.scope == MemoriesScope::All
            || mode == ModeKind::Orchestrator)
}

pub(crate) async fn build_developer_instructions(
    codex_home: &AbsolutePathBuf,
    config: &OrchestratorMemoryConfig,
) -> Option<String> {
    let base_path = root(codex_home);
    let (summary_source, summary) = read_summary_source(codex_home).await?;
    let summary = with_recent_memory_supplement(codex_home, config, &summary).await;
    let summary = truncate_text(
        &summary,
        TruncationPolicy::Tokens(ORCHESTRATOR_MEMORY_SUMMARY_TOKEN_LIMIT),
    );
    if summary.is_empty() {
        return None;
    }

    let base_path = base_path.display().to_string();
    let summary_source = summary_source.display().to_string();
    ORCHESTRATOR_MEMORY_DEVELOPER_INSTRUCTIONS_TEMPLATE
        .render([
            ("base_path", base_path.as_str()),
            ("summary_source", summary_source.as_str()),
            ("summary", summary.as_str()),
        ])
        .ok()
}

pub(crate) fn maybe_learn_from_completed_turn(
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    last_agent_message: Option<String>,
) {
    if !should_use(
        turn_context.config.as_ref(),
        turn_context.collaboration_mode.mode,
    ) {
        let codex_home = turn_context.config.codex_home.clone();
        let turn_id = turn_context.sub_id.clone();
        tokio::spawn(async move {
            let _ = append_diagnostic_event(
                &codex_home,
                "skipped_gate",
                &turn_id,
                Some("orchestrator memory disabled for current mode/config"),
            )
            .await;
        });
        return;
    }

    live::schedule_learning(session, turn_context, last_agent_message);
}

async fn read_summary_source(codex_home: &AbsolutePathBuf) -> Option<(AbsolutePathBuf, String)> {
    for candidate in [summary_path(codex_home), profile_path(codex_home)] {
        if let Ok(summary) = fs::read_to_string(&candidate).await {
            let summary = summary.trim().to_string();
            if !summary.is_empty() {
                return Some((candidate, summary));
            }
        }
    }

    None
}

async fn with_recent_memory_supplement(
    codex_home: &AbsolutePathBuf,
    config: &OrchestratorMemoryConfig,
    existing_summary: &str,
) -> String {
    let raw = match fs::read_to_string(preferences_path(codex_home)).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return existing_summary.to_string();
        }
        Err(err) => {
            warn!("failed reading orchestrator memory events for developer instructions: {err}");
            return existing_summary.to_string();
        }
    };

    let snapshot = live::aggregate_memory_items(&raw, config);
    let missing_recent_items = snapshot
        .preferences
        .iter()
        .chain(snapshot.personal_context.iter())
        .chain(snapshot.relational_attunement.iter())
        .chain(snapshot.operator_playbook.iter())
        .chain(snapshot.ongoing_threads.iter())
        .chain(snapshot.followups.iter())
        .filter(|item| item.direct_observations > 0)
        .filter(|item| !existing_summary.contains(&item.candidate))
        .map(|item| item.candidate.as_str())
        .collect::<Vec<_>>();

    if missing_recent_items.is_empty() {
        return existing_summary.to_string();
    }

    let mut supplemented = existing_summary.trim().to_string();
    if !supplemented.is_empty() {
        supplemented.push_str("\n\n");
    }
    supplemented.push_str("## Recent Continuity Items\n");
    for candidate in missing_recent_items {
        supplemented.push_str("- ");
        supplemented.push_str(candidate);
        supplemented.push('\n');
    }
    supplemented
}

pub(crate) async fn remove_generated_memory_files(
    codex_home: &AbsolutePathBuf,
) -> std::io::Result<()> {
    for path in [summary_path(codex_home), profile_path(codex_home)] {
        if let Err(err) = fs::remove_file(path).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            warn!("failed removing orchestrator memory artifact: {err}");
        }
    }

    Ok(())
}

pub async fn prune_entries_matching_needle(
    codex_home: &AbsolutePathBuf,
    config: &OrchestratorMemoryConfig,
    needle: &str,
) -> std::io::Result<OrchestratorMemoryPruneResult> {
    ensure_layout(codex_home).await?;
    let needle = needle.trim();
    if needle.is_empty() {
        return Ok(OrchestratorMemoryPruneResult {
            removed_preference_events: 0,
            removed_summary_lines: 0,
            removed_profile_lines: 0,
        });
    }
    let lowered_needle = needle.to_ascii_lowercase();

    let removed_preference_events = prune_matching_lines(
        &preferences_path(codex_home),
        &lowered_needle,
        /*preserve_trailing_newline*/ true,
    )
    .await?;
    let removed_summary_lines = prune_matching_lines(
        &summary_path(codex_home),
        &lowered_needle,
        /*preserve_trailing_newline*/ false,
    )
    .await?;
    let removed_profile_lines = prune_matching_lines(
        &profile_path(codex_home),
        &lowered_needle,
        /*preserve_trailing_newline*/ false,
    )
    .await?;

    if removed_preference_events > 0 {
        live::consolidate_preferences(codex_home, config).await?;
    } else if removed_summary_lines > 0 || removed_profile_lines > 0 {
        let summary = summary_path(codex_home);
        let profile = profile_path(codex_home);
        let summary_missing = !summary.as_path().exists();
        let profile_missing = !profile.as_path().exists();
        if summary_missing && profile_missing {
            remove_generated_memory_files(codex_home).await?;
        }
    }

    Ok(OrchestratorMemoryPruneResult {
        removed_preference_events,
        removed_summary_lines,
        removed_profile_lines,
    })
}

async fn prune_matching_lines(
    path: &AbsolutePathBuf,
    lowered_needle: &str,
    preserve_trailing_newline: bool,
) -> std::io::Result<usize> {
    let raw = match fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };

    let lines = raw.lines().collect::<Vec<_>>();
    let kept = lines
        .iter()
        .filter(|line| !line.to_ascii_lowercase().contains(lowered_needle))
        .copied()
        .collect::<Vec<_>>();
    let removed = lines.len().saturating_sub(kept.len());
    if removed == 0 {
        return Ok(0);
    }

    if kept.is_empty() {
        if let Err(err) = fs::remove_file(path).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(err);
        }
        return Ok(removed);
    }

    let mut rewritten = kept.join("\n");
    if preserve_trailing_newline {
        rewritten.push('\n');
    }
    fs::write(path, rewritten).await?;
    Ok(removed)
}

pub(crate) async fn append_diagnostic_event(
    codex_home: &AbsolutePathBuf,
    stage: &str,
    turn_id: &str,
    details: Option<&str>,
) -> std::io::Result<()> {
    ensure_layout(codex_home).await?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(diagnostics_path(codex_home))
        .await?;
    let payload = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "stage": stage,
        "turn_id": turn_id,
        "details": details.unwrap_or_default(),
    });
    let mut line = serde_json::to_string(&payload)
        .map_err(|err| std::io::Error::other(format!("serialize diagnostics event: {err}")))?;
    line.push('\n');
    file.write_all(line.as_bytes()).await?;
    file.flush().await
}

#[cfg(test)]
#[path = "orchestrator_memory_tests.rs"]
mod tests;
