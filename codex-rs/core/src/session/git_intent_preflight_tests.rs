use super::*;
use pretty_assertions::assert_eq;

#[test]
fn only_must_notes_relevant_to_a_code_change_become_conflicts() {
    let request_tokens = intent_tokens("please modify generated files for the API contract");
    let must_note = GitIntentNote {
        commit: "abc123".to_string(),
        timestamp: 1,
        subject: "generated files".to_string(),
        body: "intent_priority: must\nsummary: Preserve generated files for the API contract"
            .to_string(),
    };
    let should_note = GitIntentNote {
        commit: "def456".to_string(),
        timestamp: 2,
        subject: "generated files".to_string(),
        body: "intent_priority: should\nsummary: Preserve generated files for the API contract"
            .to_string(),
    };

    let conflict = relevant_git_intent_conflict(must_note, &request_tokens)
        .expect("relevant must note should produce a conflict");
    assert_eq!(conflict.commit, "abc123");
    assert!(relevant_git_intent_conflict(should_note, &request_tokens).is_none());
}

#[test]
fn non_code_change_questions_do_not_enter_git_intent_preflight() {
    assert!(!looks_like_code_change_request(
        "why did we choose this path?"
    ));
    assert!(looks_like_code_change_request("please refactor the API"));
}

#[test]
fn git_intent_requires_a_named_explicit_user_approval() {
    assert!(request_has_explicit_git_intent_override(
        "please override the prior Git intent and modify generated files"
    ));
    assert!(!request_has_explicit_git_intent_override(
        "please override that preference and modify generated files"
    ));
}

#[test]
fn note_content_is_not_copied_into_the_provenance_source_label() {
    let conflict = relevant_git_intent_conflict(
        GitIntentNote {
            commit: "abc123".to_string(),
            timestamp: 1,
            subject: "generated files".to_string(),
            body: "intent_priority: must\nsummary: private rationale for generated files"
                .to_string(),
        },
        &intent_tokens("please modify generated files"),
    )
    .expect("relevant must note should produce a conflict");

    assert_eq!(
        conflict.source_ref.label.as_deref(),
        Some("must-level Git intent note")
    );
    assert_eq!(conflict.source_ref.reference, "refs/notes/intention@abc123");
}
