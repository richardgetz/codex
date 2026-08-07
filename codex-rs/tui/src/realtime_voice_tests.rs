use super::REALTIME_NO_PREAMBLES_PROMPT;
use super::realtime_start_prompt;

#[test]
fn realtime_start_prompt_preserves_default_behavior_when_enabled() {
    assert_eq!(realtime_start_prompt(true), None);
}

#[test]
fn realtime_start_prompt_adds_model_instruction_when_disabled() {
    assert_eq!(
        realtime_start_prompt(false),
        Some(Some(REALTIME_NO_PREAMBLES_PROMPT.to_string()))
    );
}
