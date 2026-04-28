use super::cleanup_state_path;
use super::ensure_layout;
use super::heuristics;
use super::live::consolidate_preferences;
use super::migration;
use super::model;
use super::preferences_path;
use super::types::MemoryBucket;
use super::types::MemoryEvent;
use super::types::MemoryOperation;
use super::types::MemorySignal;
use crate::config::Config;
use crate::session::session::Session;
use chrono::DateTime;
use chrono::Duration as ChronoDuration;
use chrono::Local;
use chrono::LocalResult;
use chrono::NaiveDate;
use chrono::NaiveTime;
use chrono::TimeZone;
use chrono::Utc;
use codex_config::types::OrchestratorMemoryConfig;
use codex_protocol::protocol::SessionSource;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::fs;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CleanupState {
    last_completed_at: DateTime<Utc>,
    last_due_at: DateTime<Utc>,
    result: CleanupResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CleanupResult {
    pub(crate) raw_events_before: usize,
    pub(crate) raw_events_after: usize,
    pub(crate) removed_raw_events: usize,
}

#[derive(Debug, Clone)]
struct CompactEntry {
    bucket: MemoryBucket,
    key: String,
    candidate: String,
    observations: usize,
    direct_observations: usize,
    last_seen: DateTime<Utc>,
    confidence_sum: f32,
}

#[cfg(test)]
pub(super) async fn run_scheduled_cleanup_if_due(
    codex_home: &codex_utils_absolute_path::AbsolutePathBuf,
    config: &OrchestratorMemoryConfig,
) -> std::io::Result<()> {
    if !config.enabled || !config.cleanup.enabled {
        return Ok(());
    }

    let Some(schedule_time) = parse_schedule_time(&config.cleanup.schedule) else {
        warn!(
            schedule = %config.cleanup.schedule,
            "invalid orchestrator memory cleanup schedule; expected HH:MM"
        );
        return Ok(());
    };
    let now = Local::now();
    let Some(due_at) = latest_due_at(now, schedule_time, config.cleanup.run_missed_on_startup)
    else {
        return Ok(());
    };
    let due_at_utc = due_at.with_timezone(&Utc);
    if let Some(state) = read_cleanup_state(codex_home).await
        && state.last_completed_at >= due_at_utc
    {
        return Ok(());
    }

    let result = run_cleanup_now(codex_home, config).await?;
    write_cleanup_state(
        codex_home,
        &CleanupState {
            last_completed_at: Utc::now(),
            last_due_at: due_at_utc,
            result,
        },
    )
    .await
}

pub(super) fn start_scheduled_cleanup_task(
    session: &Arc<Session>,
    config: &Arc<Config>,
    session_source: &SessionSource,
) {
    if matches!(session_source, SessionSource::SubAgent(_)) {
        return;
    }
    if !config.orchestrator_memory.enabled || !config.orchestrator_memory.cleanup.enabled {
        return;
    }

    let session = Arc::clone(session);
    let config = Arc::clone(config);
    tokio::spawn(async move {
        if let Err(err) = run_scheduled_cleanup_for_session_if_due(&session, &config).await {
            warn!("failed running scheduled orchestrator memory cleanup: {err}");
        }
    });
}

async fn run_scheduled_cleanup_for_session_if_due(
    session: &Arc<Session>,
    config: &Arc<Config>,
) -> std::io::Result<()> {
    let Some(due_at_utc) = due_cleanup_at(&config.codex_home, &config.orchestrator_memory).await?
    else {
        return Ok(());
    };

    let result = run_cleanup_now_with_optional_model(session, config).await?;
    write_cleanup_state(
        &config.codex_home,
        &CleanupState {
            last_completed_at: Utc::now(),
            last_due_at: due_at_utc,
            result,
        },
    )
    .await
}

pub(super) async fn run_cleanup_now_for_session(
    session: &Arc<Session>,
    config: &Arc<Config>,
) -> std::io::Result<CleanupResult> {
    if !config.orchestrator_memory.enabled || !config.orchestrator_memory.cleanup.enabled {
        return Ok(CleanupResult {
            raw_events_before: 0,
            raw_events_after: 0,
            removed_raw_events: 0,
        });
    }

    run_cleanup_now_with_optional_model(session, config).await
}

#[cfg(test)]
async fn run_cleanup_now(
    codex_home: &codex_utils_absolute_path::AbsolutePathBuf,
    config: &OrchestratorMemoryConfig,
) -> std::io::Result<CleanupResult> {
    migration::migrate_if_needed(codex_home).await?;
    ensure_layout(codex_home).await?;
    let preferences = preferences_path(codex_home);
    let raw = match fs::read_to_string(&preferences).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if config.cleanup.deep_consolidation {
                consolidate_preferences(codex_home, config).await?;
            }
            return Ok(CleanupResult {
                raw_events_before: 0,
                raw_events_after: 0,
                removed_raw_events: 0,
            });
        }
        Err(err) => return Err(err),
    };

    let raw_events_before = raw.lines().filter(|line| !line.trim().is_empty()).count();
    let raw_events_after = if config.cleanup.dedupe_raw_events {
        let compacted = compact_events(&raw, config);
        write_events(&preferences, &compacted).await?;
        let compacted_raw = fs::read_to_string(&preferences).await.unwrap_or_default();
        migration::sync_bucket_files_from_raw(codex_home, &compacted_raw).await?;
        compacted.len()
    } else {
        migration::sync_bucket_files_from_raw(codex_home, &raw).await?;
        raw_events_before
    };

    if config.cleanup.deep_consolidation {
        consolidate_preferences(codex_home, config).await?;
    }

    Ok(CleanupResult {
        raw_events_before,
        raw_events_after,
        removed_raw_events: raw_events_before.saturating_sub(raw_events_after),
    })
}

async fn run_cleanup_now_with_optional_model(
    session: &Arc<Session>,
    config: &Arc<Config>,
) -> std::io::Result<CleanupResult> {
    migration::migrate_if_needed(&config.codex_home).await?;
    ensure_layout(&config.codex_home).await?;
    let preferences = preferences_path(&config.codex_home);
    let raw = match fs::read_to_string(&preferences).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if config.orchestrator_memory.cleanup.deep_consolidation {
                consolidate_preferences(&config.codex_home, &config.orchestrator_memory).await?;
            }
            return Ok(CleanupResult {
                raw_events_before: 0,
                raw_events_after: 0,
                removed_raw_events: 0,
            });
        }
        Err(err) => return Err(err),
    };

    let raw_events_before = raw.lines().filter(|line| !line.trim().is_empty()).count();
    let mechanical = if config.orchestrator_memory.cleanup.dedupe_raw_events {
        compact_events(&raw, &config.orchestrator_memory)
    } else {
        parse_valid_events(&raw)
    };

    let events = if config.orchestrator_memory.cleanup.model_consolidation {
        let raw_for_model = events_to_jsonl(&mechanical)?;
        match model::cleanup_events_with_model(session, config, raw_for_model).await {
            Ok(model_events) if !model_events.is_empty() => model_events,
            Ok(_) => mechanical,
            Err(err) => {
                warn!(
                    "model-assisted orchestrator memory cleanup failed; falling back to mechanical cleanup: {err:?}"
                );
                mechanical
            }
        }
    } else {
        mechanical
    };

    write_events(&preferences, &events).await?;
    let compacted_raw = fs::read_to_string(&preferences).await.unwrap_or_default();
    migration::sync_bucket_files_from_raw(&config.codex_home, &compacted_raw).await?;

    if config.orchestrator_memory.cleanup.deep_consolidation {
        consolidate_preferences(&config.codex_home, &config.orchestrator_memory).await?;
    }

    let raw_events_after = events.len();
    Ok(CleanupResult {
        raw_events_before,
        raw_events_after,
        removed_raw_events: raw_events_before.saturating_sub(raw_events_after),
    })
}

fn compact_events(raw: &str, config: &OrchestratorMemoryConfig) -> Vec<MemoryEvent> {
    let now = Utc::now();
    let retain_forget_after =
        now - ChronoDuration::days(config.cleanup.retain_forget_events_days as i64);
    let mut active = HashMap::<(MemoryBucket, String), CompactEntry>::new();
    let mut forgets = Vec::new();

    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let event: MemoryEvent = match serde_json::from_str(line) {
            Ok(event) => event,
            Err(err) => {
                warn!("skipping invalid orchestrator memory event during cleanup: {err}");
                continue;
            }
        };
        match event.operation {
            MemoryOperation::Upsert => upsert_compact_entry(&mut active, event),
            MemoryOperation::Forget => {
                remove_matching_entries(&mut active, &event);
                if event.observed_at >= retain_forget_after {
                    forgets.push(event);
                }
            }
        }
    }

    let mut output = forgets;
    let mut entries = active.into_values().collect::<Vec<_>>();
    entries.sort_by_key(|entry| (entry.bucket.as_str(), entry.key.clone()));
    for entry in entries {
        let event_count = if entry.direct_observations > 0 {
            1
        } else {
            entry.observations.clamp(1, config.min_observations)
        };
        for _ in 0..event_count {
            output.push(MemoryEvent {
                observed_at: entry.last_seen,
                thread_id: "orchestrator-memory-cleanup".to_string(),
                turn_id: "scheduled-cleanup".to_string(),
                bucket: entry.bucket,
                operation: MemoryOperation::Upsert,
                signal: if entry.direct_observations > 0 {
                    MemorySignal::ModelClassified
                } else {
                    MemorySignal::RepeatedSteering
                },
                key: entry.key.clone(),
                candidate: entry.candidate.clone(),
                source_excerpt: entry.candidate.clone(),
                confidence: entry.confidence_sum / entry.observations.max(1) as f32,
            });
        }
    }
    output
}

fn parse_valid_events(raw: &str) -> Vec<MemoryEvent> {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<MemoryEvent>(line) {
            Ok(event) => Some(event),
            Err(err) => {
                warn!("skipping invalid orchestrator memory event during cleanup: {err}");
                None
            }
        })
        .collect()
}

fn events_to_jsonl(events: &[MemoryEvent]) -> std::io::Result<String> {
    let mut body = String::new();
    for event in events {
        body.push_str(&serde_json::to_string(event).map_err(|err| {
            std::io::Error::other(format!("serialize memory event for model cleanup: {err}"))
        })?);
        body.push('\n');
    }
    Ok(body)
}

fn upsert_compact_entry(
    active: &mut HashMap<(MemoryBucket, String), CompactEntry>,
    event: MemoryEvent,
) {
    let key = (event.bucket, event.key.clone());
    let entry = active.entry(key).or_insert_with(|| CompactEntry {
        bucket: event.bucket,
        key: event.key.clone(),
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

fn remove_matching_entries(
    active: &mut HashMap<(MemoryBucket, String), CompactEntry>,
    event: &MemoryEvent,
) {
    if active.remove(&(event.bucket, event.key.clone())).is_some() {
        return;
    }

    let forget_tokens = similarity_tokens(&event.key);
    let forget_candidate_key = heuristics::normalized_key(&event.candidate);
    let forget_candidate_tokens = similarity_tokens(&forget_candidate_key);
    let keys_to_remove = active
        .iter()
        .filter(|((bucket, existing_key), entry)| {
            *bucket == event.bucket
                && keys_match_for_forget(
                    &event.key,
                    existing_key,
                    &entry.candidate,
                    &forget_tokens,
                    &forget_candidate_tokens,
                )
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in keys_to_remove {
        active.remove(&key);
    }
}

fn keys_match_for_forget(
    forget_key: &str,
    existing_key: &str,
    existing_candidate: &str,
    forget_tokens: &std::collections::HashSet<String>,
    forget_candidate_tokens: &std::collections::HashSet<String>,
) -> bool {
    if existing_key == forget_key {
        return true;
    }

    let normalized_candidate = heuristics::normalized_key(existing_candidate);
    if normalized_candidate == forget_key {
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

async fn write_events(
    path: &codex_utils_absolute_path::AbsolutePathBuf,
    events: &[MemoryEvent],
) -> std::io::Result<()> {
    if events.is_empty() {
        if let Err(err) = fs::remove_file(path).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(err);
        }
        return Ok(());
    }

    let mut body = String::new();
    for event in events {
        body.push_str(&serde_json::to_string(event).map_err(|err| {
            std::io::Error::other(format!("serialize compacted memory event: {err}"))
        })?);
        body.push('\n');
    }
    fs::write(path, body).await
}

async fn read_cleanup_state(
    codex_home: &codex_utils_absolute_path::AbsolutePathBuf,
) -> Option<CleanupState> {
    let raw = fs::read_to_string(cleanup_state_path(codex_home))
        .await
        .ok()?;
    serde_json::from_str(&raw).ok()
}

async fn due_cleanup_at(
    codex_home: &codex_utils_absolute_path::AbsolutePathBuf,
    config: &OrchestratorMemoryConfig,
) -> std::io::Result<Option<DateTime<Utc>>> {
    if !config.enabled || !config.cleanup.enabled {
        return Ok(None);
    }

    let Some(schedule_time) = parse_schedule_time(&config.cleanup.schedule) else {
        warn!(
            schedule = %config.cleanup.schedule,
            "invalid orchestrator memory cleanup schedule; expected HH:MM"
        );
        return Ok(None);
    };
    let now = Local::now();
    let Some(due_at) = latest_due_at(now, schedule_time, config.cleanup.run_missed_on_startup)
    else {
        return Ok(None);
    };
    let due_at_utc = due_at.with_timezone(&Utc);
    if let Some(state) = read_cleanup_state(codex_home).await
        && state.last_completed_at >= due_at_utc
    {
        return Ok(None);
    }
    Ok(Some(due_at_utc))
}

async fn write_cleanup_state(
    codex_home: &codex_utils_absolute_path::AbsolutePathBuf,
    state: &CleanupState,
) -> std::io::Result<()> {
    ensure_layout(codex_home).await?;
    let body = serde_json::to_string_pretty(state)
        .map_err(|err| std::io::Error::other(format!("serialize cleanup state: {err}")))?;
    fs::write(cleanup_state_path(codex_home), body).await
}

fn parse_schedule_time(value: &str) -> Option<NaiveTime> {
    let (hour, minute) = value.trim().split_once(':')?;
    let hour = hour.parse::<u32>().ok()?;
    let minute = minute.parse::<u32>().ok()?;
    NaiveTime::from_hms_opt(hour, minute, 0)
}

fn latest_due_at(
    now: DateTime<Local>,
    schedule_time: NaiveTime,
    run_missed_on_startup: bool,
) -> Option<DateTime<Local>> {
    let today = scheduled_at(now.date_naive(), schedule_time)?;
    if now >= today {
        return Some(today);
    }
    if !run_missed_on_startup {
        return None;
    }
    let yesterday = now.date_naive().pred_opt()?;
    scheduled_at(yesterday, schedule_time)
}

fn scheduled_at(date: NaiveDate, schedule_time: NaiveTime) -> Option<DateTime<Local>> {
    match Local.from_local_datetime(&date.and_time(schedule_time)) {
        LocalResult::Single(value) => Some(value),
        LocalResult::Ambiguous(earliest, _) => Some(earliest),
        LocalResult::None => None,
    }
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod tests;
