use super::non_substantive_realtime_reasoning_effort;
use crate::context::RealtimeDelegationSource;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

#[test]
fn selects_configured_effort_for_read_only_handoff() {
    assert_eq!(
        non_substantive_realtime_reasoning_effort(
            RealtimeDelegationSource::Handoff,
            "What time is it?",
            Some(&ReasoningEffort::Low),
        ),
        Some(ReasoningEffort::Low)
    );
}

#[test]
fn inherits_session_effort_when_override_is_omitted() {
    assert_eq!(
        non_substantive_realtime_reasoning_effort(
            RealtimeDelegationSource::Handoff,
            "Can you hear me?",
            None,
        ),
        None
    );
}

#[test]
fn keeps_session_effort_for_mutating_handoff() {
    assert_eq!(
        non_substantive_realtime_reasoning_effort(
            RealtimeDelegationSource::Handoff,
            "Please implement the microphone alias command.",
            Some(&ReasoningEffort::Low),
        ),
        None
    );
}

#[test]
fn keeps_session_effort_for_ambiguous_handoff() {
    assert_eq!(
        non_substantive_realtime_reasoning_effort(
            RealtimeDelegationSource::Handoff,
            "I have an idea for the voice controls.",
            Some(&ReasoningEffort::Low),
        ),
        None
    );
}

#[test]
fn never_classifies_transcript_tail_as_read_only_handoff() {
    assert_eq!(
        non_substantive_realtime_reasoning_effort(
            RealtimeDelegationSource::TranscriptTailFlush,
            "What time is it?",
            Some(&ReasoningEffort::Low),
        ),
        None
    );
}
