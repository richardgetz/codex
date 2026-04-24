use super::ensure_layout;
use super::model;
use super::preferences_path;
use super::profile_path;
use super::remove_generated_memory_files;
use super::summary_path;
use crate::content_items_to_text;
use crate::context_manager::is_user_turn_boundary;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use chrono::DateTime;
use chrono::Utc;
use codex_config::types::OrchestratorMemoryConfig;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use serde::Deserialize;
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::time::Duration;
use tokio::time::sleep;
use tracing::warn;

const EXPLICIT_CONFIDENCE: f32 = 0.95;
const REPEATED_STEERING_CONFIDENCE: f32 = 0.65;
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "be", "but", "by", "for", "from", "i", "if", "in", "into", "is",
    "it", "me", "my", "of", "on", "or", "so", "that", "the", "things", "to", "we", "when", "with",
    "you", "your",
];
const HIGH_SIGNAL_PREFIXES: &[&str] = &[
    "remember",
    "remember that",
    "i prefer",
    "from now on",
    "always",
    "never",
];
const DIRECTIVE_PREFIXES: &[&str] = &[
    "i want",
    "i expect",
    "you should",
    "you will",
    "don't",
    "do not",
    "focus on",
    "just focus on",
    "skip",
];
const WORKFLOW_TERMS: &[&str] = &[
    "abstraction layer",
    "agent",
    "agents",
    "ambigu",
    "assistant",
    "clarif",
    "communicat",
    "delegat",
    "middle man",
    "orchestrator",
    "preference",
    "session",
    "steer",
    "unclear",
    "workflow",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PreferenceSignal {
    Explicit,
    RepeatedSteering,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PreferenceEvent {
    observed_at: DateTime<Utc>,
    thread_id: String,
    turn_id: String,
    signal: PreferenceSignal,
    key: String,
    candidate: String,
    source_excerpt: String,
    confidence: f32,
}

#[derive(Debug, Clone, PartialEq)]
struct CandidatePreference {
    signal: PreferenceSignal,
    key: String,
    candidate: String,
    source_excerpt: String,
    confidence: f32,
}

#[derive(Debug, Clone)]
struct AggregatedPreference {
    candidate: String,
    observations: usize,
    explicit_observations: usize,
    last_seen: DateTime<Utc>,
    confidence_sum: f32,
}

pub(super) fn schedule_learning(
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    _last_agent_message: Option<String>,
) {
    if matches!(turn_context.session_source, SessionSource::SubAgent(_)) {
        return;
    }

    let weak_session = Arc::downgrade(session);
    let turn_context = Arc::clone(turn_context);
    tokio::spawn(async move {
        let Some(session) = weak_session.upgrade() else {
            return;
        };
        if let Err(err) = process_completed_turn(&session, turn_context.as_ref()).await {
            warn!("failed learning orchestrator memory from completed turn: {err}");
        }
    });
}

async fn process_completed_turn(
    session: &Arc<Session>,
    turn_context: &TurnContext,
) -> std::io::Result<()> {
    let history = session.clone_history().await;
    let current_turn_user_texts = collect_current_turn_user_texts(history.raw_items());
    if current_turn_user_texts.is_empty() {
        return Ok(());
    }

    let has_current_turn_directives = current_turn_user_texts.iter().any(|text| {
        extract_directive_sentences(text)
            .into_iter()
            .next()
            .is_some()
    });
    if !has_current_turn_directives {
        return Ok(());
    }

    let recent_user_turns = collect_recent_user_turns(
        history.raw_items(),
        turn_context.config.orchestrator_memory.recent_turn_window,
    );
    let candidates = extract_candidate_preferences(
        &current_turn_user_texts,
        &recent_user_turns,
        &turn_context.config.orchestrator_memory,
    );
    if candidates.is_empty() {
        return Ok(());
    }

    append_preference_events(
        &turn_context.config.codex_home,
        session.conversation_id.to_string(),
        turn_context.sub_id.clone(),
        &candidates,
    )
    .await?;

    let generation = session
        .services
        .orchestrator_memory_generation
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    let debounce = turn_context.config.orchestrator_memory.debounce_seconds;
    let config = Arc::clone(&turn_context.config);
    let weak_session = Arc::downgrade(session);
    tokio::spawn(async move {
        sleep(Duration::from_secs(debounce)).await;
        let Some(session) = weak_session.upgrade() else {
            return;
        };
        if session
            .services
            .orchestrator_memory_generation
            .load(Ordering::SeqCst)
            != generation
        {
            return;
        }
        if let Err(err) = model::consolidate_with_fallback(&session, &config, generation).await {
            warn!("failed consolidating orchestrator memory: {err}");
        }
    });

    Ok(())
}

async fn append_preference_events(
    codex_home: &codex_utils_absolute_path::AbsolutePathBuf,
    thread_id: String,
    turn_id: String,
    candidates: &[CandidatePreference],
) -> std::io::Result<()> {
    ensure_layout(codex_home).await?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(preferences_path(codex_home))
        .await?;

    let observed_at = Utc::now();
    for candidate in candidates {
        let event = PreferenceEvent {
            observed_at,
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            signal: candidate.signal,
            key: candidate.key.clone(),
            candidate: candidate.candidate.clone(),
            source_excerpt: candidate.source_excerpt.clone(),
            confidence: candidate.confidence,
        };
        let mut line = serde_json::to_string(&event)
            .map_err(|err| std::io::Error::other(format!("serialize preference event: {err}")))?;
        line.push('\n');
        file.write_all(line.as_bytes()).await?;
    }
    file.flush().await
}

pub(crate) async fn consolidate_preferences(
    codex_home: &codex_utils_absolute_path::AbsolutePathBuf,
    config: &OrchestratorMemoryConfig,
) -> std::io::Result<()> {
    ensure_layout(codex_home).await?;
    let raw = match fs::read_to_string(preferences_path(codex_home)).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            remove_generated_memory_files(codex_home).await?;
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    let mut aggregated = HashMap::<String, AggregatedPreference>::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let event: PreferenceEvent = match serde_json::from_str(line) {
            Ok(event) => event,
            Err(err) => {
                warn!("skipping invalid orchestrator memory event: {err}");
                continue;
            }
        };
        let entry = aggregated
            .entry(event.key)
            .or_insert_with(|| AggregatedPreference {
                candidate: event.candidate.clone(),
                observations: 0,
                explicit_observations: 0,
                last_seen: event.observed_at,
                confidence_sum: 0.0,
            });
        entry.observations += 1;
        if event.signal == PreferenceSignal::Explicit {
            entry.explicit_observations += 1;
        }
        if event.observed_at >= entry.last_seen {
            entry.last_seen = event.observed_at;
            entry.candidate = event.candidate;
        }
        entry.confidence_sum += event.confidence;
    }

    let mut preferences = aggregated
        .into_values()
        .filter(|entry| {
            entry.explicit_observations > 0 || entry.observations >= config.min_observations
        })
        .collect::<Vec<_>>();
    preferences.sort_by_key(|entry| {
        (
            Reverse(entry.explicit_observations),
            Reverse(entry.observations),
            Reverse(entry.last_seen),
        )
    });
    preferences.truncate(config.max_summary_items);

    if preferences.is_empty() {
        remove_generated_memory_files(codex_home).await?;
        return Ok(());
    }

    let summary = render_summary(&preferences);
    let profile = render_profile(&preferences);
    fs::write(summary_path(codex_home), summary).await?;
    fs::write(profile_path(codex_home), profile).await?;
    Ok(())
}

fn render_summary(preferences: &[AggregatedPreference]) -> String {
    let mut body = String::from("# Orchestrator Memory Summary\n\n");
    for preference in preferences {
        body.push_str("- ");
        body.push_str(&preference.candidate);
        body.push('\n');
    }
    body
}

fn render_profile(preferences: &[AggregatedPreference]) -> String {
    let mut body = String::from("# Orchestrator Memory Profile\n\n");
    for preference in preferences {
        body.push_str("## ");
        body.push_str(&preference.candidate);
        body.push_str("\n\n");
        body.push_str(&format!(
            "- observations: {}\n- explicit_observations: {}\n- last_seen: {}\n\n",
            preference.observations,
            preference.explicit_observations,
            preference.last_seen.to_rfc3339(),
        ));
    }
    body
}

fn extract_candidate_preferences(
    current_turn_user_texts: &[String],
    recent_user_turns: &[String],
    config: &OrchestratorMemoryConfig,
) -> Vec<CandidatePreference> {
    let mut candidates = current_turn_user_texts
        .iter()
        .flat_map(|text| extract_explicit_preferences(text))
        .collect::<Vec<_>>();

    let repeated =
        extract_repeated_steering_preferences(recent_user_turns, config.min_observations);
    let repeated_keys = repeated
        .iter()
        .map(|candidate| candidate.key.clone())
        .collect::<HashSet<_>>();
    let filtered_repeated = repeated
        .into_iter()
        .filter(|candidate| {
            !candidates
                .iter()
                .any(|existing| existing.key == candidate.key)
        })
        .collect::<Vec<_>>();
    candidates.extend(filtered_repeated);

    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for candidate in candidates {
        if seen.insert((
            candidate.signal,
            candidate.key.clone(),
            candidate.candidate.clone(),
        )) {
            deduped.push(candidate);
        }
    }

    if !repeated_keys.is_empty() {
        deduped.sort_by_key(|candidate| Reverse(candidate.signal == PreferenceSignal::Explicit));
    }

    deduped
}

fn extract_explicit_preferences(text: &str) -> Vec<CandidatePreference> {
    split_sentences(text)
        .into_iter()
        .filter_map(|sentence| extract_explicit_preference(&sentence))
        .collect()
}

fn extract_explicit_preference(sentence: &str) -> Option<CandidatePreference> {
    let sentence = normalize_sentence(sentence);
    if sentence.is_empty() || !looks_like_preference_sentence(&sentence) {
        return None;
    }

    let lowered = sentence.to_lowercase();
    let is_explicit = HIGH_SIGNAL_PREFIXES
        .iter()
        .chain(DIRECTIVE_PREFIXES)
        .any(|prefix| lowered.starts_with(prefix));
    if !is_explicit {
        return None;
    }

    let candidate = candidate_text_from_sentence(&sentence);
    let key = normalized_key(&candidate);
    if key.is_empty() {
        return None;
    }

    Some(CandidatePreference {
        signal: PreferenceSignal::Explicit,
        key,
        candidate,
        source_excerpt: sentence,
        confidence: EXPLICIT_CONFIDENCE,
    })
}

fn extract_repeated_steering_preferences(
    recent_user_turns: &[String],
    min_observations: usize,
) -> Vec<CandidatePreference> {
    let directives = recent_user_turns
        .iter()
        .flat_map(|turn| {
            extract_directive_sentences(turn)
                .into_iter()
                .map(|sentence| {
                    let candidate = candidate_text_from_sentence(&sentence);
                    let tokens = similarity_tokens(&candidate);
                    (sentence, candidate, tokens)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let mut results = Vec::new();
    let mut emitted = HashSet::new();
    for (index, (_sentence, candidate, tokens)) in directives.iter().enumerate() {
        if tokens.is_empty() {
            continue;
        }
        let mut cluster_size = 1usize;
        for (other_index, (_other_sentence, other_candidate, other_tokens)) in
            directives.iter().enumerate()
        {
            if index == other_index || candidate == other_candidate {
                continue;
            }
            if token_similarity(tokens, other_tokens) >= 0.5 {
                cluster_size += 1;
            }
        }
        if cluster_size < min_observations {
            continue;
        }
        let key = normalized_key(candidate);
        if emitted.insert(key.clone()) {
            results.push(CandidatePreference {
                signal: PreferenceSignal::RepeatedSteering,
                key,
                candidate: candidate.clone(),
                source_excerpt: candidate.clone(),
                confidence: REPEATED_STEERING_CONFIDENCE,
            });
        }
    }

    results
}

fn extract_directive_sentences(text: &str) -> Vec<String> {
    split_sentences(text)
        .into_iter()
        .map(|sentence| normalize_sentence(&sentence))
        .filter(|sentence| looks_like_preference_sentence(sentence))
        .collect()
}

fn looks_like_preference_sentence(sentence: &str) -> bool {
    let lowered = sentence.to_lowercase();
    if HIGH_SIGNAL_PREFIXES
        .iter()
        .chain(DIRECTIVE_PREFIXES)
        .any(|prefix| lowered.starts_with(prefix))
    {
        return true;
    }

    let imperative_starts = [
        "clarify", "ask", "focus", "skip", "delegate", "please", "when",
    ];
    WORKFLOW_TERMS.iter().any(|term| lowered.contains(term))
        && (lowered.contains("you")
            || imperative_starts
                .iter()
                .any(|prefix| lowered.starts_with(prefix)))
}

fn candidate_text_from_sentence(sentence: &str) -> String {
    let normalized = normalize_sentence(sentence);
    let lowered = normalized.to_lowercase();
    for prefix in [
        "remember that ",
        "remember ",
        "from now on ",
        "i prefer ",
        "i want ",
        "i expect ",
        "you should ",
        "you will ",
        "just focus on ",
        "focus on ",
        "skip ",
    ] {
        if let Some(rest) = lowered.strip_prefix(prefix) {
            let rest = normalize_sentence(rest);
            return match prefix {
                "i prefer " => format!("Prefer {rest}"),
                "just focus on " | "focus on " => format!("Focus on {rest}"),
                "skip " => format!("Skip {rest}"),
                _ => candidate_text_from_sentence(&rest),
            };
        }
    }
    if let Some(rest) = lowered.strip_prefix("don't ") {
        return format!("Do not {}", normalize_sentence(rest));
    }
    if let Some(rest) = lowered.strip_prefix("do not ") {
        return format!("Do not {}", normalize_sentence(rest));
    }

    capitalize_sentence(&normalized)
}

fn normalized_key(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch.is_whitespace() {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn similarity_tokens(text: &str) -> HashSet<String> {
    normalized_key(text)
        .split_whitespace()
        .filter(|token| token.len() > 2 && !STOPWORDS.contains(token))
        .map(normalize_similarity_token)
        .collect()
}

fn token_similarity(left: &HashSet<String>, right: &HashSet<String>) -> f32 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let overlap = left.intersection(right).count() as f32;
    let max_len = left.len().max(right.len()) as f32;
    overlap / max_len
}

fn normalize_similarity_token(token: &str) -> String {
    let mut normalized = token.to_string();
    for suffix in ["ing", "ed", "es", "s", "ity"] {
        if normalized.len() > suffix.len() + 2 && normalized.ends_with(suffix) {
            normalized.truncate(normalized.len() - suffix.len());
            break;
        }
    }
    normalized
}

fn capitalize_sentence(text: &str) -> String {
    let trimmed = normalize_sentence(text);
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn normalize_sentence(text: &str) -> String {
    text.trim()
        .trim_matches(|ch: char| ch.is_ascii_punctuation() || ch.is_whitespace())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut current = String::new();
    let mut sentences = Vec::new();
    for ch in text.chars() {
        if matches!(ch, '.' | '!' | '?' | '\n') {
            let sentence = normalize_sentence(&current);
            if !sentence.is_empty() {
                sentences.push(sentence);
            }
            current.clear();
        } else {
            current.push(ch);
        }
    }

    let sentence = normalize_sentence(&current);
    if !sentence.is_empty() {
        sentences.push(sentence);
    }
    sentences
}

fn collect_recent_user_turns(items: &[ResponseItem], limit: usize) -> Vec<String> {
    items
        .iter()
        .rev()
        .filter_map(|item| {
            let ResponseItem::Message { role, content, .. } = item else {
                return None;
            };
            if role != "user" || !is_user_turn_boundary(item) {
                return None;
            }
            content_items_to_text(content)
        })
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn collect_current_turn_user_texts(items: &[ResponseItem]) -> Vec<String> {
    let mut collected = Vec::new();
    let mut saw_current_turn_assistant = false;

    for item in items.iter().rev() {
        let ResponseItem::Message { role, content, .. } = item else {
            if saw_current_turn_assistant && !collected.is_empty() {
                continue;
            }
            continue;
        };

        if role == "assistant" {
            if !saw_current_turn_assistant {
                saw_current_turn_assistant = true;
                continue;
            }
            if !collected.is_empty() {
                break;
            }
            continue;
        }

        if role == "user"
            && saw_current_turn_assistant
            && is_user_turn_boundary(item)
            && let Some(text) = content_items_to_text(content)
        {
            collected.push(text);
        }
    }

    collected.into_iter().rev().collect()
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
