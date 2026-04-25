use super::ensure_layout;
use super::profile_path;
use super::summary_path;
use super::types::CandidateMemoryItem;
use super::types::MODEL_CLASSIFIED_CONFIDENCE;
use super::types::MemoryBucket;
use super::types::MemoryOperation;
use super::types::MemorySignal;
use crate::agent::AgentStatus;
use crate::config::Config;
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
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use codex_utils_template::Template;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::fs;
use tracing::warn;

const USER_TURN_TOKEN_LIMIT: usize = 2_500;
const ASSISTANT_TOKEN_LIMIT: usize = 1_500;
const EXISTING_MEMORY_TOKEN_LIMIT: usize = 1_200;
const CLASSIFICATION_TIMEOUT_SECONDS: u64 = 60;

static ORCHESTRATOR_MEMORY_CLASSIFICATION_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    Template::parse(include_str!(
        "../../templates/orchestrator_memory/classify.md"
    ))
    .unwrap_or_else(|err| {
        panic!("embedded template orchestrator_memory/classify.md is invalid: {err}")
    })
});

#[derive(Debug, Deserialize)]
struct ClassificationPayload {
    actions: Vec<ClassificationAction>,
    rationale: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClassificationAction {
    bucket: MemoryBucket,
    operation: MemoryOperation,
    text: String,
}

pub(super) async fn classify_with_model(
    session: &Arc<Session>,
    config: &Arc<Config>,
    current_turn_user_texts: &[String],
    last_agent_message: Option<&str>,
) -> anyhow::Result<Vec<CandidateMemoryItem>> {
    let codex_home = &config.codex_home;
    ensure_layout(codex_home).await?;

    let prompt = build_classification_prompt(
        current_turn_user_texts,
        last_agent_message.unwrap_or_default(),
        &read_existing_memory_file(summary_path(codex_home)).await,
        &read_existing_memory_file(profile_path(codex_home)).await,
    );
    let agent_config = build_classification_agent_config(config)?;
    let source = SessionSource::SubAgent(SubAgentSource::MemoryExtraction);
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
        .context("spawn orchestrator-memory classification agent")?;

    let final_status = wait_for_final_status(&agent_control, thread_id).await;
    if !matches!(final_status, AgentStatus::Shutdown | AgentStatus::NotFound) {
        let agent_control = agent_control.clone();
        tokio::spawn(async move {
            if let Err(err) = agent_control.shutdown_live_agent(thread_id).await {
                warn!(
                    "failed to auto-close orchestrator memory classification agent {thread_id}: {err}"
                );
            }
        });
    }

    match final_status {
        AgentStatus::Completed(message) => {
            let payload = parse_classification_payload(message.as_deref())?;
            Ok(payload
                .actions
                .into_iter()
                .filter_map(|action| {
                    let text = action.text.trim();
                    if text.is_empty() {
                        return None;
                    }
                    Some(CandidateMemoryItem {
                        bucket: action.bucket,
                        operation: action.operation,
                        signal: MemorySignal::ModelClassified,
                        key: super::heuristics::normalized_key(text),
                        candidate: text.to_string(),
                        source_excerpt: text.to_string(),
                        confidence: MODEL_CLASSIFIED_CONFIDENCE,
                    })
                })
                .collect())
        }
        other => anyhow::bail!("orchestrator memory classifier did not complete: {other:?}"),
    }
}

fn build_classification_prompt(
    current_turn_user_texts: &[String],
    last_agent_message: &str,
    existing_summary: &str,
    existing_profile: &str,
) -> String {
    let user_turn = truncate_text(
        &current_turn_user_texts.join("\n\n"),
        TruncationPolicy::Tokens(USER_TURN_TOKEN_LIMIT),
    );
    let assistant = truncate_text(
        last_agent_message,
        TruncationPolicy::Tokens(ASSISTANT_TOKEN_LIMIT),
    );
    let existing_summary = truncate_text(
        existing_summary,
        TruncationPolicy::Tokens(EXISTING_MEMORY_TOKEN_LIMIT),
    );
    let existing_profile = truncate_text(
        existing_profile,
        TruncationPolicy::Tokens(EXISTING_MEMORY_TOKEN_LIMIT),
    );
    ORCHESTRATOR_MEMORY_CLASSIFICATION_TEMPLATE
        .render([
            ("user_turn", user_turn.as_str()),
            ("assistant_message", assistant.as_str()),
            ("existing_summary", existing_summary.as_str()),
            ("existing_profile", existing_profile.as_str()),
        ])
        .unwrap_or_else(|err| {
            warn!("failed rendering orchestrator memory classifier prompt: {err}");
            format!(
                "Classify orchestrator continuity intent.\n\nUser turn:\n{user_turn}\n\nAssistant message:\n{assistant}\n\nExisting summary:\n{existing_summary}\n\nExisting profile:\n{existing_profile}"
            )
        })
}

fn build_classification_agent_config(base: &Arc<Config>) -> anyhow::Result<Config> {
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
        .context("set orchestrator memory classifier sandbox policy")?;
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

async fn wait_for_final_status(
    agent_control: &crate::agent::AgentControl,
    thread_id: codex_protocol::ThreadId,
) -> AgentStatus {
    let Ok(mut rx) = agent_control.subscribe_status(thread_id).await else {
        return AgentStatus::Errored(
            "failed subscribing to orchestrator memory classification agent".to_string(),
        );
    };

    let wait = async {
        loop {
            let status = rx.borrow().clone();
            if crate::agent::status::is_final(&status) {
                return status;
            }
            if rx.changed().await.is_err() {
                return AgentStatus::Errored("status channel closed".to_string());
            }
        }
    };

    match tokio::time::timeout(Duration::from_secs(CLASSIFICATION_TIMEOUT_SECONDS), wait).await {
        Ok(status) => status,
        Err(_) => AgentStatus::Errored(
            "timed out waiting for orchestrator memory classification agent".to_string(),
        ),
    }
}

async fn read_existing_memory_file(path: codex_utils_absolute_path::AbsolutePathBuf) -> String {
    match fs::read_to_string(path).await {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            warn!("failed reading orchestrator memory artifact: {err}");
            String::new()
        }
    }
}

fn parse_classification_payload(message: Option<&str>) -> anyhow::Result<ClassificationPayload> {
    let message = message.unwrap_or_default();
    for candidate in extract_json_candidates(message) {
        if let Ok(payload) = serde_json::from_str::<ClassificationPayload>(&candidate) {
            let _ = &payload.rationale;
            return Ok(payload);
        }
    }
    anyhow::bail!("orchestrator memory classifier did not return valid JSON")
}

fn extract_json_candidates(text: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0
                    && let Some(start_index) = start.take()
                {
                    candidates.push(text[start_index..=index].to_string());
                }
            }
            _ => {}
        }
    }
    candidates.reverse();
    candidates
}
