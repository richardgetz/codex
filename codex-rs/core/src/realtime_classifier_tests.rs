use super::ClassifierFailure;
use super::model_decision;
use super::text_decision;
use super::text_fallback_decision;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::realtime_handoff::RealtimeHandoffClassification;
use codex_protocol::realtime_handoff::RealtimeHandoffClassifier;
use codex_protocol::realtime_handoff::RealtimeHandoffClassifierFallback;
use codex_protocol::realtime_handoff::RealtimeHandoffClassifierKind;
use pretty_assertions::assert_eq;

#[test]
fn text_classifier_selects_configured_effort_only_for_read_only_input() {
    let decision = text_decision("What branch am I on?", Some(&ReasoningEffort::Low), None);

    assert_eq!(
        decision,
        super::RealtimeHandoffRoutingDecision {
            selected_effort: Some(ReasoningEffort::Low),
            routing: codex_protocol::realtime_handoff::RealtimeHandoffRouting {
                classifier: RealtimeHandoffClassifier {
                    kind: RealtimeHandoffClassifierKind::Text,
                    model: None,
                    reasoning_effort: None,
                    fallback: None,
                },
                classification: RealtimeHandoffClassification::ReadOnly,
                selected_effort: Some(ReasoningEffort::Low),
            },
        }
    );
}

#[test]
fn model_classifier_preserves_model_and_reasoning_in_routing() {
    let decision = model_decision(
        "The request needs work.",
        RealtimeHandoffClassification::Substantive,
        &ReasoningEffort::Low,
        "gpt-5.3-codex-spark".to_string(),
        Some(ReasoningEffort::Minimal),
    );

    assert_eq!(
        decision.routing.classifier,
        RealtimeHandoffClassifier {
            kind: RealtimeHandoffClassifierKind::Model,
            model: Some("gpt-5.3-codex-spark".to_string()),
            reasoning_effort: Some(ReasoningEffort::Minimal),
            fallback: None,
        }
    );
    assert_eq!(
        decision.routing.classification,
        RealtimeHandoffClassification::Substantive
    );
    assert_eq!(decision.selected_effort, None);
}

#[test]
fn model_read_only_result_cannot_override_an_explicit_mutation_signal() {
    let decision = model_decision(
        "Please update the configuration.",
        RealtimeHandoffClassification::ReadOnly,
        &ReasoningEffort::Low,
        "gpt-5.3-codex-spark".to_string(),
        None,
    );

    assert_eq!(decision.selected_effort, None);
    assert_eq!(
        decision.routing.classification,
        RealtimeHandoffClassification::ReadOnly
    );
}

#[test]
fn model_classifier_falls_back_to_text_with_reason() {
    let decision = text_fallback_decision(
        "What branch am I on?",
        &ReasoningEffort::Low,
        "gpt-5.3-codex-spark".to_string(),
        Some(ReasoningEffort::Minimal),
        ClassifierFailure::TimedOut,
    );

    assert_eq!(
        decision.routing.classifier,
        RealtimeHandoffClassifier {
            kind: RealtimeHandoffClassifierKind::Text,
            model: Some("gpt-5.3-codex-spark".to_string()),
            reasoning_effort: Some(ReasoningEffort::Minimal),
            fallback: Some(RealtimeHandoffClassifierFallback::TimedOut),
        }
    );
    assert_eq!(
        decision.routing.classification,
        RealtimeHandoffClassification::ReadOnly
    );
    assert_eq!(decision.selected_effort, Some(ReasoningEffort::Low));
}

#[test]
fn oversized_input_falls_back_without_model_classification() {
    let input = "what ".repeat(1_000);
    let decision = text_fallback_decision(
        &input,
        &ReasoningEffort::Low,
        "gpt-5.3-codex-spark".to_string(),
        Some(ReasoningEffort::Minimal),
        ClassifierFailure::InputTooLong,
    );

    assert_eq!(decision.selected_effort, None);
    assert_eq!(
        decision.routing.classifier.fallback,
        Some(RealtimeHandoffClassifierFallback::InputTooLong)
    );
    assert_eq!(
        decision.routing.classification,
        RealtimeHandoffClassification::Substantive
    );
}
