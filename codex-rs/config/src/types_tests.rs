use super::*;
use pretty_assertions::assert_eq;

#[test]
fn deserialize_skill_config_with_name_selector() {
    let cfg: SkillConfig = toml::from_str(
        r#"
            name = "github:yeet"
            enabled = false
        "#,
    )
    .expect("should deserialize skill config with name selector");

    assert_eq!(cfg.name.as_deref(), Some("github:yeet"));
    assert_eq!(cfg.path, None);
    assert!(!cfg.enabled);
}

#[test]
fn deserialize_skill_config_with_path_selector() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let skill_path = tempdir.path().join("skills").join("demo").join("SKILL.md");
    let cfg: SkillConfig = toml::from_str(&format!(
        r#"
            path = {path:?}
            enabled = false
        "#,
        path = skill_path.display().to_string(),
    ))
    .expect("should deserialize skill config with path selector");

    assert_eq!(
        cfg,
        SkillConfig {
            path: Some(
                AbsolutePathBuf::from_absolute_path(&skill_path)
                    .expect("skill path should be absolute"),
            ),
            name: None,
            enabled: false,
        }
    );
}

#[test]
fn memories_config_clamps_count_limits_to_nonzero_values() {
    let config = MemoriesConfig::from(MemoriesToml {
        max_raw_memories_for_consolidation: Some(0),
        max_rollouts_per_startup: Some(0),
        ..Default::default()
    });

    assert_eq!(
        config,
        MemoriesConfig {
            max_raw_memories_for_consolidation: 1,
            max_rollouts_per_startup: 1,
            ..MemoriesConfig::default()
        }
    );
}

#[test]
fn orchestrator_memory_config_defaults_to_disabled_orchestrator_scope() {
    assert_eq!(
        OrchestratorMemoryConfig::default(),
        OrchestratorMemoryConfig {
            enabled: false,
            scope: MemoriesScope::Orchestrator,
            debounce_seconds: 60,
            min_observations: 2,
            recent_turn_window: 8,
            max_summary_items: 24,
        }
    );
}

#[test]
fn orchestrator_memory_config_uses_explicit_values() {
    let config = OrchestratorMemoryConfig::from(OrchestratorMemoryToml {
        enabled: Some(true),
        scope: Some(MemoriesScope::All),
        debounce_seconds: Some(15),
        min_observations: Some(3),
        recent_turn_window: Some(6),
        max_summary_items: Some(10),
    });

    assert_eq!(
        config,
        OrchestratorMemoryConfig {
            enabled: true,
            scope: MemoriesScope::All,
            debounce_seconds: 15,
            min_observations: 3,
            recent_turn_window: 6,
            max_summary_items: 10,
        }
    );
}
