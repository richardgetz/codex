use super::bucket_dir_path;
use super::bucket_events_path;
use super::ensure_layout;
use super::heuristics;
use super::preferences_path;
use super::types::MemoryBucket;
use super::types::MemoryEvent;
use super::types::MemoryOperation;
use super::types::MemorySignal;
use chrono::DateTime;
use chrono::Utc;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use tokio::fs;
use tracing::warn;

const PRE_BUCKET_MIGRATION_BACKUP: &str = "preferences.jsonl.pre-bucket-migration";

#[derive(Debug, Deserialize)]
struct LegacyMemoryEvent {
    observed_at: DateTime<Utc>,
    thread_id: String,
    turn_id: String,
    operation: MemoryOperation,
    signal: MemorySignal,
    key: String,
    candidate: String,
    source_excerpt: String,
    confidence: f32,
}

pub(super) async fn migrate_if_needed(codex_home: &AbsolutePathBuf) -> std::io::Result<()> {
    ensure_layout(codex_home).await?;
    let preferences = preferences_path(codex_home);
    let raw = match fs::read_to_string(&preferences).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    let mut migrated_count = 0usize;
    let mut events = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        if let Ok(event) = serde_json::from_str::<MemoryEvent>(line) {
            events.push(event);
            continue;
        }

        let legacy = match serde_json::from_str::<LegacyMemoryEvent>(line) {
            Ok(legacy) => legacy,
            Err(err) => {
                warn!("skipping unparseable legacy orchestrator memory event: {err}");
                continue;
            }
        };
        migrated_count += 1;
        events.push(MemoryEvent {
            observed_at: legacy.observed_at,
            thread_id: legacy.thread_id,
            turn_id: legacy.turn_id,
            bucket: infer_bucket(&legacy.key, &legacy.candidate, &legacy.source_excerpt),
            operation: legacy.operation,
            signal: legacy.signal,
            key: legacy.key,
            candidate: legacy.candidate,
            source_excerpt: legacy.source_excerpt,
            confidence: legacy.confidence,
        });
    }

    if migrated_count > 0 {
        let backup_path = preferences.with_file_name(PRE_BUCKET_MIGRATION_BACKUP);
        if fs::metadata(&backup_path).await.is_err() {
            fs::write(&backup_path, raw).await?;
        }
        write_preferences(codex_home, &events).await?;
    }

    sync_bucket_files(codex_home, &events).await
}

pub(super) async fn sync_bucket_files_from_raw(
    codex_home: &AbsolutePathBuf,
    raw: &str,
) -> std::io::Result<()> {
    let events = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<MemoryEvent>(line) {
            Ok(event) => Some(event),
            Err(err) => {
                warn!("skipping invalid orchestrator memory event during bucket sync: {err}");
                None
            }
        })
        .collect::<Vec<_>>();
    sync_bucket_files(codex_home, &events).await
}

async fn write_preferences(
    codex_home: &AbsolutePathBuf,
    events: &[MemoryEvent],
) -> std::io::Result<()> {
    let mut body = String::new();
    for event in events {
        body.push_str(
            &serde_json::to_string(event)
                .map_err(|err| std::io::Error::other(format!("serialize memory event: {err}")))?,
        );
        body.push('\n');
    }
    fs::write(preferences_path(codex_home), body).await
}

pub(super) async fn sync_bucket_files(
    codex_home: &AbsolutePathBuf,
    events: &[MemoryEvent],
) -> std::io::Result<()> {
    ensure_layout(codex_home).await?;
    fs::create_dir_all(bucket_dir_path(codex_home)).await?;
    for bucket in MemoryBucket::all() {
        let bucket_events = events
            .iter()
            .filter(|event| event.bucket == *bucket)
            .collect::<Vec<_>>();
        let path = bucket_events_path(codex_home, *bucket);
        if bucket_events.is_empty() {
            if let Err(err) = fs::remove_file(path).await
                && err.kind() != std::io::ErrorKind::NotFound
            {
                return Err(err);
            }
            continue;
        }

        let mut body = String::new();
        for event in bucket_events {
            body.push_str(&serde_json::to_string(event).map_err(|err| {
                std::io::Error::other(format!("serialize bucket memory event: {err}"))
            })?);
            body.push('\n');
        }
        fs::write(path, body).await?;
    }
    Ok(())
}

pub(super) async fn clear_bucket_files(codex_home: &AbsolutePathBuf) -> std::io::Result<()> {
    ensure_layout(codex_home).await?;
    for bucket in MemoryBucket::all() {
        if let Err(err) = fs::remove_file(bucket_events_path(codex_home, *bucket)).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(err);
        }
    }
    Ok(())
}

fn infer_bucket(key: &str, candidate: &str, source_excerpt: &str) -> MemoryBucket {
    let text = heuristics::normalized_key(&format!("{key} {candidate} {source_excerpt}"));

    if contains_any(
        &text,
        &[
            "auth",
            "blocked",
            "guard",
            "mcp",
            "playbook",
            "try",
            "unblock",
            "warming endpoint",
            "workaround",
            "worked",
        ],
    ) && contains_any(&text, &["when", "if", "try", "worked", "unblock"])
    {
        return MemoryBucket::OperatorPlaybook;
    }

    if contains_any(
        &text,
        &[
            "adapt",
            "attunement",
            "emotion",
            "feel",
            "feeling",
            "respond",
            "tone",
            "understand",
        ],
    ) {
        return MemoryBucket::RelationalAttunement;
    }

    if contains_any(
        &text,
        &[
            "come back",
            "follow up",
            "later",
            "next time",
            "revisit",
            "when ready",
        ],
    ) {
        return MemoryBucket::FollowupState;
    }

    if contains_any(
        &text,
        &[
            "ongoing",
            "project",
            "roadmap",
            "scratchpad",
            "thread",
            "working on",
        ],
    ) {
        return MemoryBucket::OngoingThreads;
    }

    if contains_any(
        &text,
        &[
            "family", "friend", "lives", "mom", "name", "personal", "wife",
        ],
    ) {
        return MemoryBucket::PersonalContext;
    }

    MemoryBucket::DurablePreference
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}
