use super::super::heuristics;
use super::super::types::CandidateMemoryItem;
use super::super::types::EXPLICIT_CONFIDENCE;
use super::super::types::MemoryBucket;
use super::super::types::MemoryOperation;
use super::super::types::MemorySignal;
use super::*;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[test]
fn extracts_explicit_preferences_from_user_feedback() {
    let candidates =
        heuristics::extract_explicit_preferences("I prefer clarification before delegation.");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].bucket, MemoryBucket::DurablePreference);
    assert_eq!(candidates[0].operation, MemoryOperation::Upsert);
    assert_eq!(candidates[0].signal, MemorySignal::Explicit);
    assert_eq!(
        candidates[0].candidate,
        "Prefer clarification before delegation"
    );
    assert_eq!(candidates[0].key, "prefer clarification before delegation");
}

#[test]
fn detects_explicit_remember_trigger_from_natural_language() {
    let trigger = heuristics::detect_forced_memory_trigger(&[String::from(
        "this is my google calendar invite link can you remember this for future use? https://calendar.app.google/example",
    )]);

    assert_eq!(trigger, Some(heuristics::ForcedMemoryTrigger::Remember));
}

#[test]
fn detects_explicit_remember_trigger_from_should_remember_wording() {
    let trigger =
        heuristics::detect_forced_memory_trigger(&[String::from("you should remember it")]);

    assert_eq!(trigger, Some(heuristics::ForcedMemoryTrigger::Remember));
}

#[test]
fn detects_explicit_forget_trigger_from_natural_language() {
    let trigger = heuristics::detect_forced_memory_trigger(&[String::from(
        "forget this: my old calendar link is no longer valid",
    )]);

    assert_eq!(trigger, Some(heuristics::ForcedMemoryTrigger::Forget));
}

#[test]
fn skips_heuristic_capture_for_memory_intent_so_model_can_extract_payload() {
    let candidates = heuristics::extract_explicit_preferences(
        "this is my google calendar meeting request link. you should remember it.",
    );

    assert!(
        candidates.is_empty(),
        "expected model path fallback, got {candidates:?}"
    );
}

#[test]
fn infers_repeated_steering_from_recent_turns() {
    let candidates = heuristics::extract_repeated_steering_preferences(
        &[
            "Please clarify ambiguity before you delegate work.".to_string(),
            "When things are unclear, clarify before delegating.".to_string(),
            "Clarify first before delegating to other agents.".to_string(),
        ],
        2,
    );

    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.signal == MemorySignal::RepeatedSteering),
        "expected repeated steering candidate, got {candidates:?}"
    );
}

#[test]
fn extracts_acknowledged_preferences_from_assistant_summary() {
    let candidates = heuristics::extract_acknowledged_preferences(
        "Yes-good rule, and I'll treat it as a hard constraint going forward.\n- always branch off the branch you are merging into",
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].bucket, MemoryBucket::DurablePreference);
    assert_eq!(candidates[0].signal, MemorySignal::AssistantAcknowledged);
    assert_eq!(
        candidates[0].candidate,
        "Always branch off the branch you are merging into"
    );
}

#[tokio::test]
async fn consolidates_events_into_summary_and_profile_files() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[CandidateMemoryItem {
            bucket: MemoryBucket::DurablePreference,
            operation: MemoryOperation::Upsert,
            signal: MemorySignal::Explicit,
            key: "clarify before delegation".to_string(),
            candidate: "Prefer clarification before delegation".to_string(),
            source_excerpt: "Remember that I prefer clarification before delegation".to_string(),
            confidence: EXPLICIT_CONFIDENCE,
        }],
    )
    .await
    .expect("append preference event");

    consolidate_preferences(
        &codex_home,
        &OrchestratorMemoryConfig {
            enabled: true,
            ..OrchestratorMemoryConfig::default()
        },
    )
    .await
    .expect("consolidate preferences");

    let summary = fs::read_to_string(summary_path(&codex_home))
        .await
        .expect("read summary");
    let profile = fs::read_to_string(profile_path(&codex_home))
        .await
        .expect("read profile");

    assert!(summary.contains("## Working Preferences"));
    assert!(summary.contains("Prefer clarification before delegation"));
    assert!(profile.contains("observations: 1"));
}

#[tokio::test]
async fn consolidates_followup_state_into_dedicated_section() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[CandidateMemoryItem {
            bucket: MemoryBucket::FollowupState,
            operation: MemoryOperation::Upsert,
            signal: MemorySignal::ModelClassified,
            key: "check staging tags when it is live".to_string(),
            candidate: "Revisit the staging Project and Environment tag audit when staging is live".to_string(),
            source_excerpt: "I'll come back and ask you to check staging again when it actually has it ready to check".to_string(),
            confidence: 0.8,
        }],
    )
    .await
    .expect("append followup event");

    consolidate_preferences(
        &codex_home,
        &OrchestratorMemoryConfig {
            enabled: true,
            ..OrchestratorMemoryConfig::default()
        },
    )
    .await
    .expect("consolidate preferences");

    let summary = fs::read_to_string(summary_path(&codex_home))
        .await
        .expect("read summary");

    assert!(summary.contains("## Follow-Up State"));
    assert!(
        summary
            .contains("Revisit the staging Project and Environment tag audit when staging is live")
    );
}

#[tokio::test]
async fn forget_event_removes_existing_memory_item() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[
            CandidateMemoryItem {
                bucket: MemoryBucket::PersonalContext,
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::ModelClassified,
                key: "my moms name is alice".to_string(),
                candidate: "The user's mom's name is Alice".to_string(),
                source_excerpt: "My mom's name is Alice".to_string(),
                confidence: 0.8,
            },
            CandidateMemoryItem {
                bucket: MemoryBucket::PersonalContext,
                operation: MemoryOperation::Forget,
                signal: MemorySignal::ModelClassified,
                key: "my moms name is alice".to_string(),
                candidate: "The user's mom's name is Alice".to_string(),
                source_excerpt: "Forget this: my mom's name is Alice".to_string(),
                confidence: 0.8,
            },
        ],
    )
    .await
    .expect("append upsert and forget events");

    consolidate_preferences(
        &codex_home,
        &OrchestratorMemoryConfig {
            enabled: true,
            ..OrchestratorMemoryConfig::default()
        },
    )
    .await
    .expect("consolidate preferences");

    assert!(!summary_path(&codex_home).exists());
    assert!(!profile_path(&codex_home).exists());
}

#[tokio::test]
async fn forget_event_removes_semantically_matching_memory_with_different_key() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[
            CandidateMemoryItem {
                bucket: MemoryBucket::FollowupState,
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::ModelClassified,
                key: "user shared a google calendar invite link for future use https calendar app google example booking link keep this for later retrieval when calendar access or scheduling is needed".to_string(),
                candidate: "User shared a Google Calendar invite link for future use: https://calendar.app.google/example-booking-link. Keep this for later retrieval when calendar access or scheduling is needed.".to_string(),
                source_excerpt: "User shared a Google Calendar invite link for future use: https://calendar.app.google/example-booking-link. Keep this for later retrieval when calendar access or scheduling is needed.".to_string(),
                confidence: 0.8,
            },
            CandidateMemoryItem {
                bucket: MemoryBucket::FollowupState,
                operation: MemoryOperation::Forget,
                signal: MemorySignal::ModelClassified,
                key: "user s google calendar invite link https calendar app google example booking link".to_string(),
                candidate: "User's Google Calendar invite link: https://calendar.app.google/example-booking-link".to_string(),
                source_excerpt: "User's Google Calendar invite link: https://calendar.app.google/example-booking-link".to_string(),
                confidence: 0.8,
            },
        ],
    )
    .await
    .expect("append semantic upsert and forget events");

    consolidate_preferences(
        &codex_home,
        &OrchestratorMemoryConfig {
            enabled: true,
            ..OrchestratorMemoryConfig::default()
        },
    )
    .await
    .expect("consolidate preferences");

    assert!(!summary_path(&codex_home).exists());
    assert!(!profile_path(&codex_home).exists());
}
