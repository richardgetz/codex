use super::ensure_layout;
use super::live::aggregate_memory_items;
use super::preferences_path;
use super::profile_path;
use super::remove_generated_memory_files;
use super::summary_path;
use super::types::AggregatedMemoryItem;
use super::types::AggregatedMemorySnapshot;
use crate::agent::AgentStatus;
use crate::config::Config;
use crate::orchestrator_memory::live::consolidate_preferences;
use crate::session::session::Session;
use anyhow::Context;
use codex_config::Constrained;
use codex_features::Feature;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use codex_utils_template::Template;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::fs;
use tracing::warn;

const MAX_EVENT_LINES: usize = 64;
const EVENTS_TOKEN_LIMIT: usize = 4_000;
const EXISTING_MEMORY_TOKEN_LIMIT: usize = 1_500;
const CONSOLIDATION_TIMEOUT_SECONDS: u64 = 120;

static ORCHESTRATOR_MEMORY_CONSOLIDATION_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    Template::parse(include_str!(
        "../../templates/orchestrator_memory/consolidation.md"
    ))
    .unwrap_or_else(|err| {
        panic!("embedded template orchestrator_memory/consolidation.md is invalid: {err}")
    })
});

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct ConsolidationPayload {
    summary_markdown: String,
    profile_markdown: String,
    should_clear: bool,
}

pub(super) async fn consolidate_with_model(
    session: &Arc<Session>,
    config: &Arc<Config>,
    generation: u64,
) -> anyhow::Result<()> {
    let codex_home = &config.codex_home;
    ensure_layout(codex_home).await?;
    let raw_events = match fs::read_to_string(preferences_path(codex_home)).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            remove_generated_memory_files(codex_home).await?;
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };

    let selected_events = select_recent_event_lines(&raw_events);
    if selected_events.trim().is_empty() {
        remove_generated_memory_files(codex_home).await?;
        return Ok(());
    }

    let existing_summary = read_existing_memory_file(summary_path(codex_home)).await;
    let existing_profile = read_existing_memory_file(profile_path(codex_home)).await;
    let prompt = build_consolidation_prompt(
        super::root(codex_home).as_path(),
        &selected_events,
        &existing_summary,
        &existing_profile,
    );

    let agent_config = build_consolidation_agent_config(config)?;
    let source = SessionSource::SubAgent(SubAgentSource::MemoryConsolidation);
    let agent_control = session.services.agent_control.detached_registry();
    let thread_id = agent_control
        .spawn_agent(
            agent_config,
            vec![UserInput::Text {
                text: prompt,
                text_elements: vec![],
            }]
            .into(),
            Some(source),
        )
        .await
        .context("spawn orchestrator-memory consolidation agent")?;

    let final_status = wait_for_final_status(&agent_control, thread_id).await;

    if !matches!(final_status, AgentStatus::Shutdown | AgentStatus::NotFound) {
        let agent_control = agent_control.clone();
        tokio::spawn(async move {
            if let Err(err) = agent_control.shutdown_live_agent(thread_id).await {
                warn!(
                    "failed to auto-close orchestrator memory consolidation agent {thread_id}: {err}"
                );
            }
        });
    }

    if session
        .services
        .orchestrator_memory_generation
        .load(Ordering::SeqCst)
        != generation
    {
        return Ok(());
    }

    match final_status {
        AgentStatus::Completed(message) => {
            let payload = parse_consolidation_payload(message.as_deref())?;
            let payload = apply_heuristic_guarantees(
                payload,
                &aggregate_memory_items(&raw_events, &config.orchestrator_memory),
            );
            write_consolidation_payload(codex_home, payload).await?;
            Ok(())
        }
        other => {
            anyhow::bail!("orchestrator memory consolidation agent did not complete: {other:?}")
        }
    }
}

pub(super) async fn consolidate_with_fallback(
    session: &Arc<Session>,
    config: &Arc<Config>,
    generation: u64,
) -> std::io::Result<()> {
    if let Err(err) = consolidate_with_model(session, config, generation).await {
        warn!(
            "model-assisted orchestrator memory consolidation failed; falling back to heuristic consolidation: {err:?}"
        );
        consolidate_preferences(&config.codex_home, &config.orchestrator_memory).await?;
    }
    Ok(())
}

fn build_consolidation_agent_config(base: &Arc<Config>) -> anyhow::Result<Config> {
    let mut agent_config = (*base.as_ref()).clone();
    let root = super::root(&base.codex_home);

    agent_config.cwd = root.clone();
    agent_config.ephemeral = true;
    agent_config.memories.generate_memories = false;
    agent_config.memories.use_memories = false;
    agent_config.orchestrator_memory.enabled = false;
    agent_config.permissions.approval_policy = Constrained::allow_only(AskForApproval::Never);
    let _ = agent_config.features.disable(Feature::SpawnCsv);
    let _ = agent_config.features.disable(Feature::Collab);
    let _ = agent_config.features.disable(Feature::MemoryTool);

    let sandbox_policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![root],
        read_only_access: Default::default(),
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    agent_config
        .permissions
        .sandbox_policy
        .set(sandbox_policy.clone())
        .context("set orchestrator memory consolidation sandbox policy")?;
    agent_config.permissions.file_system_sandbox_policy =
        FileSystemSandboxPolicy::from_legacy_sandbox_policy(
            &sandbox_policy,
            agent_config.cwd.as_path(),
        );
    agent_config.permissions.network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);

    agent_config.model = base
        .thread_control
        .orchestrator
        .model
        .clone()
        .or_else(|| base.model.clone());
    agent_config.model_reasoning_effort = base
        .thread_control
        .orchestrator
        .reasoning_effort
        .or(base.model_reasoning_effort);

    Ok(agent_config)
}

fn build_consolidation_prompt(
    root: &Path,
    selected_events: &str,
    existing_summary: &str,
    existing_profile: &str,
) -> String {
    let root = root.display().to_string();
    let selected_events = truncate_text(
        selected_events,
        TruncationPolicy::Tokens(EVENTS_TOKEN_LIMIT),
    );
    let existing_summary = truncate_text(
        existing_summary,
        TruncationPolicy::Tokens(EXISTING_MEMORY_TOKEN_LIMIT),
    );
    let existing_profile = truncate_text(
        existing_profile,
        TruncationPolicy::Tokens(EXISTING_MEMORY_TOKEN_LIMIT),
    );
    ORCHESTRATOR_MEMORY_CONSOLIDATION_TEMPLATE
        .render([
            ("memory_root", root.as_str()),
            ("selected_events", selected_events.as_str()),
            ("existing_summary", existing_summary.as_str()),
            ("existing_profile", existing_profile.as_str()),
        ])
        .unwrap_or_else(|err| {
            warn!("failed to render orchestrator memory consolidation prompt template: {err}");
            format!(
                "Consolidate orchestrator memory in {root}\n\nExisting summary:\n{existing_summary}\n\nExisting profile:\n{existing_profile}\n\nRecent preference events:\n{selected_events}"
            )
        })
}

async fn read_existing_memory_file(path: AbsolutePathBuf) -> String {
    match fs::read_to_string(path).await {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            warn!("failed reading orchestrator memory artifact: {err}");
            String::new()
        }
    }
}

fn select_recent_event_lines(raw_events: &str) -> String {
    let mut lines = raw_events
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(MAX_EVENT_LINES)
        .map(str::to_string)
        .collect::<Vec<_>>();
    lines.reverse();
    lines.join("\n")
}

async fn wait_for_final_status(
    agent_control: &crate::agent::AgentControl,
    thread_id: codex_protocol::ThreadId,
) -> AgentStatus {
    let Ok(mut rx) = agent_control.subscribe_status(thread_id).await else {
        return AgentStatus::Errored(
            "failed subscribing to orchestrator memory consolidation agent".to_string(),
        );
    };

    let wait = async {
        loop {
            let status = rx.borrow().clone();
            if crate::agent::status::is_final(&status) {
                return status;
            }
            if rx.changed().await.is_err() {
                return AgentStatus::Errored(
                    "lost status updates for orchestrator memory consolidation agent".to_string(),
                );
            }
        }
    };

    match tokio::time::timeout(Duration::from_secs(CONSOLIDATION_TIMEOUT_SECONDS), wait).await {
        Ok(status) => status,
        Err(_) => AgentStatus::Errored(
            "timed out waiting for orchestrator memory consolidation agent".to_string(),
        ),
    }
}

fn parse_consolidation_payload(text: Option<&str>) -> anyhow::Result<ConsolidationPayload> {
    let Some(text) = text else {
        anyhow::bail!("orchestrator memory consolidation completed without a payload");
    };
    if let Ok(payload) = serde_json::from_str::<ConsolidationPayload>(text) {
        return Ok(payload);
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && start < end
        && let Some(slice) = text.get(start..=end)
    {
        return Ok(serde_json::from_str::<ConsolidationPayload>(slice)?);
    }
    anyhow::bail!("orchestrator memory consolidation payload was not valid JSON")
}

fn apply_heuristic_guarantees(
    mut payload: ConsolidationPayload,
    snapshot: &AggregatedMemorySnapshot,
) -> ConsolidationPayload {
    let has_guaranteed_items = snapshot
        .preferences
        .iter()
        .chain(snapshot.personal_context.iter())
        .chain(snapshot.followups.iter())
        .any(|item| item.direct_observations > 0);

    if !has_guaranteed_items {
        return payload;
    }

    if payload.summary_markdown.trim().is_empty() {
        payload.summary_markdown = "# Orchestrator Memory Summary\n".to_string();
    }
    if payload.profile_markdown.trim().is_empty() {
        payload.profile_markdown = "# Orchestrator Memory Profile\n".to_string();
    }

    append_missing_summary_items(
        &mut payload.summary_markdown,
        "Working Preferences",
        &snapshot.preferences,
    );
    append_missing_summary_items(
        &mut payload.summary_markdown,
        "Personal Context",
        &snapshot.personal_context,
    );
    append_missing_summary_items(
        &mut payload.summary_markdown,
        "Follow-Up State",
        &snapshot.followups,
    );

    append_missing_profile_items(
        &mut payload.profile_markdown,
        "Working Preferences",
        &snapshot.preferences,
    );
    append_missing_profile_items(
        &mut payload.profile_markdown,
        "Personal Context",
        &snapshot.personal_context,
    );
    append_missing_profile_items(
        &mut payload.profile_markdown,
        "Follow-Up State",
        &snapshot.followups,
    );

    payload.should_clear = false;
    payload
}

fn append_missing_summary_items(body: &mut String, title: &str, items: &[AggregatedMemoryItem]) {
    let missing = items
        .iter()
        .filter(|item| item.direct_observations > 0)
        .filter(|item| !body.contains(&item.candidate))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return;
    }

    body.push_str("\n\n## ");
    body.push_str(title);
    body.push('\n');
    for item in missing {
        body.push_str("- ");
        body.push_str(&item.candidate);
        body.push('\n');
    }
}

fn append_missing_profile_items(body: &mut String, title: &str, items: &[AggregatedMemoryItem]) {
    let missing = items
        .iter()
        .filter(|item| item.direct_observations > 0)
        .filter(|item| !body.contains(&item.candidate))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return;
    }

    body.push_str("\n\n## ");
    body.push_str(title);
    body.push_str("\n\n");
    for item in missing {
        body.push_str("### ");
        body.push_str(&item.candidate);
        body.push_str("\n\n");
        body.push_str(&format!(
            "- observations: {}\n- direct_observations: {}\n- last_seen: {}\n\n",
            item.observations,
            item.direct_observations,
            item.last_seen.to_rfc3339(),
        ));
    }
}

async fn write_consolidation_payload(
    codex_home: &AbsolutePathBuf,
    payload: ConsolidationPayload,
) -> std::io::Result<()> {
    if payload.should_clear
        || (payload.summary_markdown.trim().is_empty()
            && payload.profile_markdown.trim().is_empty())
    {
        remove_generated_memory_files(codex_home).await?;
        return Ok(());
    }

    ensure_layout(codex_home).await?;
    fs::write(summary_path(codex_home), payload.summary_markdown).await?;
    fs::write(profile_path(codex_home), payload.profile_markdown).await?;
    Ok(())
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
