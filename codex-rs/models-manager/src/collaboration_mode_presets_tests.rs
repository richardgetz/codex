use super::*;
use pretty_assertions::assert_eq;

#[test]
fn preset_names_use_mode_display_names() {
    assert_eq!(
        default_preset(CollaborationModesConfig::default()).name,
        ModeKind::Default.display_name()
    );
    assert_eq!(plan_preset().name, ModeKind::Plan.display_name());
    assert_eq!(
        continuous_preset().name,
        ModeKind::Continuous.display_name()
    );
    assert_eq!(
        orchestrator_preset().name,
        ModeKind::Orchestrator.display_name()
    );
    assert_eq!(
        plan_preset().reasoning_effort,
        Some(Some(ReasoningEffort::Medium))
    );
    assert_eq!(continuous_preset().reasoning_effort, None);
    assert_eq!(orchestrator_preset().reasoning_effort, None);
}

#[test]
fn builtin_collaboration_mode_presets_are_returned_in_tui_order() {
    let presets = builtin_collaboration_mode_presets(CollaborationModesConfig::default());
    let mode_order = presets
        .iter()
        .map(|preset| preset.mode.expect("preset mode"))
        .collect::<Vec<_>>();
    assert_eq!(
        mode_order,
        vec![
            ModeKind::Default,
            ModeKind::Plan,
            ModeKind::Continuous,
            ModeKind::Orchestrator,
        ]
    );
}

#[test]
fn default_mode_instructions_replace_mode_names_placeholder() {
    let default_instructions = default_preset(CollaborationModesConfig {
        default_mode_request_user_input: true,
    })
    .developer_instructions
    .expect("default preset should include instructions")
    .expect("default instructions should be set");

    assert!(!default_instructions.contains("{{KNOWN_MODE_NAMES}}"));
    assert!(!default_instructions.contains("{{REQUEST_USER_INPUT_AVAILABILITY}}"));
    assert!(!default_instructions.contains("{{ASKING_QUESTIONS_GUIDANCE}}"));

    let known_mode_names = format_mode_names(&TUI_VISIBLE_COLLABORATION_MODES);
    let expected_snippet = format!("Known mode names are {known_mode_names}.");
    assert!(default_instructions.contains(&expected_snippet));

    let expected_availability_message = request_user_input_availability_message(
        ModeKind::Default,
        /*default_mode_request_user_input*/ true,
    );
    assert!(default_instructions.contains(&expected_availability_message));
    assert!(default_instructions.contains("prefer using the `request_user_input` tool"));
}

#[test]
fn default_mode_instructions_use_plain_text_questions_when_feature_disabled() {
    let default_instructions = default_preset(CollaborationModesConfig::default())
        .developer_instructions
        .expect("default preset should include instructions")
        .expect("default instructions should be set");

    assert!(!default_instructions.contains("prefer using the `request_user_input` tool"));
    assert!(
        default_instructions.contains("ask the user directly with a concise plain-text question")
    );
}

#[test]
fn continuous_mode_instructions_replace_mode_names_placeholder() {
    let continuous_instructions = continuous_preset()
        .developer_instructions
        .expect("continuous preset should include instructions")
        .expect("continuous instructions should be set");

    assert!(!continuous_instructions.contains("{{KNOWN_MODE_NAMES}}"));
    assert!(!continuous_instructions.contains("{{REQUEST_USER_INPUT_AVAILABILITY}}"));
    assert!(continuous_instructions.contains("harness-enforced run semantics"));

    let known_mode_names = format_mode_names(&TUI_VISIBLE_COLLABORATION_MODES);
    let expected_snippet = format!("Known mode names are {known_mode_names}.");
    assert!(continuous_instructions.contains(&expected_snippet));

    let expected_availability_message = request_user_input_availability_message(
        ModeKind::Continuous,
        /*default_mode_request_user_input*/ false,
    );
    assert!(continuous_instructions.contains(&expected_availability_message));
}

#[test]
fn orchestrator_mode_instructions_replace_mode_names_placeholder() {
    let orchestrator_instructions = orchestrator_preset()
        .developer_instructions
        .expect("orchestrator preset should include instructions")
        .expect("orchestrator instructions should be set");

    assert!(!orchestrator_instructions.contains("{{KNOWN_MODE_NAMES}}"));
    assert!(!orchestrator_instructions.contains("{{REQUEST_USER_INPUT_AVAILABILITY}}"));
    assert!(
        orchestrator_instructions.contains("Orchestrator mode is for supervising delegated work")
    );
    assert!(orchestrator_instructions.contains(
        "Communication and supervision tools explicitly enabled for Orchestrator mode may be used directly in this thread"
    ));
    assert!(orchestrator_instructions.contains("Active-worker check-ins are patience checks"));
    assert!(
        orchestrator_instructions.contains("Do not tell workers to move faster"),
        "orchestrator prompt should keep patient supervision guidance"
    );

    let known_mode_names = format_mode_names(&TUI_VISIBLE_COLLABORATION_MODES);
    let expected_snippet = format!("Known mode names are {known_mode_names}.");
    assert!(orchestrator_instructions.contains(&expected_snippet));

    let expected_availability_message = request_user_input_availability_message(
        ModeKind::Orchestrator,
        /*default_mode_request_user_input*/ false,
    );
    assert!(orchestrator_instructions.contains(&expected_availability_message));
}
