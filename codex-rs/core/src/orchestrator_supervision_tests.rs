use super::OrchestratorSupervisionStore;
use crate::agent::AgentStatus;
use codex_config::types::OrchestratorEscalationConfig;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn thread_id(value: &str) -> ThreadId {
    ThreadId::from_string(value).expect("valid thread id")
}

#[tokio::test]
async fn build_developer_instructions_renders_registered_workers() {
    let tmp = TempDir::new().expect("tempdir");
    let codex_home = codex_utils_absolute_path::AbsolutePathBuf::try_from(tmp.path().to_path_buf())
        .expect("absolute path");
    let store = OrchestratorSupervisionStore::new(codex_home);
    let parent = thread_id("019dbc89-81eb-7300-a9a7-8db90bfa4f1f");
    let child = thread_id("019dbfd0-6c49-7623-bcd3-6d43a46d5916");
    store
        .register_worker(
            parent,
            child,
            Some("Arendt".to_string()),
            Some("worker".to_string()),
            "Check worker status".to_string(),
            Some(ModeKind::Default),
        )
        .await
        .expect("register worker");
    store
        .note_status(
            parent,
            child,
            &AgentStatus::Completed(Some("Blocked on boto3".to_string())),
        )
        .await
        .expect("note status");
    store.note_check(parent, child).await.expect("note check");

    let instructions = store
        .build_developer_instructions(parent, &OrchestratorEscalationConfig::default())
        .await
        .expect("developer instructions");

    assert!(instructions.contains("Arendt"));
    assert!(instructions.contains("Blocked on boto3"));
    assert!(instructions.contains("mode: inline"));
    assert!(instructions.contains("Global background job tables"));
    assert!(instructions.contains("past N hours"));
}

#[tokio::test]
async fn register_worker_updates_existing_entry() {
    let tmp = TempDir::new().expect("tempdir");
    let codex_home = codex_utils_absolute_path::AbsolutePathBuf::try_from(tmp.path().to_path_buf())
        .expect("absolute path");
    let store = OrchestratorSupervisionStore::new(codex_home.clone());
    let parent = thread_id("019dbc89-81eb-7300-a9a7-8db90bfa4f1f");
    let child = thread_id("019dbfd0-6c49-7623-bcd3-6d43a46d5916");

    store
        .register_worker(
            parent,
            child,
            Some("Arendt".to_string()),
            Some("worker".to_string()),
            "First prompt".to_string(),
            Some(ModeKind::Default),
        )
        .await
        .expect("first register");
    store
        .register_worker(
            parent,
            child,
            Some("Arendt".to_string()),
            Some("worker".to_string()),
            "Updated prompt".to_string(),
            Some(ModeKind::Continuous),
        )
        .await
        .expect("second register");

    let raw = tokio::fs::read_to_string(
        codex_home
            .join("orchestrator_supervision")
            .join("state.json"),
    )
    .await
    .expect("read state");
    assert!(raw.contains("Updated prompt"));
    assert!(!raw.contains("First prompt"));

    let instructions = store
        .build_developer_instructions(parent, &OrchestratorEscalationConfig::default())
        .await
        .expect("developer instructions");
    assert!(instructions.contains("Updated prompt"));
    assert_eq!(instructions.matches("Arendt").count(), 1);
}
