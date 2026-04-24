use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_config::types::MemoriesScope;
use codex_protocol::config_types::ModeKind;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use codex_utils_template::Template;
use std::sync::Arc;
use std::sync::LazyLock;
use tokio::fs;
use tracing::warn;

#[path = "orchestrator_memory/live.rs"]
mod live;
#[path = "orchestrator_memory/model.rs"]
mod model;

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

pub(crate) async fn build_developer_instructions(codex_home: &AbsolutePathBuf) -> Option<String> {
    let base_path = root(codex_home);
    let (summary_source, summary) = read_summary_source(codex_home).await?;
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

#[cfg(test)]
#[path = "orchestrator_memory_tests.rs"]
mod tests;
