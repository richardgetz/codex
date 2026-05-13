use super::*;
use codex_config::types::OrchestratorMemoryConfig;
use codex_config::types::UserPreferencesMemoryBucket;
use codex_config::types::UserPreferencesMemoryBucketPolicy;
use codex_config::types::UserPreferencesMemoryConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::PathExt;
use tempfile::tempdir;
use tokio::fs as tokio_fs;

fn user_preferences_dir(
    codex_home: &codex_utils_absolute_path::AbsolutePathBuf,
) -> AbsolutePathBuf {
    user_preferences_root(codex_home)
}

#[tokio::test]
async fn build_user_preferences_instructions_reads_user_preferences_root() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let user_preferences_dir = user_preferences_dir(&codex_home);
    tokio_fs::create_dir_all(&user_preferences_dir)
        .await
        .unwrap();
    tokio_fs::write(
        user_preferences_dir.join("summary.md"),
        "Prefer concise implementation updates.",
    )
    .await
    .unwrap();

    let instructions = build_user_preferences_developer_instructions(
        &codex_home,
        &UserPreferencesMemoryConfig {
            scope: codex_config::types::MemoriesScope::All,
            ..UserPreferencesMemoryConfig::default()
        },
    )
    .await
    .unwrap();

    assert!(instructions.contains("User Preferences Memory"));
    assert!(instructions.contains("User Preferences Memory layout"));
    assert!(!instructions.contains("## Orchestrator Memory"));
    assert!(!instructions.contains("ORCHESTRATOR_MEMORY_SUMMARY"));
    assert!(instructions.contains("Default to checking the memory"));
    assert!(instructions.contains("Do not spawn a subagent or search the workspace before this"));
    assert!(instructions.contains(&format!(
        "- {} (already provided below; do NOT open again)",
        user_preferences_dir.join("summary.md").display()
    )));
    assert!(instructions.contains("Prefer concise implementation updates."));
}

#[tokio::test]
async fn build_user_preferences_instructions_falls_back_to_profile() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let user_preferences_dir = user_preferences_dir(&codex_home);
    tokio_fs::create_dir_all(&user_preferences_dir)
        .await
        .unwrap();
    tokio_fs::write(
        user_preferences_dir.join("profile.md"),
        "Act as the user's durable context layer.",
    )
    .await
    .unwrap();

    let instructions = build_user_preferences_developer_instructions(
        &codex_home,
        &UserPreferencesMemoryConfig::default(),
    )
    .await
    .unwrap();

    assert!(instructions.contains(&format!(
        "- {} (already provided below; do NOT open again)",
        user_preferences_dir.join("profile.md").display()
    )));
    assert!(instructions.contains("Act as the user's durable context layer."));
}

#[tokio::test]
async fn build_user_preferences_instructions_filters_raw_events_by_read_policy() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let user_preferences_dir = user_preferences_dir(&codex_home);
    tokio_fs::create_dir_all(&user_preferences_dir)
        .await
        .unwrap();
    tokio_fs::write(
        user_preferences_dir.join("preferences.jsonl"),
        concat!(
            "{\"observed_at\":\"2026-04-25T00:00:00Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\",\"bucket\":\"durable_preference\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"direct updates\",\"candidate\":\"Prefer concise implementation updates\",\"source_excerpt\":\"be concise\",\"confidence\":0.8}\n",
            "{\"observed_at\":\"2026-04-25T00:00:01Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-2\",\"bucket\":\"personal_context\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"private detail\",\"candidate\":\"Private personal context item\",\"source_excerpt\":\"private\",\"confidence\":0.8}\n",
        ),
    )
    .await
    .unwrap();

    let instructions = build_user_preferences_developer_instructions(
        &codex_home,
        &UserPreferencesMemoryConfig {
            bucket_policy: UserPreferencesMemoryBucketPolicy {
                read_buckets: vec![UserPreferencesMemoryBucket::DurablePreference],
                write_buckets: UserPreferencesMemoryBucket::all().to_vec(),
            },
            ..UserPreferencesMemoryConfig::default()
        },
    )
    .await
    .unwrap();

    assert!(instructions.contains("Prefer concise implementation updates"));
    assert!(!instructions.contains("Private personal context item"));
}

#[tokio::test]
async fn build_user_preferences_instructions_reports_legacy_raw_event_source() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let legacy_user_preferences_dir = codex_home.join("user_preferences_memory");
    tokio_fs::create_dir_all(&legacy_user_preferences_dir)
        .await
        .unwrap();
    tokio_fs::write(
        legacy_user_preferences_dir.join("preferences.jsonl"),
        "{\"observed_at\":\"2026-04-25T00:00:00Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\",\"bucket\":\"durable_preference\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"direct updates\",\"candidate\":\"Prefer concise implementation updates\",\"source_excerpt\":\"be concise\",\"confidence\":0.8}\n",
    )
    .await
    .unwrap();

    let instructions = build_user_preferences_developer_instructions(
        &codex_home,
        &UserPreferencesMemoryConfig {
            bucket_policy: UserPreferencesMemoryBucketPolicy {
                read_buckets: vec![UserPreferencesMemoryBucket::DurablePreference],
                write_buckets: UserPreferencesMemoryBucket::all().to_vec(),
            },
            ..UserPreferencesMemoryConfig::default()
        },
    )
    .await
    .unwrap();

    assert!(instructions.contains(&format!(
        "- {} (already provided below; do NOT open again)",
        legacy_user_preferences_dir.join("preferences.jsonl").display()
    )));
    assert!(instructions.contains("Prefer concise implementation updates"));
}

#[tokio::test]
async fn build_user_preferences_instructions_reads_legacy_root_without_migration() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let legacy_user_preferences_dir = codex_home.join("user_preferences_memory");
    tokio_fs::create_dir_all(&legacy_user_preferences_dir)
        .await
        .unwrap();
    tokio_fs::write(
        legacy_user_preferences_dir.join("summary.md"),
        "Legacy summary remains readable.",
    )
    .await
    .unwrap();

    let instructions = build_user_preferences_developer_instructions(
        &codex_home,
        &UserPreferencesMemoryConfig::default(),
    )
    .await
    .unwrap();

    assert!(instructions.contains(&format!(
        "- {} (already provided below; do NOT open again)",
        legacy_user_preferences_dir.join("summary.md").display()
    )));
    assert!(instructions.contains("Legacy summary remains readable."));
    assert!(!user_preferences_dir(&codex_home).exists());
}

#[test]
fn migrate_orchestrator_memory_to_user_preferences_copies_missing_files() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let orchestrator_memory_dir = codex_home.join("orchestrator_memory");
    let user_preferences_dir = user_preferences_dir(&codex_home);
    std::fs::create_dir_all(orchestrator_memory_dir.join("buckets")).unwrap();
    std::fs::write(orchestrator_memory_dir.join("summary.md"), "summary").unwrap();
    std::fs::write(
        orchestrator_memory_dir.join("buckets/followup_state.jsonl"),
        "bucket",
    )
    .unwrap();

    let migrated = migrate_orchestrator_memory_to_user_preferences(&codex_home).unwrap();

    assert!(migrated);
    assert_eq!(
        std::fs::read_to_string(user_preferences_dir.join("summary.md")).unwrap(),
        "summary"
    );
    assert_eq!(
        std::fs::read_to_string(user_preferences_dir.join("buckets/followup_state.jsonl")).unwrap(),
        "bucket"
    );
}

#[test]
fn migrate_legacy_user_preferences_memory_to_extension_copies_missing_files() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let legacy_user_preferences_dir = codex_home.join("user_preferences_memory");
    let user_preferences_dir = user_preferences_dir(&codex_home);
    std::fs::create_dir_all(legacy_user_preferences_dir.join("buckets")).unwrap();
    std::fs::write(legacy_user_preferences_dir.join("summary.md"), "summary").unwrap();
    std::fs::write(
        legacy_user_preferences_dir.join("buckets/followup_state.jsonl"),
        "bucket",
    )
    .unwrap();

    let migrated = migrate_legacy_user_preferences_memory_to_extension(&codex_home).unwrap();

    assert!(migrated);
    assert_eq!(
        std::fs::read_to_string(user_preferences_dir.join("summary.md")).unwrap(),
        "summary"
    );
    assert_eq!(
        std::fs::read_to_string(user_preferences_dir.join("buckets/followup_state.jsonl")).unwrap(),
        "bucket"
    );
}

#[tokio::test]
async fn prune_entries_matching_needle_rewrites_preferences_and_generated_artifacts() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let user_preferences_dir = user_preferences_dir(&codex_home);
    tokio_fs::create_dir_all(&user_preferences_dir)
        .await
        .unwrap();
    tokio_fs::write(
        user_preferences_dir.join("preferences.jsonl"),
        concat!(
            "{\"observed_at\":\"2026-04-25T00:00:00Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\",\"bucket\":\"followup_state\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"alpha needle\",\"candidate\":\"keep alpha\",\"source_excerpt\":\"alpha needle\",\"confidence\":0.8}\n",
            "{\"observed_at\":\"2026-04-25T00:00:01Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-2\",\"bucket\":\"followup_state\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"beta\",\"candidate\":\"keep beta\",\"source_excerpt\":\"beta\",\"confidence\":0.8}\n",
        ),
    )
    .await
    .unwrap();
    tokio_fs::write(
        user_preferences_dir.join("summary.md"),
        "alpha needle\nkeep beta\n",
    )
    .await
    .unwrap();
    tokio_fs::write(
        user_preferences_dir.join("profile.md"),
        "alpha needle profile\nkeep beta profile\n",
    )
    .await
    .unwrap();

    let result = prune_entries_matching_needle(
        &codex_home,
        &OrchestratorMemoryConfig {
            enabled: true,
            ..OrchestratorMemoryConfig::default()
        },
        "alpha needle",
    )
    .await
    .unwrap();

    assert_eq!(
        result,
        OrchestratorMemoryPruneResult {
            removed_preference_events: 1,
            removed_summary_lines: 1,
            removed_profile_lines: 1,
        }
    );
    let preferences = tokio_fs::read_to_string(user_preferences_dir.join("preferences.jsonl"))
        .await
        .unwrap();
    assert!(!preferences.contains("alpha needle"));
    let followup_bucket =
        tokio_fs::read_to_string(user_preferences_dir.join("buckets/followup_state.jsonl"))
            .await
            .unwrap();
    assert!(!followup_bucket.contains("alpha needle"));
    assert!(followup_bucket.contains("keep beta"));
}

#[tokio::test]
async fn build_user_preferences_instructions_rebuilds_restricted_summary_from_allowed_raw_items() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let user_preferences_dir = user_preferences_dir(&codex_home);
    tokio_fs::create_dir_all(&user_preferences_dir)
        .await
        .unwrap();
    tokio_fs::write(
        user_preferences_dir.join("summary.md"),
        "# User Preferences Memory Summary\n\n## Working Preferences\n- Prefer direct answers.\n",
    )
    .await
    .unwrap();
    tokio_fs::write(
        user_preferences_dir.join("preferences.jsonl"),
        concat!(
            "{\"observed_at\":\"2026-04-25T00:00:00Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\",\"bucket\":\"operator_playbook\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"warming endpoint unblock\",\"candidate\":\"When aws-auth-guard authentication stalls, try the warming endpoint to unblock it\",\"source_excerpt\":\"warming endpoint worked\",\"confidence\":0.8}\n",
            "{\"observed_at\":\"2026-04-25T00:00:01Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-2\",\"bucket\":\"personal_context\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"private detail\",\"candidate\":\"Private personal context item\",\"source_excerpt\":\"private\",\"confidence\":0.8}\n",
        ),
    )
    .await
    .unwrap();

    let instructions = build_user_preferences_developer_instructions(
        &codex_home,
        &UserPreferencesMemoryConfig {
            bucket_policy: UserPreferencesMemoryBucketPolicy {
                read_buckets: vec![UserPreferencesMemoryBucket::OperatorPlaybook],
                write_buckets: UserPreferencesMemoryBucket::all().to_vec(),
            },
            ..UserPreferencesMemoryConfig::default()
        },
    )
    .await
    .unwrap();

    assert!(instructions.contains("## Operator Playbook"));
    assert!(instructions.contains("warming endpoint to unblock it"));
    assert!(!instructions.contains("Private personal context item"));
}

#[tokio::test]
async fn build_user_preferences_instructions_appends_recent_items_missing_from_summary() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let user_preferences_dir = user_preferences_dir(&codex_home);
    tokio_fs::create_dir_all(&user_preferences_dir)
        .await
        .unwrap();
    tokio_fs::write(
        user_preferences_dir.join("summary.md"),
        "# User Preferences Memory Summary\n\n## Working Preferences\n- Prefer direct answers.\n",
    )
    .await
    .unwrap();
    tokio_fs::write(
        user_preferences_dir.join("preferences.jsonl"),
        "{\"observed_at\":\"2026-04-25T00:00:00Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\",\"bucket\":\"operator_playbook\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"warming endpoint unblock\",\"candidate\":\"When aws-auth-guard authentication stalls, try the warming endpoint to unblock it\",\"source_excerpt\":\"warming endpoint worked\",\"confidence\":0.8}\n",
    )
    .await
    .unwrap();

    let instructions = build_user_preferences_developer_instructions(
        &codex_home,
        &UserPreferencesMemoryConfig::default(),
    )
    .await
    .unwrap();

    assert!(instructions.contains("## Recent Continuity Items"));
    assert!(instructions.contains("warming endpoint to unblock it"));
}
