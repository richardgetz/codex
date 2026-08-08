use super::configured_read_only_effort;
use super::contains_explicit_mutation_signal;
use super::is_conservative_read_only_request;
use crate::openai_models::ReasoningEffort;
use crate::realtime_handoff::RealtimeHandoffClassification;
use crate::realtime_handoff::RealtimeHandoffClassifier;
use crate::realtime_handoff::RealtimeHandoffClassifierKind;
use crate::realtime_handoff::RealtimeHandoffRouting;
use pretty_assertions::assert_eq;

#[test]
fn classifies_read_only_questions() {
    assert!(is_conservative_read_only_request("What time is it?"));
    assert!(is_conservative_read_only_request("Can you hear me"));
}

#[test]
fn rejects_mutating_or_ambiguous_requests() {
    assert!(!is_conservative_read_only_request(
        "Please implement the microphone alias command."
    ));
    assert!(!is_conservative_read_only_request("Do the migration"));
    assert!(!is_conservative_read_only_request(
        "Can you execute the migration?"
    ));
    assert!(!is_conservative_read_only_request(
        "Can you rename the file?"
    ));
    assert!(!is_conservative_read_only_request(
        "I have an idea for the voice controls."
    ));
}

#[test]
fn explicit_mutation_signals_are_a_model_classifier_deny_gate() {
    assert!(contains_explicit_mutation_signal(
        "Please update the configuration."
    ));
    assert!(!contains_explicit_mutation_signal("What branch am I on?"));
}

#[test]
fn applies_only_a_configured_effort() {
    assert_eq!(
        configured_read_only_effort("What time is it?", Some(&ReasoningEffort::Low)),
        Some(ReasoningEffort::Low)
    );
    assert_eq!(configured_read_only_effort("What time is it?", None), None);
    assert_eq!(
        configured_read_only_effort("Please implement this.", Some(&ReasoningEffort::Low)),
        None
    );
}

#[test]
fn routing_serializes_classifier_details() {
    let routing = RealtimeHandoffRouting {
        classifier: RealtimeHandoffClassifier {
            kind: RealtimeHandoffClassifierKind::Model,
            model: Some("classifier-model".to_string()),
            reasoning_effort: Some(ReasoningEffort::Minimal),
            fallback: None,
        },
        classification: RealtimeHandoffClassification::ReadOnly,
        selected_effort: Some(ReasoningEffort::Low),
    };

    assert_eq!(
        serde_json::to_value(routing).expect("routing should serialize"),
        serde_json::json!({
            "classifier": {
                "kind": "model",
                "model": "classifier-model",
                "reasoning_effort": "minimal"
            },
            "classification": "read_only",
            "selected_effort": "low"
        })
    );
}
