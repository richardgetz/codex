use super::*;
use pretty_assertions::assert_eq;
use std::time::Duration;

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
fn decision_provenance_config_is_opt_in() {
    assert_eq!(
        DecisionProvenanceConfig::from(DecisionProvenanceToml::default()),
        DecisionProvenanceConfig {
            enabled: false,
            git_intent_bridge: false,
        }
    );
    assert_eq!(
        DecisionProvenanceConfig::from(DecisionProvenanceToml {
            enabled: Some(true),
            git_intent_bridge: Some(true),
        }),
        DecisionProvenanceConfig {
            enabled: true,
            git_intent_bridge: true,
        }
    );
    assert_eq!(
        DecisionProvenanceConfig::from(DecisionProvenanceToml {
            enabled: Some(false),
            git_intent_bridge: Some(true),
        }),
        DecisionProvenanceConfig {
            enabled: false,
            git_intent_bridge: false,
        }
    );
}

#[test]
fn orchestrator_memory_config_defaults_to_enabled_orchestrator_scope() {
    assert_eq!(
        OrchestratorMemoryConfig::default(),
        OrchestratorMemoryConfig {
            enabled: true,
            scope: MemoriesScope::Orchestrator,
            debounce_seconds: 60,
            min_observations: 2,
            recent_turn_window: 8,
            max_summary_items: 24,
            model_on_heuristic_miss: false,
            model_consolidation: false,
            cleanup: OrchestratorMemoryCleanupConfig::default(),
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
        model_on_heuristic_miss: Some(true),
        model_consolidation: Some(true),
        cleanup: Some(OrchestratorMemoryCleanupToml {
            enabled: Some(false),
            schedule: Some("04:15".to_string()),
            run_missed_on_startup: Some(false),
            dedupe_raw_events: Some(false),
            deep_consolidation: Some(false),
            model_consolidation: Some(false),
            retain_forget_events_days: Some(7),
        }),
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
            model_on_heuristic_miss: true,
            model_consolidation: true,
            cleanup: OrchestratorMemoryCleanupConfig {
                enabled: false,
                schedule: "04:15".to_string(),
                run_missed_on_startup: false,
                dedupe_raw_events: false,
                deep_consolidation: false,
                model_consolidation: false,
                retain_forget_events_days: 7,
            },
        }
    );
}

#[test]
fn user_preferences_memory_defaults_to_enabled_all_scope() {
    assert_eq!(
        UserPreferencesMemoryConfig::default(),
        UserPreferencesMemoryConfig {
            enabled: true,
            scope: MemoriesScope::All,
            debounce_seconds: 60,
            min_observations: 2,
            recent_turn_window: 8,
            max_summary_items: 24,
            model_on_heuristic_miss: false,
            model_consolidation: false,
            bucket_policy: UserPreferencesMemoryBucketPolicy::default(),
            migrate_from_orchestrator_memory: false,
            disable_orchestrator_memory_after_migration: false,
            cleanup: OrchestratorMemoryCleanupConfig::default(),
        }
    );
}

#[test]
fn user_preferences_memory_policy_defaults_to_all_buckets() {
    let policy = UserPreferencesMemoryBucketPolicy::default();
    assert_eq!(policy.read_buckets, UserPreferencesMemoryBucket::all());
    assert_eq!(policy.write_buckets, UserPreferencesMemoryBucket::all());
}

#[test]
fn situational_requirements_filters_incomplete_rules() {
    let config = SituationalRequirementsConfig::from(SituationalRequirementsToml {
        enabled: Some(true),
        rules: vec![
            SituationalRequirementRuleToml {
                trigger: Some(SituationalRequirementTrigger::CodeChange),
                actions: vec![SituationalRequirementActionToml {
                    action: Some(SituationalRequirementAction::GitIntentNote),
                    mcp: Some(" git-intent-notes ".to_string()),
                    skill: None,
                    reason: Some(" preserve intent ".to_string()),
                }],
            },
            SituationalRequirementRuleToml {
                trigger: Some(SituationalRequirementTrigger::DocChange),
                actions: Vec::new(),
            },
        ],
    });

    assert_eq!(
        config,
        SituationalRequirementsConfig {
            enabled: true,
            rules: vec![SituationalRequirementRuleConfig {
                trigger: SituationalRequirementTrigger::CodeChange,
                actions: vec![SituationalRequirementActionConfig {
                    action: SituationalRequirementAction::GitIntentNote,
                    mcp: Some("git-intent-notes".to_string()),
                    skill: None,
                    reason: Some("preserve intent".to_string()),
                }],
            }],
        }
    );
}

#[test]
fn scratchpad_fanout_defaults_off_and_clamps_max_agents() {
    assert_eq!(
        ScratchpadFanoutConfig::from(Some(ScratchpadFanoutToml {
            enabled: Some(true),
            max_agents: Some(99),
        })),
        ScratchpadFanoutConfig {
            enabled: true,
            max_agents: 16,
        }
    );
}

#[test]
fn session_tmp_defaults_off_without_changing_existing_temp_behavior() {
    assert_eq!(
        SessionTmpConfig::from(None),
        SessionTmpConfig {
            enabled: false,
            root: None,
            stale_after: Duration::from_secs(DEFAULT_SESSION_TMP_STALE_AFTER_DAYS * 24 * 60 * 60),
        }
    );
}

#[test]
fn session_tmp_config_accepts_explicit_root_and_stale_age() {
    let root = AbsolutePathBuf::from_absolute_path("/tmp/codex-session-tmp")
        .expect("test root should be absolute");
    assert_eq!(
        SessionTmpConfig::from(Some(SessionTmpToml {
            enabled: Some(true),
            root: Some(root.clone()),
            stale_after_days: Some(3),
        })),
        SessionTmpConfig {
            enabled: true,
            root: Some(root),
            stale_after: Duration::from_secs(3 * 24 * 60 * 60),
        }
    );
}

#[test]
fn scratchpad_rollback_defaults_allows_disable_and_clamps_retention() {
    assert_eq!(
        ScratchpadRollbackConfig::from(None),
        ScratchpadRollbackConfig {
            max_user_turn_checkpoints: 10,
        }
    );
    assert_eq!(
        ScratchpadRollbackConfig::from(Some(ScratchpadRollbackToml {
            max_user_turn_checkpoints: Some(0),
        })),
        ScratchpadRollbackConfig {
            max_user_turn_checkpoints: 0,
        }
    );
    assert_eq!(
        ScratchpadRollbackConfig::from(Some(ScratchpadRollbackToml {
            max_user_turn_checkpoints: Some(2048),
        })),
        ScratchpadRollbackConfig {
            max_user_turn_checkpoints: 1024,
        }
    );
}

#[test]
fn scratchpad_loopback_defaults_to_five_in_five_minutes_and_clamps_values() {
    assert_eq!(
        ScratchpadLoopbackConfig::from(None),
        ScratchpadLoopbackConfig {
            max_loopbacks: 5,
            window: Duration::from_secs(5 * 60),
        }
    );
    assert_eq!(
        ScratchpadLoopbackConfig::from(Some(ScratchpadLoopbackToml {
            max_loopbacks: Some(0),
            window_minutes: Some(0),
        })),
        ScratchpadLoopbackConfig {
            max_loopbacks: 1,
            window: Duration::from_secs(60),
        }
    );
    assert_eq!(
        ScratchpadLoopbackConfig::from(Some(ScratchpadLoopbackToml {
            max_loopbacks: Some(2048),
            window_minutes: Some(9),
        })),
        ScratchpadLoopbackConfig {
            max_loopbacks: 1024,
            window: Duration::from_secs(9 * 60),
        }
    );
}

#[test]
fn accounts_config_trims_blank_active_alias() {
    let config = AccountsConfig::from(AccountsToml {
        active: Some("   ".to_string()),
        rotation: None,
    });

    assert_eq!(
        config,
        AccountsConfig {
            active: None,
            rotation: Vec::new(),
        }
    );
}

#[test]
fn accounts_config_preserves_active_alias() {
    let config = AccountsConfig::from(AccountsToml {
        active: Some("work".to_string()),
        rotation: None,
    });

    assert_eq!(
        config,
        AccountsConfig {
            active: Some("work".to_string()),
            rotation: Vec::new(),
        }
    );
}

#[test]
fn accounts_config_normalizes_rotation_aliases() {
    let config = AccountsConfig::from(AccountsToml {
        active: None,
        rotation: Some(vec![
            " default ".to_string(),
            "work".to_string(),
            "WORK".to_string(),
            " ".to_string(),
            "personal".to_string(),
        ]),
    });

    assert_eq!(
        config,
        AccountsConfig {
            active: None,
            rotation: vec![
                "default".to_string(),
                "work".to_string(),
                "personal".to_string()
            ],
        }
    );
}

#[test]
fn memories_config_clamps_rate_limit_remaining_threshold() {
    let config = MemoriesConfig::from(MemoriesToml {
        min_rate_limit_remaining_percent: Some(101),
        ..Default::default()
    });
    assert_eq!(
        config,
        MemoriesConfig {
            min_rate_limit_remaining_percent: 100,
            ..MemoriesConfig::default()
        }
    );

    let config = MemoriesConfig::from(MemoriesToml {
        min_rate_limit_remaining_percent: Some(-1),
        ..Default::default()
    });
    assert_eq!(
        config,
        MemoriesConfig {
            min_rate_limit_remaining_percent: 0,
            ..MemoriesConfig::default()
        }
    );
}
