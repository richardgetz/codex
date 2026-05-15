use super::super::bucket_events_path;
use super::super::heuristics;
use super::super::types::CandidateMemoryItem;
use super::super::types::EXPLICIT_CONFIDENCE;
use super::super::types::MemoryBucket;
use super::super::types::MemoryEvent;
use super::super::types::MemoryOperation;
use super::super::types::MemoryScope;
use super::super::types::MemorySignal;
use super::*;
use crate::session::SessionSettingsUpdate;
use codex_config::types::UserPreferencesMemoryBucket;
use codex_config::types::UserPreferencesMemoryBucketPolicy;
use codex_protocol::config_types::MemoryAccessPolicy;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::Ordering;
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
        /*min_observations*/ 2,
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

#[test]
fn infers_process_scope_for_pull_request_guidance() {
    let scope = heuristics::inferred_scope_for_text(
        "When a PR is failing after Rick steps away, fix branch issues and merge only when authorized.",
    );

    assert_eq!(
        scope,
        MemoryScope::process("pull_request_workflow", "pull request or merge workflow")
    );
}

#[test]
fn infers_generic_review_scope_without_treating_review_as_pull_request() {
    let scope = heuristics::inferred_scope_for_text(
        "When I ask for a code review, prioritize behavioral bugs and missing tests.",
    );

    assert_eq!(
        scope,
        MemoryScope::process("review_workflow", "review workflow")
    );
}

#[test]
fn infers_repo_scope_for_repo_specific_heuristic_candidates() {
    let mut candidates = vec![CandidateMemoryItem {
        bucket: MemoryBucket::DurablePreference,
        scope: MemoryScope::global(),
        operation: MemoryOperation::Upsert,
        signal: MemorySignal::Explicit,
        key: "mobius repo cli first".to_string(),
        candidate: "When working in the Mobius repo, keep it CLI-first".to_string(),
        source_excerpt: "When working in the Mobius repo, keep it CLI-first".to_string(),
        confidence: EXPLICIT_CONFIDENCE,
    }];

    apply_inferred_scopes(&mut candidates);

    assert_eq!(
        candidates[0].scope,
        MemoryScope::new(
            super::super::types::MemoryScopeKind::Repo,
            "mobius",
            "repo-specific wording",
            /*confidence*/ 0.75,
        )
    );
}

#[test]
fn leaves_global_check_preferences_global() {
    let mut candidates = vec![CandidateMemoryItem {
        bucket: MemoryBucket::DurablePreference,
        scope: MemoryScope::global(),
        operation: MemoryOperation::Upsert,
        signal: MemorySignal::Explicit,
        key: "check memory before answering".to_string(),
        candidate: "Always check memory before answering".to_string(),
        source_excerpt: "Always check memory before answering".to_string(),
        confidence: EXPLICIT_CONFIDENCE,
    }];

    apply_inferred_scopes(&mut candidates);

    assert_eq!(candidates[0].scope, MemoryScope::global());
}

#[test]
fn leaves_global_words_containing_ci_global() {
    let mut candidates = vec![CandidateMemoryItem {
        bucket: MemoryBucket::DurablePreference,
        scope: MemoryScope::global(),
        operation: MemoryOperation::Upsert,
        signal: MemorySignal::Explicit,
        key: "prefer concise answers".to_string(),
        candidate: "Prefer concise answers".to_string(),
        source_excerpt: "Prefer concise answers".to_string(),
        confidence: EXPLICIT_CONFIDENCE,
    }];

    apply_inferred_scopes(&mut candidates);

    assert_eq!(candidates[0].scope, MemoryScope::global());
}

#[test]
fn does_not_treat_preview_as_review_scope() {
    let mut candidates = vec![CandidateMemoryItem {
        bucket: MemoryBucket::DurablePreference,
        scope: MemoryScope::global(),
        operation: MemoryOperation::Upsert,
        signal: MemorySignal::Explicit,
        key: "prefer previewing docs".to_string(),
        candidate: "Prefer previewing docs before finalizing".to_string(),
        source_excerpt: "Prefer previewing docs before finalizing".to_string(),
        confidence: EXPLICIT_CONFIDENCE,
    }];

    apply_inferred_scopes(&mut candidates);

    assert_eq!(candidates[0].scope, MemoryScope::global());
}

#[test]
fn repo_marker_takes_precedence_over_workflow_scope() {
    let mut candidates = vec![CandidateMemoryItem {
        bucket: MemoryBucket::OperatorPlaybook,
        scope: MemoryScope::global(),
        operation: MemoryOperation::Upsert,
        signal: MemorySignal::Explicit,
        key: "mobius pr workflow".to_string(),
        candidate: "When working in the Mobius repo on PRs, keep it CLI-first".to_string(),
        source_excerpt: "When working in the Mobius repo on PRs, keep it CLI-first".to_string(),
        confidence: EXPLICIT_CONFIDENCE,
    }];

    apply_inferred_scopes(&mut candidates);

    assert_eq!(
        candidates[0].scope,
        MemoryScope::new(
            super::super::types::MemoryScopeKind::Repo,
            "mobius",
            "repo-specific wording",
            /*confidence*/ 0.75,
        )
    );
}

#[test]
fn preserves_hyphenated_repo_scope_ids() {
    let mut candidates = vec![CandidateMemoryItem {
        bucket: MemoryBucket::OperatorPlaybook,
        scope: MemoryScope::global(),
        operation: MemoryOperation::Upsert,
        signal: MemorySignal::Explicit,
        key: "video brand safety pr workflow".to_string(),
        candidate: "When working in the video-brand-safety repo on PRs, keep it CLI-first"
            .to_string(),
        source_excerpt: "When working in the video-brand-safety repo on PRs, keep it CLI-first"
            .to_string(),
        confidence: EXPLICIT_CONFIDENCE,
    }];

    apply_inferred_scopes(&mut candidates);

    assert_eq!(
        candidates[0].scope,
        MemoryScope::new(
            super::super::types::MemoryScopeKind::Repo,
            "video-brand-safety",
            "repo-specific wording",
            /*confidence*/ 0.75,
        )
    );
}

#[test]
fn keeps_scope_sensitive_heuristic_candidates_when_scope_can_be_inferred() {
    let mut candidates = vec![CandidateMemoryItem {
        bucket: MemoryBucket::OperatorPlaybook,
        scope: MemoryScope::global(),
        operation: MemoryOperation::Upsert,
        signal: MemorySignal::Explicit,
        key: "pr merge authorization".to_string(),
        candidate: "When babysitting PR checks, merge only when authorized".to_string(),
        source_excerpt: "When babysitting PR checks, merge only when authorized".to_string(),
        confidence: EXPLICIT_CONFIDENCE,
    }];

    apply_inferred_scopes(&mut candidates);

    assert_eq!(
        candidates[0].scope,
        MemoryScope::process("pull_request_workflow", "pull request or merge workflow")
    );
}

#[test]
fn write_policy_removes_candidates_for_disallowed_buckets() {
    let mut candidates = vec![
        CandidateMemoryItem {
            bucket: MemoryBucket::DurablePreference,
            scope: MemoryScope::global(),
            operation: MemoryOperation::Upsert,
            signal: MemorySignal::Explicit,
            key: "direct updates".to_string(),
            candidate: "Prefer direct implementation updates".to_string(),
            source_excerpt: "remember direct updates".to_string(),
            confidence: EXPLICIT_CONFIDENCE,
        },
        CandidateMemoryItem {
            bucket: MemoryBucket::PersonalContext,
            scope: MemoryScope::global(),
            operation: MemoryOperation::Upsert,
            signal: MemorySignal::ModelClassified,
            key: "private context".to_string(),
            candidate: "Private personal context item".to_string(),
            source_excerpt: "private".to_string(),
            confidence: 0.8,
        },
    ];
    let policy = UserPreferencesMemoryBucketPolicy {
        read_buckets: UserPreferencesMemoryBucket::all().to_vec(),
        write_buckets: vec![UserPreferencesMemoryBucket::DurablePreference],
    };

    retain_write_allowed_candidates(&mut candidates, &policy);

    assert_eq!(
        candidates,
        vec![CandidateMemoryItem {
            bucket: MemoryBucket::DurablePreference,
            scope: MemoryScope::global(),
            operation: MemoryOperation::Upsert,
            signal: MemorySignal::Explicit,
            key: "direct updates".to_string(),
            candidate: "Prefer direct implementation updates".to_string(),
            source_excerpt: "remember direct updates".to_string(),
            confidence: EXPLICIT_CONFIDENCE,
        }]
    );
}

#[tokio::test]
async fn scheduled_consolidation_skips_when_memory_writes_are_disabled() {
    let (session, _turn_context) = crate::session::tests::make_session_and_context().await;
    session
        .services
        .orchestrator_memory_generation
        .store(1, Ordering::SeqCst);

    assert_eq!(
        scheduled_consolidation_skip_reason(&session, /*generation*/ 1).await,
        None
    );
    assert_eq!(
        scheduled_consolidation_skip_reason(&session, /*generation*/ 2).await,
        Some(ScheduledConsolidationSkipReason::StaleGeneration)
    );

    session
        .update_settings(SessionSettingsUpdate {
            memory_policy: Some(MemoryAccessPolicy::new(
                /*read*/ false, /*write*/ false,
            )),
            ..Default::default()
        })
        .await
        .expect("memory policy update should apply");

    assert_eq!(
        scheduled_consolidation_skip_reason(&session, /*generation*/ 1).await,
        Some(ScheduledConsolidationSkipReason::WriteDisabled)
    );
}

#[tokio::test]
async fn completed_turn_processing_does_not_create_memory_files_when_writes_disabled() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    session
        .update_settings(SessionSettingsUpdate {
            memory_policy: Some(MemoryAccessPolicy::new(
                /*read*/ false, /*write*/ false,
            )),
            ..Default::default()
        })
        .await
        .expect("memory policy update should apply");
    let codex_home = turn_context.config.codex_home.clone();
    let session = Arc::new(session);

    process_completed_turn(&session, &turn_context, /*last_agent_message*/ None)
        .await
        .expect("completed turn processing should no-op");

    assert!(!super::super::root(&codex_home).exists());
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
            scope: MemoryScope::global(),
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
async fn scoped_events_render_scope_and_do_not_merge_with_global_events() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[
            CandidateMemoryItem {
                bucket: MemoryBucket::OperatorPlaybook,
                scope: MemoryScope::global(),
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::ModelClassified,
                key: "merge only when authorized".to_string(),
                candidate: "Never merge without explicit authorization".to_string(),
                source_excerpt: "never merge without explicit authorization".to_string(),
                confidence: 0.8,
            },
            CandidateMemoryItem {
                bucket: MemoryBucket::OperatorPlaybook,
                scope: MemoryScope::process(
                    "pull_request_workflow",
                    "pull request monitoring workflow",
                ),
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::ModelClassified,
                key: "merge only when authorized".to_string(),
                candidate: "When babysitting pull requests, merge only when Rick has authorized merging".to_string(),
                source_excerpt: "When a PR is failing after Rick steps away, handle fixes and merge only when authorized".to_string(),
                confidence: 0.8,
            },
        ],
    )
    .await
    .expect("append scoped events");

    consolidate_preferences(&codex_home, &OrchestratorMemoryConfig::default())
        .await
        .expect("consolidate preferences");

    let summary = fs::read_to_string(summary_path(&codex_home))
        .await
        .expect("read summary");
    let profile = fs::read_to_string(profile_path(&codex_home))
        .await
        .expect("read profile");

    assert!(summary.contains("Never merge without explicit authorization"));
    assert!(
        summary.contains(
            "[process:pull_request_workflow] When babysitting pull requests, merge only when Rick has authorized merging"
        )
    );
    assert!(profile.contains("- scope: process:pull_request_workflow"));
    assert!(profile.contains("- scope_evidence: pull request monitoring workflow"));
}

#[tokio::test]
async fn global_forget_removes_matching_global_and_scoped_entries() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[
            CandidateMemoryItem {
                bucket: MemoryBucket::OperatorPlaybook,
                scope: MemoryScope::global(),
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::ModelClassified,
                key: "merge only when authorized".to_string(),
                candidate: "Merge only when authorized".to_string(),
                source_excerpt: "merge only when authorized".to_string(),
                confidence: 0.8,
            },
            CandidateMemoryItem {
                bucket: MemoryBucket::OperatorPlaybook,
                scope: MemoryScope::process(
                    "pull_request_workflow",
                    "pull request monitoring workflow",
                ),
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::ModelClassified,
                key: "merge only when authorized".to_string(),
                candidate: "Merge only when authorized".to_string(),
                source_excerpt: "merge only when authorized for PRs".to_string(),
                confidence: 0.8,
            },
            CandidateMemoryItem {
                bucket: MemoryBucket::OperatorPlaybook,
                scope: MemoryScope::global(),
                operation: MemoryOperation::Forget,
                signal: MemorySignal::Explicit,
                key: "merge only when authorized".to_string(),
                candidate: "Merge only when authorized".to_string(),
                source_excerpt: "forget merge only when authorized".to_string(),
                confidence: EXPLICIT_CONFIDENCE,
            },
        ],
    )
    .await
    .expect("append events");

    consolidate_preferences(&codex_home, &OrchestratorMemoryConfig::default())
        .await
        .expect("consolidate preferences");

    assert!(!summary_path(&codex_home).exists());
}

#[tokio::test]
async fn append_writes_bucket_specific_event_mirrors() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[CandidateMemoryItem {
            bucket: MemoryBucket::OperatorPlaybook,
            scope: MemoryScope::global(),
            operation: MemoryOperation::Upsert,
            signal: MemorySignal::ModelClassified,
            key: "when auth guard stalls try warming endpoint".to_string(),
            candidate: "When auth guard stalls, try the warming endpoint".to_string(),
            source_excerpt: "The warming endpoint unblocked auth".to_string(),
            confidence: 0.8,
        }],
    )
    .await
    .expect("append operator playbook event");

    let bucket_raw = fs::read_to_string(bucket_events_path(
        &codex_home,
        MemoryBucket::OperatorPlaybook,
    ))
    .await
    .expect("read operator playbook bucket file");
    let events = bucket_raw
        .lines()
        .map(|line| serde_json::from_str::<MemoryEvent>(line).expect("bucket event"))
        .collect::<Vec<_>>();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].bucket, MemoryBucket::OperatorPlaybook);
    assert_eq!(
        events[0].candidate,
        "When auth guard stalls, try the warming endpoint"
    );
}

#[tokio::test]
async fn consolidation_migrates_legacy_events_into_buckets() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    ensure_layout(&codex_home).await.expect("layout");
    let legacy = serde_json::json!({
        "observed_at": "2026-04-26T12:00:00Z",
        "thread_id": "thread-1",
        "turn_id": "turn-1",
        "operation": "upsert",
        "signal": "model_classified",
        "key": "when aws auth guard stalls try warming endpoint",
        "candidate": "When aws-auth-guard authentication stalls, try the warming endpoint to unblock it",
        "source_excerpt": "The user suggested the warming endpoint and it worked",
        "confidence": 0.8
    });
    fs::write(preferences_path(&codex_home), format!("{legacy}\n"))
        .await
        .expect("write legacy event");

    consolidate_preferences(
        &codex_home,
        &OrchestratorMemoryConfig {
            enabled: true,
            ..OrchestratorMemoryConfig::default()
        },
    )
    .await
    .expect("consolidate preferences");

    let migrated_raw = fs::read_to_string(preferences_path(&codex_home))
        .await
        .expect("read migrated preferences");
    let migrated =
        serde_json::from_str::<MemoryEvent>(migrated_raw.lines().next().expect("migrated event"))
            .expect("parse migrated event");
    let bucket_raw = fs::read_to_string(bucket_events_path(
        &codex_home,
        MemoryBucket::OperatorPlaybook,
    ))
    .await
    .expect("read operator playbook bucket file");
    let summary = fs::read_to_string(summary_path(&codex_home))
        .await
        .expect("read summary");

    assert_eq!(migrated.bucket, MemoryBucket::OperatorPlaybook);
    assert_eq!(migrated.scope, MemoryScope::global());
    assert!(bucket_raw.contains("warming endpoint"));
    assert!(summary.contains("## Operator Playbook"));
    assert!(summary.contains("warming endpoint to unblock it"));
    assert!(
        preferences_path(&codex_home)
            .with_file_name("preferences.jsonl.pre-bucket-migration")
            .exists()
    );
}

#[tokio::test]
async fn migration_infers_scope_for_bucketed_events_missing_scope() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    ensure_layout(&codex_home).await.expect("layout");
    let legacy_bucketed = serde_json::json!({
        "observed_at": "2026-04-26T12:00:00Z",
        "thread_id": "thread-1",
        "turn_id": "turn-1",
        "bucket": "operator_playbook",
        "operation": "upsert",
        "signal": "model_classified",
        "key": "mobius pr workflow",
        "candidate": "When working in the Mobius repo on PRs, keep it CLI-first",
        "source_excerpt": "When working in the Mobius repo on PRs, keep it CLI-first",
        "confidence": 0.8
    });
    fs::write(
        preferences_path(&codex_home),
        format!("{legacy_bucketed}\n"),
    )
    .await
    .expect("write bucketed legacy event");

    consolidate_preferences(
        &codex_home,
        &OrchestratorMemoryConfig {
            enabled: true,
            ..OrchestratorMemoryConfig::default()
        },
    )
    .await
    .expect("consolidate preferences");

    let migrated_raw = fs::read_to_string(preferences_path(&codex_home))
        .await
        .expect("read migrated preferences");
    let migrated =
        serde_json::from_str::<MemoryEvent>(migrated_raw.lines().next().expect("migrated event"))
            .expect("parse migrated event");

    assert_eq!(migrated.bucket, MemoryBucket::OperatorPlaybook);
    assert_eq!(
        migrated.scope,
        MemoryScope::new(
            super::super::types::MemoryScopeKind::Repo,
            "mobius",
            "repo-specific wording",
            /*confidence*/ 0.75,
        )
    );
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
            scope: MemoryScope::global(),
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
async fn consolidates_relational_attunement_into_dedicated_section() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[CandidateMemoryItem {
            bucket: MemoryBucket::RelationalAttunement,
            scope: MemoryScope::global(),
            operation: MemoryOperation::Upsert,
            signal: MemorySignal::ModelClassified,
            key: "direct but clarify when ambiguity could trigger the wrong thing".to_string(),
            candidate:
                "Respond directly, but slow down and clarify when ambiguity could trigger the wrong thing"
                    .to_string(),
            source_excerpt:
                "When things are unclear you will clarify to ensure nothing incorrect is triggered"
                    .to_string(),
            confidence: 0.8,
        }],
    )
    .await
    .expect("append relational attunement event");

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

    assert!(summary.contains("## Relational Attunement"));
    assert!(summary.contains("Respond directly, but slow down and clarify"));
}

#[tokio::test]
async fn consolidates_ongoing_threads_into_dedicated_section() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[CandidateMemoryItem {
            bucket: MemoryBucket::OngoingThreads,
            scope: MemoryScope::global(),
            operation: MemoryOperation::Upsert,
            signal: MemorySignal::ModelClassified,
            key: "designing orchestrator memory around emotional continuity".to_string(),
            candidate:
                "The user is actively shaping orchestrator memory around emotional continuity and attunement"
                    .to_string(),
            source_excerpt:
                "Memory is very important. We want to evoke understanding and the emotion or tone of conversations"
                    .to_string(),
            confidence: 0.8,
        }],
    )
    .await
    .expect("append ongoing thread event");

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

    assert!(summary.contains("## Ongoing Threads"));
    assert!(summary.contains("orchestrator memory around emotional continuity"));
}

#[tokio::test]
async fn consolidates_operator_playbook_into_dedicated_section() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    append_preference_events(
        &codex_home,
        "thread-1".to_string(),
        "turn-1".to_string(),
        &[CandidateMemoryItem {
            bucket: MemoryBucket::OperatorPlaybook,
            scope: MemoryScope::global(),
            operation: MemoryOperation::Upsert,
            signal: MemorySignal::ModelClassified,
            key: "when aws auth guard auth stalls try the warming endpoint".to_string(),
            candidate:
                "When aws-auth-guard authentication stalls, try the warming endpoint to unblock it"
                    .to_string(),
            source_excerpt:
                "agent was blocked on aws-auth-guard auth; the user suggested the warming endpoint and it worked"
                    .to_string(),
            confidence: 0.8,
        }],
    )
    .await
    .expect("append operator playbook event");

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

    assert!(summary.contains("## Operator Playbook"));
    assert!(summary.contains("warming endpoint to unblock it"));
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
                scope: MemoryScope::global(),
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::ModelClassified,
                key: "my moms name is alice".to_string(),
                candidate: "The user's mom's name is Alice".to_string(),
                source_excerpt: "My mom's name is Alice".to_string(),
                confidence: 0.8,
            },
            CandidateMemoryItem {
                bucket: MemoryBucket::PersonalContext,
                scope: MemoryScope::global(),
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
                scope: MemoryScope::global(),
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::ModelClassified,
                key: "user shared a google calendar invite link for future use https calendar app google example booking link keep this for later retrieval when calendar access or scheduling is needed".to_string(),
                candidate: "User shared a Google Calendar invite link for future use: https://calendar.app.google/example-booking-link. Keep this for later retrieval when calendar access or scheduling is needed.".to_string(),
                source_excerpt: "User shared a Google Calendar invite link for future use: https://calendar.app.google/example-booking-link. Keep this for later retrieval when calendar access or scheduling is needed.".to_string(),
                confidence: 0.8,
            },
            CandidateMemoryItem {
                bucket: MemoryBucket::FollowupState,
                scope: MemoryScope::global(),
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
