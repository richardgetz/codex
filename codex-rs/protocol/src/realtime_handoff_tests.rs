use super::configured_read_only_effort;
use super::is_conservative_read_only_request;
use crate::openai_models::ReasoningEffort;
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
