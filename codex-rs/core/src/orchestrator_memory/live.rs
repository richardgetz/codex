use super::append_diagnostic_event;
use super::classifier;
use super::ensure_layout;
use super::heuristics;
use super::preferences_path;
use super::profile_path;
use super::remove_generated_memory_files;
use super::summary_path;
use super::types::AggregatedMemoryItem;
use super::types::AggregatedMemorySnapshot;
use super::types::CandidateMemoryItem;
use super::types::MemoryBucket;
use super::types::MemoryEvent;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use chrono::Utc;
use codex_config::types::OrchestratorMemoryConfig;
use codex_protocol::protocol::SessionSource;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::time::Duration;
use tokio::time::sleep;
use tracing::warn;

pub(super) fn schedule_learning(
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    last_agent_message: Option<String>,
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
        if let Err(err) = process_completed_turn(
            &session,
            turn_context.as_ref(),
            last_agent_message.as_deref(),
        )
        .await
        {
            warn!("failed learning orchestrator memory from completed turn: {err}");
        }
    });
}

async fn process_completed_turn(
    session: &Arc<Session>,
    turn_context: &TurnContext,
    last_agent_message: Option<&str>,
) -> std::io::Result<()> {
    let history = session.clone_history().await;
    append_diagnostic_event(
        &turn_context.config.codex_home,
        "turn_hook_fired",
        &turn_context.sub_id,
        Some("processing completed turn"),
    )
    .await?;

    let current_turn_user_texts = heuristics::collect_current_turn_user_texts(history.raw_items());
    if current_turn_user_texts.is_empty() {
        append_diagnostic_event(
            &turn_context.config.codex_home,
            "skipped_no_user_text",
            &turn_context.sub_id,
            Some("no current-turn user text found"),
        )
        .await?;
        return Ok(());
    }

    let recent_user_turns = heuristics::collect_recent_user_turns(
        history.raw_items(),
        turn_context.config.orchestrator_memory.recent_turn_window,
    );
    let forced_trigger = heuristics::detect_forced_memory_trigger(&current_turn_user_texts);
    if let Some(trigger) = forced_trigger {
        let (stage, details) = match trigger {
            heuristics::ForcedMemoryTrigger::Remember => (
                "forced_memory_trigger",
                "explicit remember/keep/bookmark wording detected",
            ),
            heuristics::ForcedMemoryTrigger::Forget => (
                "forced_forget_trigger",
                "explicit forget/remove wording detected",
            ),
        };
        append_diagnostic_event(
            &turn_context.config.codex_home,
            stage,
            &turn_context.sub_id,
            Some(details),
        )
        .await?;
    }

    let mut candidates = if forced_trigger.is_some() {
        Vec::new()
    } else {
        heuristics::extract_candidate_preferences(
            &current_turn_user_texts,
            &recent_user_turns,
            last_agent_message,
            turn_context.config.orchestrator_memory.min_observations,
        )
    };

    if candidates.is_empty() {
        let classifier_reason = if forced_trigger.is_some() {
            "forced by explicit remember/forget trigger"
        } else {
            "heuristics produced no continuity candidates"
        };
        append_diagnostic_event(
            &turn_context.config.codex_home,
            "running_model_classifier",
            &turn_context.sub_id,
            Some(classifier_reason),
        )
        .await?;
        match classifier::classify_with_model(
            session,
            &turn_context.config,
            &current_turn_user_texts,
            last_agent_message,
        )
        .await
        {
            Ok(classified) => candidates = classified,
            Err(err) => {
                append_diagnostic_event(
                    &turn_context.config.codex_home,
                    "model_classifier_failed",
                    &turn_context.sub_id,
                    Some(&err.to_string()),
                )
                .await?;
            }
        }
    }

    if candidates.is_empty() {
        append_diagnostic_event(
            &turn_context.config.codex_home,
            "skipped_no_signal",
            &turn_context.sub_id,
            Some("no continuity signal or follow-up state detected"),
        )
        .await?;
        return Ok(());
    }
    append_diagnostic_event(
        &turn_context.config.codex_home,
        "extracted_candidates",
        &turn_context.sub_id,
        Some(&format!("candidate_count={}", candidates.len())),
    )
    .await?;

    append_preference_events(
        &turn_context.config.codex_home,
        session.conversation_id.to_string(),
        turn_context.sub_id.clone(),
        &candidates,
    )
    .await?;
    append_diagnostic_event(
        &turn_context.config.codex_home,
        "wrote_events",
        &turn_context.sub_id,
        Some(&format!("event_count={}", candidates.len())),
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
    append_diagnostic_event(
        &turn_context.config.codex_home,
        "scheduled_consolidation",
        &turn_context.sub_id,
        Some(&format!("debounce_seconds={debounce}")),
    )
    .await?;
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
        if let Err(err) =
            super::model::consolidate_with_fallback(&session, &config, generation).await
        {
            warn!("failed consolidating orchestrator memory: {err}");
            let _ = append_diagnostic_event(
                &config.codex_home,
                "consolidation_failed",
                "background",
                Some(&err.to_string()),
            )
            .await;
        } else {
            let _ = append_diagnostic_event(
                &config.codex_home,
                "consolidation_completed",
                "background",
                Some("consolidation finished"),
            )
            .await;
        }
    });

    Ok(())
}

pub(super) async fn append_preference_events(
    codex_home: &codex_utils_absolute_path::AbsolutePathBuf,
    thread_id: String,
    turn_id: String,
    candidates: &[CandidateMemoryItem],
) -> std::io::Result<()> {
    ensure_layout(codex_home).await?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(preferences_path(codex_home))
        .await?;

    let observed_at = Utc::now();
    for candidate in candidates {
        let event = MemoryEvent {
            observed_at,
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            bucket: candidate.bucket,
            operation: candidate.operation,
            signal: candidate.signal,
            key: candidate.key.clone(),
            candidate: candidate.candidate.clone(),
            source_excerpt: candidate.source_excerpt.clone(),
            confidence: candidate.confidence,
        };
        let mut line = serde_json::to_string(&event)
            .map_err(|err| std::io::Error::other(format!("serialize memory event: {err}")))?;
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

    let snapshot = aggregate_memory_items(&raw, config);

    if snapshot.preferences.is_empty()
        && snapshot.personal_context.is_empty()
        && snapshot.followups.is_empty()
    {
        remove_generated_memory_files(codex_home).await?;
        return Ok(());
    }

    let summary = render_summary(
        &snapshot.preferences,
        &snapshot.personal_context,
        &snapshot.followups,
    );
    let profile = render_profile(
        &snapshot.preferences,
        &snapshot.personal_context,
        &snapshot.followups,
    );
    fs::write(summary_path(codex_home), summary).await?;
    fs::write(profile_path(codex_home), profile).await?;
    Ok(())
}

pub(super) fn aggregate_memory_items(
    raw: &str,
    config: &OrchestratorMemoryConfig,
) -> AggregatedMemorySnapshot {
    let mut aggregated = HashMap::<(MemoryBucket, String), AggregatedMemoryItem>::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let event: MemoryEvent = match serde_json::from_str(line) {
            Ok(event) => event,
            Err(err) => {
                warn!("skipping invalid orchestrator memory event: {err}");
                continue;
            }
        };
        let key = (event.bucket, event.key.clone());
        match event.operation {
            super::types::MemoryOperation::Upsert => {
                let entry = aggregated
                    .entry(key)
                    .or_insert_with(|| AggregatedMemoryItem {
                        bucket: event.bucket,
                        candidate: event.candidate.clone(),
                        observations: 0,
                        direct_observations: 0,
                        last_seen: event.observed_at,
                        confidence_sum: 0.0,
                    });
                entry.observations += 1;
                if event.signal.is_direct() {
                    entry.direct_observations += 1;
                }
                if event.observed_at >= entry.last_seen {
                    entry.last_seen = event.observed_at;
                    entry.candidate = event.candidate;
                }
                entry.confidence_sum += event.confidence;
            }
            super::types::MemoryOperation::Forget => {
                remove_matching_memory_entries(&mut aggregated, &event);
            }
        }
    }

    let mut preferences = aggregated
        .values()
        .filter(|entry| entry.bucket == MemoryBucket::DurablePreference)
        .filter(|entry| {
            entry.direct_observations > 0 || entry.observations >= config.min_observations
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut personal_context = aggregated
        .values()
        .filter(|entry| entry.bucket == MemoryBucket::PersonalContext)
        .filter(|entry| {
            entry.direct_observations > 0 || entry.observations >= config.min_observations
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut followups = aggregated
        .values()
        .filter(|entry| entry.bucket == MemoryBucket::FollowupState)
        .filter(|entry| {
            entry.direct_observations > 0 || entry.observations >= config.min_observations
        })
        .cloned()
        .collect::<Vec<_>>();

    sort_entries(&mut preferences);
    sort_entries(&mut personal_context);
    sort_entries(&mut followups);

    preferences.truncate(config.max_summary_items);
    personal_context.truncate(config.max_summary_items);
    followups.truncate(config.max_summary_items);

    AggregatedMemorySnapshot {
        preferences,
        personal_context,
        followups,
    }
}

fn sort_entries(entries: &mut [AggregatedMemoryItem]) {
    entries.sort_by_key(|entry| {
        (
            Reverse(entry.direct_observations),
            Reverse(entry.observations),
            Reverse(entry.last_seen),
        )
    });
}

fn render_summary(
    preferences: &[AggregatedMemoryItem],
    personal_context: &[AggregatedMemoryItem],
    followups: &[AggregatedMemoryItem],
) -> String {
    let mut body = String::from("# Orchestrator Memory Summary\n\n");
    append_summary_section(&mut body, "Working Preferences", preferences);
    append_summary_section(&mut body, "Personal Context", personal_context);
    append_summary_section(&mut body, "Follow-Up State", followups);
    body
}

fn append_summary_section(body: &mut String, title: &str, items: &[AggregatedMemoryItem]) {
    if items.is_empty() {
        return;
    }
    body.push_str("## ");
    body.push_str(title);
    body.push('\n');
    for item in items {
        body.push_str("- ");
        body.push_str(&item.candidate);
        body.push('\n');
    }
    body.push('\n');
}

fn render_profile(
    preferences: &[AggregatedMemoryItem],
    personal_context: &[AggregatedMemoryItem],
    followups: &[AggregatedMemoryItem],
) -> String {
    let mut body = String::from("# Orchestrator Memory Profile\n\n");
    append_profile_section(&mut body, "Working Preferences", preferences);
    append_profile_section(&mut body, "Personal Context", personal_context);
    append_profile_section(&mut body, "Follow-Up State", followups);
    body
}

fn append_profile_section(body: &mut String, title: &str, items: &[AggregatedMemoryItem]) {
    if items.is_empty() {
        return;
    }
    body.push_str("## ");
    body.push_str(title);
    body.push_str("\n\n");
    for item in items {
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

fn remove_matching_memory_entries(
    aggregated: &mut HashMap<(MemoryBucket, String), AggregatedMemoryItem>,
    event: &MemoryEvent,
) {
    let forget_key = (event.bucket, event.key.clone());
    if aggregated.remove(&forget_key).is_some() {
        return;
    }

    let forget_tokens = similarity_tokens(&event.key);
    let forget_candidate_key = heuristics::normalized_key(&event.candidate);
    let forget_candidate_tokens = similarity_tokens(&forget_candidate_key);
    let keys_to_remove = aggregated
        .iter()
        .filter(|((bucket, existing_key), entry)| {
            *bucket == event.bucket
                && keys_match_for_forget(
                    &event.key,
                    &forget_key.1,
                    existing_key,
                    &entry.candidate,
                    &forget_tokens,
                    &forget_candidate_tokens,
                )
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in keys_to_remove {
        aggregated.remove(&key);
    }
}

fn keys_match_for_forget(
    forget_key: &str,
    forget_lookup_key: &str,
    existing_key: &str,
    existing_candidate: &str,
    forget_tokens: &std::collections::HashSet<String>,
    forget_candidate_tokens: &std::collections::HashSet<String>,
) -> bool {
    if existing_key == forget_lookup_key {
        return true;
    }

    let normalized_candidate = heuristics::normalized_key(existing_candidate);
    if normalized_candidate == forget_key || normalized_candidate == forget_lookup_key {
        return true;
    }

    let existing_tokens = similarity_tokens(existing_key);
    let candidate_tokens = similarity_tokens(&normalized_candidate);
    token_similarity(forget_tokens, &existing_tokens) >= 0.6
        || token_similarity(forget_candidate_tokens, &existing_tokens) >= 0.6
        || token_similarity(forget_tokens, &candidate_tokens) >= 0.6
        || token_similarity(forget_candidate_tokens, &candidate_tokens) >= 0.6
        || subset_similarity(forget_tokens, &existing_tokens) >= 0.75
        || subset_similarity(forget_candidate_tokens, &existing_tokens) >= 0.75
        || subset_similarity(forget_tokens, &candidate_tokens) >= 0.75
        || subset_similarity(forget_candidate_tokens, &candidate_tokens) >= 0.75
}

fn similarity_tokens(text: &str) -> std::collections::HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "a",
        "an",
        "and",
        "app",
        "for",
        "future",
        "google",
        "is",
        "it",
        "keep",
        "later",
        "link",
        "of",
        "or",
        "s",
        "scheduling",
        "shared",
        "the",
        "this",
        "to",
        "use",
        "user",
        "when",
    ];

    heuristics::normalized_key(text)
        .split_whitespace()
        .filter(|token| token.len() > 2 && !STOPWORDS.contains(token))
        .map(normalize_similarity_token)
        .collect()
}

fn token_similarity(
    left: &std::collections::HashSet<String>,
    right: &std::collections::HashSet<String>,
) -> f32 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let overlap = left.intersection(right).count() as f32;
    let max_len = left.len().max(right.len()) as f32;
    overlap / max_len
}

fn subset_similarity(
    left: &std::collections::HashSet<String>,
    right: &std::collections::HashSet<String>,
) -> f32 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let overlap = left.intersection(right).count() as f32;
    let min_len = left.len().min(right.len()) as f32;
    overlap / min_len
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

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
