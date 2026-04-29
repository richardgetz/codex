use super::*;
use crate::orchestrator_memory::types::AggregatedMemoryItem;
use crate::orchestrator_memory::types::AggregatedMemorySnapshot;
use crate::orchestrator_memory::types::MemoryBucket;
use chrono::Utc;
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
    let built = build_consolidation_agent_config(&config, /*is_chatgpt_auth*/ false)
        .expect("build consolidation config");

    assert_eq!(built.model.as_deref(), Some("gpt-5.3-codex-spark"));
    assert_eq!(built.model_reasoning_effort, Some(ReasoningEffort::Low));
    assert!(!built.orchestrator_memory.enabled);
    assert!(!built.memories.generate_memories);
    assert!(!built.memories.use_memories);
}

#[tokio::test]
async fn build_consolidation_agent_config_uses_implicit_orchestrator_defaults() {
    let temp = tempdir().expect("tempdir");
    let mut config = crate::config::ConfigBuilder::without_managed_config_for_tests()
        .codex_home(temp.path().to_path_buf())
        .build()
        .await
        .expect("test config");
    config.model = Some("gpt-5".to_string());
    config.model_reasoning_effort = Some(ReasoningEffort::High);

    let config = Arc::new(config);
    let built = build_consolidation_agent_config(&config, /*is_chatgpt_auth*/ false)
        .expect("build consolidation config");

    assert_eq!(built.model.as_deref(), Some("gpt-5.3-codex-spark"));
    assert_eq!(built.model_reasoning_effort, Some(ReasoningEffort::Low));
}

#[tokio::test]
async fn resolve_orchestrator_memory_model_falls_back_for_chatgpt_accounts() {
    let temp = tempdir().expect("tempdir");
    let config = crate::config::ConfigBuilder::without_managed_config_for_tests()
        .codex_home(temp.path().to_path_buf())
        .build()
        .await
        .expect("test config");

    assert_eq!(
        super::resolve_orchestrator_memory_model(&config, /*is_chatgpt_auth*/ true),
        "gpt-5.5"
    );
    assert_eq!(
        super::resolve_orchestrator_memory_model(&config, /*is_chatgpt_auth*/ false),
        "gpt-5.3-codex-spark"
    );
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

#[test]
fn parse_cleanup_payload_accepts_semantically_merged_memory_events() {
    let payload = parse_cleanup_payload(Some(
        r#"notes {
          "events": [
            {
              "bucket": "personal_context",
              "key": "user calendar meeting request link",
              "candidate": "User's Google Calendar meeting request link lets others schedule meetings with them: https://calendar.app.google/example-booking-link",
              "source_excerpt": "Merged duplicate calendar invite and meeting request link memories."
            }
          ]
        }"#,
    ))
    .expect("payload");

    assert_eq!(
        payload,
        CleanupPayload {
            events: vec![CleanupPayloadEvent {
                bucket: MemoryBucket::PersonalContext,
                key: "user calendar meeting request link".to_string(),
                candidate: "User's Google Calendar meeting request link lets others schedule meetings with them: https://calendar.app.google/example-booking-link".to_string(),
                source_excerpt: "Merged duplicate calendar invite and meeting request link memories."
                    .to_string(),
            }],
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

#[test]
fn apply_heuristic_guarantees_preserves_direct_items_missing_from_model_payload() {
    let payload = apply_heuristic_guarantees(
        ConsolidationPayload {
            summary_markdown:
                "# Orchestrator Memory Summary\n\n## Follow-Up State\n- Older orchestration note"
                    .to_string(),
            profile_markdown:
                "# Orchestrator Memory Profile\n\n## Follow-Up State\n- Older orchestration note"
                    .to_string(),
            should_clear: false,
        },
        &AggregatedMemorySnapshot {
            preferences: Vec::new(),
            personal_context: vec![AggregatedMemoryItem {
                bucket: MemoryBucket::PersonalContext,
                candidate:
                    "User's meeting scheduling link: https://calendar.app.google/example-booking-link"
                        .to_string(),
                observations: 1,
                direct_observations: 1,
                last_seen: Utc::now(),
                confidence_sum: 0.8,
            }],
            relational_attunement: Vec::new(),
            operator_playbook: Vec::new(),
            ongoing_threads: Vec::new(),
            followups: Vec::new(),
        },
    );

    assert!(
        payload
            .summary_markdown
            .contains("https://calendar.app.google/example-booking-link")
    );
    assert!(
        payload
            .profile_markdown
            .contains("https://calendar.app.google/example-booking-link")
    );
}
