use super::*;
use codex_state::SqliteConfig;
use codex_state::decision_provenance::Actor;
use codex_state::decision_provenance::ApprovalState;
use codex_state::decision_provenance::Authority;
use codex_state::decision_provenance::Crossroad;
use codex_state::decision_provenance::CrossroadOption;
use codex_state::decision_provenance::CrossroadStatus;
use codex_state::decision_provenance::Decision;
use codex_state::decision_provenance::DecisionStatus;
use codex_state::decision_provenance::EntityType;
use codex_state::decision_provenance::EventSummary;
use codex_state::decision_provenance::PrivacyClass;
use codex_state::decision_provenance::ProvenanceRelationship;
use codex_state::decision_provenance::ProvenanceWriteOptions;
use codex_state::decision_provenance::RelationshipEvidence;
use codex_state::decision_provenance::RelationshipKind;
use codex_state::decision_provenance::SourceReference;
use codex_state::decision_provenance::Timestamps;
use codex_state::decision_provenance::now;
use codex_utils_absolute_path::test_support::PathExt;
use std::sync::Arc;
use tempfile::TempDir;

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
fn crossroad_listing_snapshot_is_informational() {
    let timestamp = now();
    let crossroad = Crossroad {
        id: "crossroad-ui".to_string(),
        request_ref: Some("request-ui".to_string()),
        task_ref: Some("task-ui".to_string()),
        project_ref: Some("project-ui".to_string()),
        session_id: Some("session-ui".to_string()),
        question: "Review the recorded direction before changing generated files.".to_string(),
        options: vec![CrossroadOption {
            id: "discuss".to_string(),
            label: "Discuss a new direction".to_string(),
            summary: Some("Keep actor and source explicit.".to_string()),
            tradeoffs: vec!["May change an earlier assumption.".to_string()],
        }],
        recommended_option: None,
        affected_boundary_ids: vec!["boundary-ui".to_string()],
        constraint_ids: Vec::new(),
        expected_tradeoffs: vec!["No decision is recorded by this observation.".to_string()],
        authority_required: None,
        status: CrossroadStatus::Open,
        actor: Actor::System,
        source_refs: vec![SourceReference::new(
            "git_intent_note",
            "refs/notes/intention@abc",
        )],
        linked_scratchpad_wait_id: None,
        timestamps: Timestamps {
            created_at: timestamp,
            observed_at: Some(timestamp),
            recorded_at: timestamp,
            effective_at: Some(timestamp),
            superseded_at: None,
            updated_at: Some(timestamp),
        },
        privacy: PrivacyClass::Private,
    };

    insta::assert_snapshot!(format_crossroads(vec![crossroad]).expect("format crossroads"), @r#"
Crossroads (informational; nothing here blocks work):
- crossroad-ui [open] — Review the recorded direction before changing generated files. (1 option, 1 source)
Entry point: `/decisions crossroads [all]`. Use full or unique short IDs; ambiguous prefixes are rejected.
Review bookkeeping: `/decisions reviewed <id>`, `/decisions dismiss <id>`, or `/decisions revisit <id>`; these never approve, block, or roll back work.
Use `/decisions show <id>` to discuss sources and linked history.
"#);
}

#[test]
fn crossroad_detail_snapshot_shows_history_without_approval() {
    let timestamp = now();
    let crossroad = Crossroad {
        id: "crossroad-detail".to_string(),
        request_ref: None,
        task_ref: None,
        project_ref: None,
        session_id: None,
        question: "Which direction should be discussed next?".to_string(),
        options: vec![
            CrossroadOption {
                id: "retain".to_string(),
                label: "Retain earlier direction".to_string(),
                summary: Some("Preserve the existing assumption.".to_string()),
                tradeoffs: vec!["Less migration work.".to_string()],
            },
            CrossroadOption {
                id: "override".to_string(),
                label: "Proceed with override".to_string(),
                summary: Some("Legacy option text remains historical.".to_string()),
                tradeoffs: Vec::new(),
            },
        ],
        recommended_option: None,
        affected_boundary_ids: Vec::new(),
        constraint_ids: Vec::new(),
        expected_tradeoffs: vec!["Discussing a change does not execute it.".to_string()],
        authority_required: None,
        status: CrossroadStatus::Reopened,
        actor: Actor::System,
        source_refs: vec![SourceReference::new("session", "session-detail")],
        linked_scratchpad_wait_id: None,
        timestamps: Timestamps {
            created_at: timestamp,
            observed_at: Some(timestamp),
            recorded_at: timestamp,
            effective_at: Some(timestamp),
            superseded_at: None,
            updated_at: Some(timestamp),
        },
        privacy: PrivacyClass::Private,
    };
    let relationship = ProvenanceRelationship {
        id: "relationship-detail-outbound".to_string(),
        from_type: EntityType::Crossroad,
        from_id: crossroad.id.clone(),
        relation: RelationshipKind::ConsideredNotDecisive,
        to_type: EntityType::Commit,
        to_id: "abc123".to_string(),
        evidence: RelationshipEvidence::Considered,
        summary: Some("Earlier source retained for discussion.".to_string()),
        source_refs: Vec::new(),
        created_at: timestamp,
        privacy: PrivacyClass::Private,
    };
    let inbound_relationship = ProvenanceRelationship {
        id: "relationship-detail-inbound".to_string(),
        from_type: EntityType::Decision,
        from_id: "decision-detail".to_string(),
        relation: RelationshipKind::ReviewedBy,
        to_type: EntityType::Crossroad,
        to_id: crossroad.id.clone(),
        evidence: RelationshipEvidence::Explicit,
        summary: None,
        source_refs: Vec::new(),
        created_at: timestamp,
        privacy: PrivacyClass::Private,
    };

    let history = vec![EventSummary {
        event_id: "event-detail".to_string(),
        idempotency_key: Some("review-detail".to_string()),
        event_type: "crossroad_status_changed".to_string(),
        aggregate_type: "crossroad".to_string(),
        aggregate_id: "crossroad-detail".to_string(),
        occurred_at: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
            .expect("fixed history timestamp"),
        actor: Actor::User,
        privacy: PrivacyClass::Private,
    }];

    insta::assert_snapshot!(format_crossroad_detail(&crossroad, &[relationship, inbound_relationship], &[], &history), @r#"
Crossroad `crossroad-detail`
Status: reopened
Question: Which direction should be discussed next?

This record is informational bookkeeping. It does not approve a path, block execution, or roll back code.

Review bookkeeping: `/decisions reviewed crossroad-detail`, `/decisions dismiss crossroad-detail`, or `/decisions revisit crossroad-detail`. These commands only append review history; they do not approve, block, or roll back work.
Use a full ID or a unique short prefix; ambiguous prefixes must be disambiguated.

Options recorded for discussion/reference:
- `retain`: Retain earlier direction — Preserve the existing assumption.
  Tradeoff: Less migration work.
- `override`: Proceed with override — Legacy option text remains historical.

Recorded caveats: Discussing a change does not execute it.

Prior sources (references only):
- session:session-detail

Linked records:
- considered_not_decisive → commit:abc123 [considered] — Earlier source retained for discussion.
- reviewed_by ← decision:decision-detail [explicit]

History:
- 2023-11-14T22:13:20+00:00 crossroad_status_changed by user (event-detail)
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

#[test]
fn review_cycles_use_distinct_append_only_write_keys() {
    let reviewed = user_write_options("reviewed", "crossroad-ui");
    let revisited = user_write_options("reopened", "crossroad-ui");
    let reviewed_again = user_write_options("reviewed", "crossroad-ui");

    assert_ne!(reviewed.idempotency_key, revisited.idempotency_key);
    assert_ne!(revisited.idempotency_key, reviewed_again.idempotency_key);
    assert_eq!(reviewed.actor, Actor::User);
    assert_eq!(revisited.actor, Actor::User);
    assert_eq!(reviewed_again.actor, Actor::User);
}

async fn test_runtime() -> (Arc<codex_state::StateRuntime>, TempDir) {
    let home = tempfile::tempdir().expect("create state home");
    let runtime = codex_state::StateRuntime::init(
        SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("initialize state runtime");
    (runtime, home)
}

fn test_crossroad(id: &str, status: CrossroadStatus) -> Crossroad {
    Crossroad {
        id: id.to_string(),
        request_ref: None,
        task_ref: None,
        project_ref: None,
        session_id: None,
        question: format!("Discuss direction for {id}"),
        options: vec![CrossroadOption {
            id: "discuss".to_string(),
            label: "Discuss a direction".to_string(),
            summary: None,
            tradeoffs: Vec::new(),
        }],
        recommended_option: None,
        affected_boundary_ids: Vec::new(),
        constraint_ids: Vec::new(),
        expected_tradeoffs: Vec::new(),
        authority_required: None,
        status,
        actor: Actor::System,
        source_refs: Vec::new(),
        linked_scratchpad_wait_id: None,
        timestamps: Timestamps::now(),
        privacy: PrivacyClass::Private,
    }
}

fn test_decision(id: &str, parent_crossroad_id: Option<&str>) -> Decision {
    Decision {
        id: id.to_string(),
        parent_crossroad_id: parent_crossroad_id.map(str::to_string),
        selected_option: "discuss".to_string(),
        unselected_options: Vec::new(),
        actor: Actor::Agent,
        approval_state: ApprovalState::NotRequired,
        authority_basis: Authority::System,
        summary: format!("Recorded direction for {id}"),
        rationale: Some("Recorded for traversal testing.".to_string()),
        assumptions: Vec::new(),
        tradeoffs: Vec::new(),
        request_ref: None,
        task_ref: None,
        project_ref: None,
        repository: None,
        source_session_id: None,
        source_turn_id: None,
        related_preference_boundary_ids: Vec::new(),
        related_constraint_ids: Vec::new(),
        warrant_id: None,
        change_set_ids: Vec::new(),
        status: DecisionStatus::Accepted,
        timestamps: Timestamps::now(),
        superseded_by: None,
        reopened_as: None,
        privacy: PrivacyClass::Private,
    }
}

fn test_write_options(id: &str, actor: Actor) -> ProvenanceWriteOptions {
    ProvenanceWriteOptions {
        idempotency_key: Some(format!("test:{id}")),
        actor,
        occurred_at: now(),
    }
}

#[tokio::test]
async fn mixed_exact_ids_are_rejected_by_show_history_and_review_commands() {
    let (runtime, _home) = test_runtime().await;
    runtime
        .record_crossroad(
            test_crossroad("shared-id", CrossroadStatus::Open),
            test_write_options("crossroad", Actor::System),
        )
        .await
        .expect("record crossroad");
    runtime
        .record_decision(
            test_decision("shared-id", None),
            test_write_options("decision", Actor::Agent),
        )
        .await
        .expect("record decision");

    for command in ["show shared-id", "history shared-id", "reviewed shared-id"] {
        let error = run_decisions_command(runtime.as_ref(), command)
            .await
            .expect_err("mixed exact ID should be rejected");
        assert!(
            error
                .to_string()
                .contains("ambiguous between a decision and crossroad")
        );
    }
}

#[tokio::test]
async fn over_limit_prefixes_remain_ambiguous_and_candidate_output_is_bounded() {
    let (runtime, _home) = test_runtime().await;
    for index in 0..3 {
        let id = format!("many-crossroads-{index}");
        runtime
            .record_crossroad(
                test_crossroad(&id, CrossroadStatus::Open),
                test_write_options(&id, Actor::System),
            )
            .await
            .expect("record crossroad");
    }

    let error = run_decisions_command(runtime.as_ref(), "show many-crossroads-")
        .await
        .expect_err("more than the resolver limit must remain ambiguous");
    let message = error.to_string();
    assert!(message.contains("is ambiguous"));
    assert_eq!(message.matches("crossroad:").count(), 2);
}

#[tokio::test]
async fn review_revisit_review_is_crossroad_history_without_fake_decision() {
    let (runtime, _home) = test_runtime().await;
    runtime
        .record_crossroad(
            test_crossroad("review-cycle", CrossroadStatus::Open),
            test_write_options("record", Actor::System),
        )
        .await
        .expect("record crossroad");

    run_decisions_command(runtime.as_ref(), "reviewed review-cycle")
        .await
        .expect("review crossroad");
    run_decisions_command(runtime.as_ref(), "revisit review-cycle")
        .await
        .expect("revisit crossroad");
    run_decisions_command(runtime.as_ref(), "reviewed review-cycle")
        .await
        .expect("review crossroad again");

    assert!(
        runtime
            .get_decision("review-cycle")
            .await
            .expect("read decision table")
            .is_none()
    );
    assert_eq!(
        runtime
            .crossroad_history("review-cycle")
            .await
            .unwrap()
            .len(),
        4
    );
}

#[tokio::test]
async fn decision_show_includes_linked_crossroad_and_nonempty_history() {
    let (runtime, _home) = test_runtime().await;
    runtime
        .record_crossroad(
            test_crossroad("show-parent", CrossroadStatus::Open),
            test_write_options("parent", Actor::System),
        )
        .await
        .expect("record parent crossroad");
    runtime
        .record_decision(
            test_decision("show-decision", Some("show-parent")),
            test_write_options("decision", Actor::Agent),
        )
        .await
        .expect("record decision");

    let output = run_decisions_command(runtime.as_ref(), "show show-decision")
        .await
        .expect("show decision");
    assert!(output.contains("Why:"));
    assert!(output.contains("Crossroad: show-parent"));
    assert!(output.contains("History:"));
    assert!(output.contains("decision_recorded"));
}
