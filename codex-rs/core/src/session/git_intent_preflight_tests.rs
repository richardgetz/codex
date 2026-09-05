use super::*;
use pretty_assertions::assert_eq;

#[test]
fn only_must_notes_relevant_to_a_code_change_become_candidates() {
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

    let candidate = relevant_git_intent_candidate(must_note, &request_tokens)
        .expect("relevant must note should produce a candidate");
    assert_eq!(candidate.commit, "abc123");
    assert!(relevant_git_intent_candidate(should_note, &request_tokens).is_none());
}

#[test]
fn non_code_change_questions_do_not_enter_git_intent_preflight() {
    assert!(!looks_like_code_change_request(
        "why did we choose this path?"
    ));
    assert!(looks_like_code_change_request("please refactor the API"));
}

#[test]
fn note_content_is_not_copied_into_the_provenance_source_label() {
    let candidate = relevant_git_intent_candidate(
        GitIntentNote {
            commit: "abc123".to_string(),
            timestamp: 1,
            subject: "generated files".to_string(),
            body: "intent_priority: must\nsummary: private rationale for generated files"
                .to_string(),
        },
        &intent_tokens("please modify generated files"),
    )
    .expect("relevant must note should produce a candidate");

    assert_eq!(
        candidate.source_ref.label.as_deref(),
        Some("must-level Git intent note")
    );
    assert_eq!(
        candidate.source_ref.reference,
        "refs/notes/intention@abc123"
    );
}
