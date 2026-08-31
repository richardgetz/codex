use super::*;
use codex_state::decision_provenance::Actor;
use codex_state::decision_provenance::ApprovalState;
use codex_state::decision_provenance::Authority;
use codex_state::decision_provenance::Decision;
use codex_state::decision_provenance::DecisionStatus;
use codex_state::decision_provenance::PrivacyClass;
use codex_state::decision_provenance::Timestamps;
use codex_state::decision_provenance::now;

#[test]
fn decision_listing_snapshot() {
    let timestamp = now();
    let decision = Decision {
        id: "decision-ui".to_string(),
        parent_crossroad_id: Some("crossroad-ui".to_string()),
        selected_option: "additive".to_string(),
        unselected_options: vec!["rewrite".to_string()],
        actor: Actor::Collaborative,
        approval_state: ApprovalState::Approved,
        authority_basis: Authority::User,
        summary: "Keep existing state owners intact.".to_string(),
        rationale: Some("This keeps the migration bounded.".to_string()),
        assumptions: vec!["The local state home is writable.".to_string()],
        tradeoffs: vec!["Some traversal remains manual.".to_string()],
        request_ref: Some("request-ui".to_string()),
        task_ref: Some("task-ui".to_string()),
        project_ref: Some("project-ui".to_string()),
        repository: Some("codex".to_string()),
        source_session_id: Some("session-ui".to_string()),
        source_turn_id: Some("turn-ui".to_string()),
        related_preference_boundary_ids: vec!["boundary-ui".to_string()],
        related_constraint_ids: Vec::new(),
        warrant_id: None,
        change_set_ids: Vec::new(),
        status: DecisionStatus::Accepted,
        timestamps: Timestamps {
            created_at: timestamp,
            observed_at: Some(timestamp),
            recorded_at: timestamp,
            effective_at: Some(timestamp),
            superseded_at: None,
            updated_at: Some(timestamp),
        },
        superseded_by: None,
        reopened_as: None,
        privacy: PrivacyClass::Private,
    };

    insta::assert_snapshot!(format_decisions(vec![decision]).expect("format decisions"), @r#"
Decisions:
- decision-ui [accepted] additive — Keep existing state owners intact. (collaborative, approved)
"#);
}

#[test]
fn command_argument_validation_preserves_multi_word_searches() {
    assert_eq!(
        required_text("keep existing owners", DECISIONS_USAGE).expect("search text"),
        "keep existing owners"
    );
    assert!(required_id("decision-ui extra", DECISIONS_USAGE).is_err());
}
