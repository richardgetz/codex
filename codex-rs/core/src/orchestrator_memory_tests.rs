use super::*;
use codex_config::types::OrchestratorMemoryConfig;
use core_test_support::PathExt;
use tempfile::tempdir;
use tokio::fs as tokio_fs;

#[tokio::test]
async fn build_developer_instructions_renders_summary_template() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let orchestrator_memory_dir = codex_home.join("orchestrator_memory");
    tokio_fs::create_dir_all(&orchestrator_memory_dir)
        .await
        .unwrap();
    tokio_fs::write(
        orchestrator_memory_dir.join("summary.md"),
        "Prefer clarification before delegation.",
    )
    .await
    .unwrap();

    let instructions =
        build_developer_instructions(&codex_home, &OrchestratorMemoryConfig::default())
            .await
            .unwrap();

    assert!(instructions.contains("Orchestrator Memory"));
    assert!(instructions.contains(&format!(
        "- {} (already provided below; do NOT open again)",
        orchestrator_memory_dir.join("summary.md").display()
    )));
    assert!(instructions.contains("Prefer clarification before delegation."));
}

#[tokio::test]
async fn build_developer_instructions_falls_back_to_profile() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let orchestrator_memory_dir = codex_home.join("orchestrator_memory");
    tokio_fs::create_dir_all(&orchestrator_memory_dir)
        .await
        .unwrap();
    tokio_fs::write(
        orchestrator_memory_dir.join("profile.md"),
        "Act as the user's orchestration layer.",
    )
    .await
    .unwrap();

    let instructions =
        build_developer_instructions(&codex_home, &OrchestratorMemoryConfig::default())
            .await
            .unwrap();

    assert!(instructions.contains(&format!(
        "- {} (already provided below; do NOT open again)",
        orchestrator_memory_dir.join("profile.md").display()
    )));
    assert!(instructions.contains("Act as the user's orchestration layer."));
}

#[tokio::test]
async fn prune_entries_matching_needle_rewrites_preferences_and_generated_artifacts() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let orchestrator_memory_dir = codex_home.join("orchestrator_memory");
    tokio_fs::create_dir_all(&orchestrator_memory_dir)
        .await
        .unwrap();
    tokio_fs::write(
        orchestrator_memory_dir.join("preferences.jsonl"),
        concat!(
            "{\"observed_at\":\"2026-04-25T00:00:00Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\",\"bucket\":\"followup_state\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"alpha needle\",\"candidate\":\"keep alpha\",\"source_excerpt\":\"alpha needle\",\"confidence\":0.8}\n",
            "{\"observed_at\":\"2026-04-25T00:00:01Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-2\",\"bucket\":\"followup_state\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"beta\",\"candidate\":\"keep beta\",\"source_excerpt\":\"beta\",\"confidence\":0.8}\n",
        ),
    )
    .await
    .unwrap();
    tokio_fs::write(
        orchestrator_memory_dir.join("summary.md"),
        "alpha needle\nkeep beta\n",
    )
    .await
    .unwrap();
    tokio_fs::write(
        orchestrator_memory_dir.join("profile.md"),
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
    let preferences = tokio_fs::read_to_string(orchestrator_memory_dir.join("preferences.jsonl"))
        .await
        .unwrap();
    assert!(!preferences.contains("alpha needle"));
}

#[tokio::test]
async fn build_developer_instructions_appends_recent_direct_items_missing_from_summary() {
    let temp = tempdir().unwrap();
    let codex_home = temp.path().abs();
    let orchestrator_memory_dir = codex_home.join("orchestrator_memory");
    tokio_fs::create_dir_all(&orchestrator_memory_dir)
        .await
        .unwrap();
    tokio_fs::write(
        orchestrator_memory_dir.join("summary.md"),
        "# Orchestrator Memory Summary\n\n## Follow-Up State\n- Older orchestration note.\n",
    )
    .await
    .unwrap();
    tokio_fs::write(
        orchestrator_memory_dir.join("preferences.jsonl"),
        "{\"observed_at\":\"2026-04-25T00:00:00Z\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\",\"bucket\":\"personal_context\",\"operation\":\"upsert\",\"signal\":\"model_classified\",\"key\":\"calendar\",\"candidate\":\"User's meeting scheduling link: https://calendar.app.google/example-booking-link\",\"source_excerpt\":\"remember this link\",\"confidence\":0.8}\n",
    )
    .await
    .unwrap();

    let instructions = build_developer_instructions(
        &codex_home,
        &OrchestratorMemoryConfig {
            enabled: true,
            ..OrchestratorMemoryConfig::default()
        },
    )
    .await
    .unwrap();

    assert!(instructions.contains("## Recent Continuity Items"));
    assert!(instructions.contains("https://calendar.app.google/example-booking-link"));
}
