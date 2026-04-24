use super::*;
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

    let instructions = build_developer_instructions(&codex_home).await.unwrap();

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

    let instructions = build_developer_instructions(&codex_home).await.unwrap();

    assert!(instructions.contains(&format!(
        "- {} (already provided below; do NOT open again)",
        orchestrator_memory_dir.join("profile.md").display()
    )));
    assert!(instructions.contains("Act as the user's orchestration layer."));
}
