use super::super::bucket_events_path;
use super::super::preferences_path;
use super::super::summary_path;
use super::super::types::CandidateMemoryItem;
use super::super::types::EXPLICIT_CONFIDENCE;
use super::*;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[tokio::test]
async fn scheduled_cleanup_compacts_duplicate_raw_events_and_rebuilds_artifacts() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    let config = OrchestratorMemoryConfig {
        cleanup: codex_config::types::OrchestratorMemoryCleanupConfig {
            schedule: "00:00".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };

    super::super::live::append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[
            CandidateMemoryItem {
                bucket: MemoryBucket::DurablePreference,
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::Explicit,
                key: "prefer direct concise answers".to_string(),
                candidate: "Prefer direct concise answers".to_string(),
                source_excerpt: "Remember that I prefer direct concise answers".to_string(),
                confidence: EXPLICIT_CONFIDENCE,
            },
            CandidateMemoryItem {
                bucket: MemoryBucket::DurablePreference,
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::Explicit,
                key: "prefer direct concise answers".to_string(),
                candidate: "Prefer direct concise answers".to_string(),
                source_excerpt: "Remember that I prefer direct concise answers".to_string(),
                confidence: EXPLICIT_CONFIDENCE,
            },
            CandidateMemoryItem {
                bucket: MemoryBucket::OperatorPlaybook,
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::ModelClassified,
                key: "when auth guard stalls try warming endpoint".to_string(),
                candidate: "When auth guard stalls, try the warming endpoint".to_string(),
                source_excerpt: "The warming endpoint unblocked auth".to_string(),
                confidence: 0.8,
            },
        ],
    )
    .await
    .expect("append events");

    run_scheduled_cleanup_if_due(&codex_home, &config)
        .await
        .expect("scheduled cleanup");

    let compacted = fs::read_to_string(preferences_path(&codex_home))
        .await
        .expect("read compacted preferences");
    let mut event_count = 0usize;
    for line in compacted.lines() {
        serde_json::from_str::<MemoryEvent>(line).expect("event");
        event_count += 1;
    }
    let summary = fs::read_to_string(summary_path(&codex_home))
        .await
        .expect("read summary");
    let bucket = fs::read_to_string(bucket_events_path(
        &codex_home,
        MemoryBucket::DurablePreference,
    ))
    .await
    .expect("read bucket mirror");
    let state = read_cleanup_state(&codex_home)
        .await
        .expect("cleanup state");

    assert_eq!(event_count, 2);
    assert_eq!(state.result.raw_events_before, 3);
    assert_eq!(state.result.raw_events_after, 2);
    assert!(summary.contains("Prefer direct concise answers"));
    assert_eq!(bucket.lines().count(), 1);
}

#[tokio::test]
async fn scheduled_cleanup_does_not_rerun_after_current_due_window_completed() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    let config = OrchestratorMemoryConfig {
        cleanup: codex_config::types::OrchestratorMemoryCleanupConfig {
            schedule: "00:00".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };

    super::super::live::append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[CandidateMemoryItem {
            bucket: MemoryBucket::DurablePreference,
            operation: MemoryOperation::Upsert,
            signal: MemorySignal::Explicit,
            key: "prefer direct concise answers".to_string(),
            candidate: "Prefer direct concise answers".to_string(),
            source_excerpt: "Remember that I prefer direct concise answers".to_string(),
            confidence: EXPLICIT_CONFIDENCE,
        }],
    )
    .await
    .expect("append event");
    run_scheduled_cleanup_if_due(&codex_home, &config)
        .await
        .expect("first scheduled cleanup");

    super::super::live::append_preference_events(
        &codex_home,
        "thread-2".to_string(),
        "turn-2".to_string(),
        &[CandidateMemoryItem {
            bucket: MemoryBucket::DurablePreference,
            operation: MemoryOperation::Upsert,
            signal: MemorySignal::Explicit,
            key: "prefer direct concise answers".to_string(),
            candidate: "Prefer direct concise answers".to_string(),
            source_excerpt: "Remember that I prefer direct concise answers".to_string(),
            confidence: EXPLICIT_CONFIDENCE,
        }],
    )
    .await
    .expect("append duplicate after cleanup");

    run_scheduled_cleanup_if_due(&codex_home, &config)
        .await
        .expect("second scheduled cleanup");

    let raw = fs::read_to_string(preferences_path(&codex_home))
        .await
        .expect("read preferences");
    assert_eq!(raw.lines().count(), 2);
}

#[test]
fn compact_events_keeps_recent_forget_tombstones_but_drops_old_ones() {
    let now = Utc::now();
    let recent = MemoryEvent {
        observed_at: now - ChronoDuration::days(3),
        thread_id: "thread-1".to_string(),
        turn_id: "turn-1".to_string(),
        bucket: MemoryBucket::PersonalContext,
        operation: MemoryOperation::Forget,
        signal: MemorySignal::ModelClassified,
        key: "old phone number".to_string(),
        candidate: "Old phone number".to_string(),
        source_excerpt: "Forget the old phone number".to_string(),
        confidence: 0.8,
    };
    let old = MemoryEvent {
        observed_at: now - ChronoDuration::days(60),
        key: "old scheduling link".to_string(),
        candidate: "Old scheduling link".to_string(),
        source_excerpt: "Forget the old scheduling link".to_string(),
        ..recent.clone()
    };
    let raw = [old, recent]
        .into_iter()
        .map(|event| serde_json::to_string(&event).expect("serialize event"))
        .collect::<Vec<_>>()
        .join("\n");

    let compacted = compact_events(&raw, &OrchestratorMemoryConfig::default());

    assert_eq!(compacted.len(), 1);
    assert_eq!(compacted[0].key, "old phone number");
    assert_eq!(compacted[0].operation, MemoryOperation::Forget);
}
