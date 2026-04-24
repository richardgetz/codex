use super::*;
use core_test_support::PathExt;
use tempfile::tempdir;

#[test]
fn extracts_explicit_preferences_from_user_feedback() {
    let candidates =
        extract_explicit_preferences("Remember that I prefer clarification before delegation.");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].signal, PreferenceSignal::Explicit);
    assert_eq!(
        candidates[0].candidate,
        "Prefer clarification before delegation"
    );
    assert_eq!(candidates[0].key, "prefer clarification before delegation");
}

#[test]
fn infers_repeated_steering_from_recent_turns() {
    let candidates = extract_repeated_steering_preferences(
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
            .any(|candidate| candidate.signal == PreferenceSignal::RepeatedSteering),
        "expected repeated steering candidate, got {candidates:?}"
    );
}

#[test]
fn extracts_acknowledged_preferences_from_assistant_summary() {
    let candidates = extract_acknowledged_preferences(
        "Yes—good rule, and I'll treat it as a hard constraint going forward.\n- always branch off the branch you are merging into",
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].signal,
        PreferenceSignal::AssistantAcknowledged
    );
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
        &[CandidatePreference {
            signal: PreferenceSignal::Explicit,
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

    assert!(summary.contains("Prefer clarification before delegation"));
    assert!(profile.contains("observations: 1"));
}
