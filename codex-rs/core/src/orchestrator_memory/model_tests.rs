use super::*;
use codex_protocol::openai_models::ReasoningEffort;
use core_test_support::PathExt;
use tempfile::tempdir;

#[tokio::test]
async fn build_consolidation_agent_config_prefers_orchestrator_model_defaults() {
    let temp = tempdir().expect("tempdir");
    let mut config = crate::config::ConfigBuilder::without_managed_config_for_tests()
        .codex_home(temp.path().to_path_buf())
        .build()
        .await
        .expect("test config");
    config.model = Some("gpt-5".to_string());
    config.model_reasoning_effort = Some(ReasoningEffort::High);
    config.thread_control.orchestrator.model = Some("gpt-5.3-codex-spark".to_string());
    config.thread_control.orchestrator.reasoning_effort = Some(ReasoningEffort::Low);

    let config = Arc::new(config);
    let built = build_consolidation_agent_config(&config).expect("build consolidation config");

    assert_eq!(built.model.as_deref(), Some("gpt-5.3-codex-spark"));
    assert_eq!(built.model_reasoning_effort, Some(ReasoningEffort::Low));
    assert!(!built.orchestrator_memory.enabled);
    assert!(!built.memories.generate_memories);
    assert!(!built.memories.use_memories);
}

#[test]
fn parse_consolidation_payload_extracts_embedded_json() {
    let payload = parse_consolidation_payload(Some(
        "preface {\"summary_markdown\":\"# Orchestrator Memory Summary\\n\\n- Prefer clarification first\",\"profile_markdown\":\"# Orchestrator Memory Profile\\n\\n## Prefer clarification first\",\"should_clear\":false}",
    ))
    .expect("payload");

    assert_eq!(
        payload,
        ConsolidationPayload {
            summary_markdown: "# Orchestrator Memory Summary\n\n- Prefer clarification first"
                .to_string(),
            profile_markdown: "# Orchestrator Memory Profile\n\n## Prefer clarification first"
                .to_string(),
            should_clear: false,
        }
    );
}

#[tokio::test]
async fn write_consolidation_payload_clears_generated_files_when_requested() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path().abs();
    ensure_layout(&codex_home).await.expect("layout");
    fs::write(summary_path(&codex_home), "old summary")
        .await
        .expect("write summary");
    fs::write(profile_path(&codex_home), "old profile")
        .await
        .expect("write profile");

    write_consolidation_payload(
        &codex_home,
        ConsolidationPayload {
            summary_markdown: String::new(),
            profile_markdown: String::new(),
            should_clear: true,
        },
    )
    .await
    .expect("write payload");

    assert!(!summary_path(&codex_home).exists());
    assert!(!profile_path(&codex_home).exists());
}
