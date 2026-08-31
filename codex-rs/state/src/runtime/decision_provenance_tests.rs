use super::super::test_support::unique_temp_dir;
use super::*;
use crate::SqliteConfig;
use crate::decision_provenance::Actor;
use crate::decision_provenance::ApprovalState;
use crate::decision_provenance::Authority;
use crate::decision_provenance::BoundaryTransition;
use crate::decision_provenance::BoundaryTransitionKind;
use crate::decision_provenance::ChangeSet;
use crate::decision_provenance::Crossroad;
use crate::decision_provenance::CrossroadOption;
use crate::decision_provenance::CrossroadStatus;
use crate::decision_provenance::Decision;
use crate::decision_provenance::DecisionStatus;
use crate::decision_provenance::EntityType;
use crate::decision_provenance::LifecycleStatus;
use crate::decision_provenance::NotificationCategory;
use crate::decision_provenance::PreferenceBoundary;
use crate::decision_provenance::PreferenceBoundaryFilter;
use crate::decision_provenance::PreferenceBoundaryPreflight;
use crate::decision_provenance::PreferenceKind;
use crate::decision_provenance::PreferenceStrength;
use crate::decision_provenance::PrivacyClass;
use crate::decision_provenance::ProvenanceEvent;
use crate::decision_provenance::ProvenanceEventPayload;
use crate::decision_provenance::ProvenanceEventType;
use crate::decision_provenance::ProvenanceNotification;
use crate::decision_provenance::ProvenanceRelationship;
use crate::decision_provenance::ProvenanceWriteOptions;
use crate::decision_provenance::RelationshipEvidence;
use crate::decision_provenance::RelationshipKind;
use crate::decision_provenance::Scope;
use crate::decision_provenance::ScopeRef;
use crate::decision_provenance::SourceReference;
use crate::decision_provenance::Timestamps;
use crate::decision_provenance::Warrant;
use chrono::DateTime;
use chrono::TimeZone;
use chrono::Utc;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0)
        .single()
        .expect("valid timestamp")
}

fn timestamps(created_at: DateTime<Utc>) -> Timestamps {
    Timestamps {
        created_at,
        observed_at: Some(created_at),
        recorded_at: created_at,
        effective_at: Some(created_at),
        superseded_at: None,
        updated_at: Some(created_at),
    }
}

async fn runtime() -> (Arc<StateRuntime>, std::path::PathBuf) {
    let codex_home = unique_temp_dir();
    let runtime = StateRuntime::init(
        SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("state runtime should initialize");
    (runtime, codex_home)
}

fn boundary(
    id: &str,
    kind: PreferenceKind,
    authority: Authority,
    status: LifecycleStatus,
    scope: ScopeRef,
    created_at: DateTime<Utc>,
) -> PreferenceBoundary {
    PreferenceBoundary {
        id: id.to_string(),
        kind,
        statement: format!("Pause before changing {id}"),
        scope,
        strength: match kind {
            PreferenceKind::HardConstraint => PreferenceStrength::Hard,
            PreferenceKind::PreferenceBoundary => PreferenceStrength::Confirmation,
            PreferenceKind::SoftPreference | PreferenceKind::CandidatePreference => {
                PreferenceStrength::Soft
            }
        },
        authority,
        source: SourceReference::new("user_instruction", format!("session:{id}")),
        rationale: Some("The decision has a meaningful user-visible tradeoff.".to_string()),
        confidence: (authority == Authority::Agent).then_some(70),
        lifecycle_status: status,
        timestamps: timestamps(created_at),
        related_memory_record_id: Some(format!("memory:{id}")),
        superseded_by: None,
        privacy: PrivacyClass::Private,
    }
}

fn write_options(key: &str, actor: Actor, occurred_at: DateTime<Utc>) -> ProvenanceWriteOptions {
    ProvenanceWriteOptions {
        idempotency_key: Some(key.to_string()),
        actor,
        occurred_at,
    }
}

#[tokio::test]
async fn explicit_boundary_is_active_but_agent_inference_stays_candidate() {
    let (runtime, codex_home) = runtime().await;
    let scope = ScopeRef::new(Scope::Repo, "repo-a");
    let explicit = boundary(
        "boundary-explicit",
        PreferenceKind::PreferenceBoundary,
        Authority::User,
        LifecycleStatus::Active,
        scope.clone(),
        at(1_700_000_000),
    );
    let candidate = boundary(
        "boundary-candidate",
        PreferenceKind::CandidatePreference,
        Authority::Agent,
        LifecycleStatus::Candidate,
        scope.clone(),
        at(1_700_000_001),
    );

    runtime
        .record_preference_boundary(
            explicit.clone(),
            write_options("record-explicit", Actor::User, at(1_700_000_000)),
        )
        .await
        .expect("record explicit boundary");
    runtime
        .record_preference_boundary(
            candidate.clone(),
            write_options("record-candidate", Actor::Agent, at(1_700_000_001)),
        )
        .await
        .expect("record candidate boundary");

    let active = runtime
        .active_preference_boundaries(scope.clone())
        .await
        .expect("list active boundaries");
    assert_eq!(active, vec![explicit.clone()]);
    assert_eq!(
        runtime
            .preflight_preference_boundaries(scope.clone())
            .await
            .expect("preflight preference boundaries"),
        PreferenceBoundaryPreflight {
            scope: scope.clone(),
            active: vec![explicit.clone()],
            candidates: vec![candidate.clone()],
        }
    );
    assert_eq!(
        runtime
            .list_preference_boundaries(PreferenceBoundaryFilter {
                scope: Some(scope),
                ..PreferenceBoundaryFilter::default()
            })
            .await
            .expect("list boundaries"),
        vec![candidate, explicit]
    );

    runtime.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
}

#[tokio::test]
async fn candidate_confirmation_requires_user_and_promotes_one_boundary_record() {
    let (runtime, codex_home) = runtime().await;
    let candidate = boundary(
        "boundary-confirmation",
        PreferenceKind::CandidatePreference,
        Authority::Agent,
        LifecycleStatus::Candidate,
        ScopeRef::new(Scope::Task, "task-confirmation"),
        at(1_700_000_002),
    );
    runtime
        .record_preference_boundary(
            candidate.clone(),
            write_options(
                "record-confirmation-candidate",
                Actor::Agent,
                at(1_700_000_002),
            ),
        )
        .await
        .expect("record candidate boundary");

    let agent_confirmation = runtime
        .transition_preference_boundary(
            BoundaryTransition {
                boundary_id: candidate.id.clone(),
                transition: BoundaryTransitionKind::Confirm,
                replacement: None,
                actor: Actor::Agent,
                source: Some(SourceReference::new("agent", "turn-confirmation")),
            },
            write_options(
                "agent-confirmation-rejected",
                Actor::Agent,
                at(1_700_000_003),
            ),
        )
        .await;
    assert!(agent_confirmation.is_err());

    runtime
        .transition_preference_boundary(
            BoundaryTransition {
                boundary_id: candidate.id.clone(),
                transition: BoundaryTransitionKind::Confirm,
                replacement: None,
                actor: Actor::User,
                source: Some(SourceReference::new(
                    "user_instruction",
                    "turn-confirmation",
                )),
            },
            write_options("user-confirmation", Actor::User, at(1_700_000_004)),
        )
        .await
        .expect("confirm candidate boundary");

    let mut expected = candidate;
    expected.kind = PreferenceKind::PreferenceBoundary;
    expected.strength = PreferenceStrength::Confirmation;
    expected.authority = Authority::User;
    expected.confidence = None;
    expected.lifecycle_status = LifecycleStatus::Confirmed;
    expected.timestamps.updated_at = Some(at(1_700_000_004));
    expected.timestamps.effective_at = Some(at(1_700_000_004));
    assert_eq!(
        runtime
            .get_preference_boundary(&expected.id)
            .await
            .expect("read confirmed boundary"),
        Some(expected.clone())
    );
    assert_eq!(
        runtime
            .preflight_preference_boundaries(expected.scope.clone())
            .await
            .expect("preflight confirmed boundary")
            .active,
        vec![expected]
    );

    runtime.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
}

#[tokio::test]
async fn boundary_replacements_preserve_actor_authority_invariants() {
    let (runtime, codex_home) = runtime().await;
    let scope = ScopeRef::new(Scope::Task, "task-replacement");
    let agent_boundary = boundary(
        "boundary-agent-source",
        PreferenceKind::CandidatePreference,
        Authority::Agent,
        LifecycleStatus::Candidate,
        scope.clone(),
        at(1_700_000_005),
    );
    runtime
        .record_preference_boundary(
            agent_boundary.clone(),
            write_options("record-agent-source", Actor::Agent, at(1_700_000_005)),
        )
        .await
        .expect("record agent candidate");

    let agent_replacement = boundary(
        "boundary-agent-replacement",
        PreferenceKind::PreferenceBoundary,
        Authority::User,
        LifecycleStatus::Confirmed,
        scope.clone(),
        at(1_700_000_006),
    );
    let actor_mismatch = runtime
        .transition_preference_boundary(
            BoundaryTransition {
                boundary_id: agent_boundary.id.clone(),
                transition: BoundaryTransitionKind::Supersede,
                replacement: Some(agent_replacement.clone()),
                actor: Actor::Agent,
                source: None,
            },
            write_options("agent-replacement-mismatch", Actor::User, at(1_700_000_006)),
        )
        .await;
    assert!(actor_mismatch.is_err());

    runtime
        .transition_preference_boundary(
            BoundaryTransition {
                boundary_id: agent_boundary.id.clone(),
                transition: BoundaryTransitionKind::Supersede,
                replacement: Some(agent_replacement.clone()),
                actor: Actor::Agent,
                source: None,
            },
            write_options("agent-replacement", Actor::Agent, at(1_700_000_006)),
        )
        .await
        .expect("replace agent candidate");
    let mut expected_agent_replacement = agent_replacement;
    expected_agent_replacement.kind = PreferenceKind::CandidatePreference;
    expected_agent_replacement.strength = PreferenceStrength::Soft;
    expected_agent_replacement.authority = Authority::Agent;
    expected_agent_replacement.lifecycle_status = LifecycleStatus::Candidate;
    expected_agent_replacement.timestamps.effective_at = None;
    assert_eq!(
        runtime
            .get_preference_boundary("boundary-agent-replacement")
            .await
            .expect("read agent replacement"),
        Some(expected_agent_replacement)
    );

    let user_source = boundary(
        "boundary-user-source",
        PreferenceKind::CandidatePreference,
        Authority::Agent,
        LifecycleStatus::Candidate,
        scope.clone(),
        at(1_700_000_007),
    );
    runtime
        .record_preference_boundary(
            user_source.clone(),
            write_options("record-user-source", Actor::Agent, at(1_700_000_007)),
        )
        .await
        .expect("record second agent candidate");
    let user_replacement = boundary(
        "boundary-user-replacement",
        PreferenceKind::CandidatePreference,
        Authority::Agent,
        LifecycleStatus::Candidate,
        scope,
        at(1_700_000_008),
    );
    runtime
        .transition_preference_boundary(
            BoundaryTransition {
                boundary_id: user_source.id,
                transition: BoundaryTransitionKind::Supersede,
                replacement: Some(user_replacement.clone()),
                actor: Actor::User,
                source: None,
            },
            write_options("user-replacement", Actor::User, at(1_700_000_008)),
        )
        .await
        .expect("replace candidate with user boundary");
    let mut expected_user_replacement = user_replacement;
    expected_user_replacement.kind = PreferenceKind::PreferenceBoundary;
    expected_user_replacement.strength = PreferenceStrength::Confirmation;
    expected_user_replacement.authority = Authority::User;
    expected_user_replacement.lifecycle_status = LifecycleStatus::Active;
    expected_user_replacement.confidence = None;
    expected_user_replacement.timestamps.effective_at =
        Some(expected_user_replacement.timestamps.created_at);
    assert_eq!(
        runtime
            .get_preference_boundary("boundary-user-replacement")
            .await
            .expect("read user replacement"),
        Some(expected_user_replacement)
    );

    runtime.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
}

#[tokio::test]
async fn decision_graph_links_boundaries_scratchpad_and_implementation_artifacts() {
    let (runtime, codex_home) = runtime().await;
    let decision_at = at(1_700_000_010);
    let boundary = boundary(
        "boundary-graph",
        PreferenceKind::PreferenceBoundary,
        Authority::User,
        LifecycleStatus::Confirmed,
        ScopeRef::new(Scope::Project, "project-a"),
        at(1_700_000_000),
    );
    runtime
        .record_preference_boundary(
            boundary.clone(),
            write_options("record-graph-boundary", Actor::User, at(1_700_000_000)),
        )
        .await
        .expect("record graph boundary");

    let crossroad = Crossroad {
        id: "crossroad-graph".to_string(),
        request_ref: Some("request-1".to_string()),
        task_ref: Some("task-1".to_string()),
        project_ref: Some("project-a".to_string()),
        session_id: Some("session-old".to_string()),
        question: "Which implementation path should be used?".to_string(),
        options: vec![
            CrossroadOption {
                id: "small".to_string(),
                label: "Small additive change".to_string(),
                summary: Some("Keep existing owners intact".to_string()),
                tradeoffs: vec!["slower initial traversal".to_string()],
            },
            CrossroadOption {
                id: "rewrite".to_string(),
                label: "Rewrite the owner".to_string(),
                summary: None,
                tradeoffs: vec!["larger migration".to_string()],
            },
        ],
        recommended_option: Some("small".to_string()),
        affected_boundary_ids: vec![boundary.id.clone()],
        constraint_ids: vec!["constraint-1".to_string()],
        expected_tradeoffs: vec!["smaller review surface".to_string()],
        authority_required: Some(Authority::User),
        status: CrossroadStatus::Resolved,
        actor: Actor::Collaborative,
        source_refs: vec![SourceReference::new("session", "session-old")],
        linked_scratchpad_wait_id: Some("wait-42".to_string()),
        timestamps: timestamps(at(1_700_000_005)),
        privacy: PrivacyClass::Private,
    };
    runtime
        .record_crossroad(
            crossroad.clone(),
            write_options(
                "record-graph-crossroad",
                Actor::Collaborative,
                at(1_700_000_005),
            ),
        )
        .await
        .expect("record graph crossroad");

    let decision = Decision {
        id: "decision-graph".to_string(),
        parent_crossroad_id: Some(crossroad.id.clone()),
        selected_option: "small".to_string(),
        unselected_options: vec!["rewrite".to_string()],
        actor: Actor::Agent,
        approval_state: ApprovalState::Acknowledged,
        authority_basis: Authority::User,
        summary: "Use the additive implementation path.".to_string(),
        rationale: Some(
            "It preserves existing state owners and keeps migration bounded.".to_string(),
        ),
        assumptions: vec!["The local SQLite state home is available.".to_string()],
        tradeoffs: vec!["Projection consumers receive a narrower first version.".to_string()],
        request_ref: Some("request-1".to_string()),
        task_ref: Some("task-1".to_string()),
        project_ref: Some("project-a".to_string()),
        repository: Some("codex".to_string()),
        source_session_id: Some("session-old".to_string()),
        source_turn_id: Some("turn-1".to_string()),
        related_preference_boundary_ids: vec![boundary.id.clone()],
        related_constraint_ids: vec!["constraint-1".to_string()],
        warrant_id: Some("warrant-graph".to_string()),
        change_set_ids: vec!["change-set-graph".to_string()],
        status: DecisionStatus::Accepted,
        timestamps: timestamps(decision_at),
        superseded_by: None,
        reopened_as: None,
        privacy: PrivacyClass::Private,
    };
    runtime
        .record_decision(
            decision.clone(),
            write_options("record-graph-decision", Actor::Agent, decision_at),
        )
        .await
        .expect("record graph decision");

    let warrant = Warrant {
        id: "warrant-graph".to_string(),
        decision_id: decision.id.clone(),
        observations: vec![
            "The existing state runtime already owns local SQLite state.".to_string(),
        ],
        assumptions: decision.assumptions.clone(),
        priorities: vec!["Preserve existing ownership boundaries.".to_string()],
        evidence_refs: vec![SourceReference::new("session", "session-old")],
        tradeoffs: decision.tradeoffs.clone(),
        uncertainty: Some("Future adapters may need additional fields.".to_string()),
        qualifier: Some("This is an additive first version.".to_string()),
        timestamps: timestamps(decision_at),
        privacy: PrivacyClass::Private,
    };
    runtime
        .record_warrant(
            warrant.clone(),
            write_options("record-graph-warrant", Actor::Agent, decision_at),
        )
        .await
        .expect("record graph warrant");

    let change_set = ChangeSet {
        id: "change-set-graph".to_string(),
        decision_id: Some(decision.id.clone()),
        session_id: Some("session-old".to_string()),
        scratchpad_id: Some("scratchpad-1".to_string()),
        paths: vec!["state/src/decision_provenance".to_string()],
        commit_sha: Some("0123456789abcdef".to_string()),
        git_intent_note_ref: Some("refs/notes/intention".to_string()),
        pull_request: Some("#123".to_string()),
        issue: Some("#99".to_string()),
        test_runs: vec!["codex-state".to_string()],
        deployment_result: Some("not deployed".to_string()),
        later_failure_or_rollback: None,
        source_refs: vec![SourceReference::new("commit", "0123456789abcdef")],
        timestamps: timestamps(at(1_700_000_020)),
        privacy: PrivacyClass::Private,
    };
    runtime
        .link_change_set(
            change_set.clone(),
            write_options("record-graph-change-set", Actor::System, at(1_700_000_020)),
        )
        .await
        .expect("record graph change set");

    let constraint_link = ProvenanceRelationship {
        id: "relationship-constraint".to_string(),
        from_type: EntityType::Decision,
        from_id: decision.id.clone(),
        relation: RelationshipKind::ConstrainedBy,
        to_type: EntityType::Constraint,
        to_id: "constraint-1".to_string(),
        evidence: RelationshipEvidence::Explicit,
        summary: Some("repository invariant".to_string()),
        source_refs: Vec::new(),
        created_at: decision_at,
        privacy: PrivacyClass::Private,
    };
    runtime
        .record_relationship(
            constraint_link.clone(),
            write_options("record-graph-constraint-link", Actor::System, decision_at),
        )
        .await
        .expect("record constraint link");

    let why = runtime
        .decision_why(&decision.id)
        .await
        .expect("read why view")
        .expect("decision exists");
    assert_eq!(why.decision, decision);
    assert_eq!(why.crossroad, Some(crossroad.clone()));
    assert_eq!(why.boundaries, vec![boundary.clone()]);
    assert_eq!(why.warrant, Some(warrant.clone()));
    assert_eq!(why.change_sets, vec![change_set.clone()]);
    assert!(why.relationships.iter().any(|relationship| {
        relationship.relation == RelationshipKind::ConstrainedBy
            && relationship.to_id == "constraint-1"
    }));
    assert!(why.relationships.iter().any(|relationship| {
        relationship.relation == RelationshipKind::InfluencedBy
            && relationship.to_id == "boundary-graph"
    }));
    assert!(why.relationships.iter().any(|relationship| {
        relationship.relation == RelationshipKind::ImplementedBy
            && relationship.to_id == "change-set-graph"
    }));

    assert_eq!(
        runtime
            .decisions_influenced_by("boundary-graph", 20)
            .await
            .expect("find influenced decisions"),
        vec![decision.clone()]
    );
    assert_eq!(
        runtime
            .decision_sessions(&decision.id)
            .await
            .expect("find decision sessions"),
        vec!["session-old".to_string()]
    );
    assert_eq!(
        runtime
            .decision_artifacts(&decision.id)
            .await
            .expect("find decision artifacts"),
        vec![change_set.clone()]
    );

    let projection = runtime
        .read_provenance_projection()
        .await
        .expect("read projection");
    assert_eq!(projection.schema_version, 1);
    assert!(projection.read_only);
    assert_eq!(projection.decisions, vec![decision]);
    assert_eq!(projection.crossroads, vec![crossroad]);
    assert_eq!(projection.preference_boundaries, vec![boundary]);
    assert_eq!(projection.warrants, vec![warrant]);
    assert_eq!(projection.change_sets, vec![change_set]);
    assert_eq!(
        projection.indexes.decision_id.get("decision-graph"),
        Some(&vec!["decision-graph".to_string()])
    );
    assert_eq!(
        projection.indexes.commit_sha.get("0123456789abcdef"),
        Some(&vec!["change-set-graph".to_string()])
    );

    runtime.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
}

#[tokio::test]
async fn append_is_idempotent_and_rejects_a_different_payload_for_same_key() {
    let (runtime, codex_home) = runtime().await;
    let notification = ProvenanceNotification {
        id: "notification-idempotent".to_string(),
        category: NotificationCategory::ApprovalRequired,
        message: "Approval is required before selecting this path.".to_string(),
        preference_boundary_id: None,
        crossroad_id: Some("crossroad-idempotent".to_string()),
        decision_id: None,
        authority_required: Some(Authority::User),
        choice: Some("approve".to_string()),
        will_record: true,
        created_at: at(1_700_000_030),
        source_refs: Vec::new(),
        privacy: PrivacyClass::Private,
    };
    let event = ProvenanceEvent {
        schema_version: crate::decision_provenance::PROVENANCE_EVENT_VERSION,
        event_id: "event-idempotent".to_string(),
        idempotency_key: Some("idempotency-key".to_string()),
        event_type: ProvenanceEventType::NotificationRecorded,
        aggregate_type: EntityType::Notification,
        aggregate_id: notification.id.clone(),
        occurred_at: at(1_700_000_030),
        actor: Actor::System,
        privacy: PrivacyClass::Private,
        payload: ProvenanceEventPayload::Notification(notification.clone()),
    };
    assert!(
        runtime
            .append_provenance_event(event.clone())
            .await
            .expect("first append")
            .inserted
    );
    assert!(
        !runtime
            .append_provenance_event(event.clone())
            .await
            .expect("idempotent append")
            .inserted
    );
    let mut retry = event.clone();
    retry.event_id = "event-retry-with-new-id".to_string();
    retry.occurred_at = at(1_700_000_031);
    let retry_result = runtime
        .append_provenance_event(retry)
        .await
        .expect("idempotent retry with a fresh event id");
    assert!(!retry_result.inserted);
    assert_eq!(retry_result.event_id, "event-idempotent");
    assert_eq!(
        runtime
            .list_provenance_notifications(20)
            .await
            .expect("list notifications"),
        vec![notification.clone()]
    );

    let mut different = event;
    different.event_id = "event-different".to_string();
    if let ProvenanceEventPayload::Notification(notification) = &mut different.payload {
        notification.message = "A different message must not replace history.".to_string();
    }
    assert!(runtime.append_provenance_event(different).await.is_err());

    let event_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provenance_events")
        .fetch_one(runtime.pool.as_ref())
        .await
        .expect("count provenance events");
    assert_eq!(event_count, 1);

    runtime.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
}

#[tokio::test]
async fn boundary_pivot_and_decision_reopen_preserve_historical_views() {
    let (runtime, codex_home) = runtime().await;
    let initial_at = at(1_700_000_100);
    let decision_at = at(1_700_000_105);
    let pivot_at = at(1_700_000_110);
    let reopen_at = at(1_700_000_120);
    let old_boundary = boundary(
        "boundary-old",
        PreferenceKind::PreferenceBoundary,
        Authority::User,
        LifecycleStatus::Active,
        ScopeRef::new(Scope::Task, "task-1"),
        initial_at,
    );
    runtime
        .record_preference_boundary(
            old_boundary.clone(),
            write_options("record-old-boundary", Actor::User, initial_at),
        )
        .await
        .expect("record old boundary");
    let decision = Decision {
        id: "decision-pivot".to_string(),
        parent_crossroad_id: None,
        selected_option: "continue".to_string(),
        unselected_options: Vec::new(),
        actor: Actor::Collaborative,
        approval_state: ApprovalState::Approved,
        authority_basis: Authority::User,
        summary: "Continue with the confirmed boundary.".to_string(),
        rationale: Some("The current request is within the original scope.".to_string()),
        assumptions: Vec::new(),
        tradeoffs: Vec::new(),
        request_ref: Some("request-pivot".to_string()),
        task_ref: Some("task-1".to_string()),
        project_ref: None,
        repository: None,
        source_session_id: Some("session-pivot".to_string()),
        source_turn_id: Some("turn-pivot".to_string()),
        related_preference_boundary_ids: vec![old_boundary.id.clone()],
        related_constraint_ids: Vec::new(),
        warrant_id: None,
        change_set_ids: Vec::new(),
        status: DecisionStatus::Accepted,
        timestamps: timestamps(decision_at),
        superseded_by: None,
        reopened_as: None,
        privacy: PrivacyClass::Private,
    };
    runtime
        .record_decision(
            decision.clone(),
            write_options("record-pivot-decision", Actor::Collaborative, decision_at),
        )
        .await
        .expect("record pivot decision");

    let replacement = boundary(
        "boundary-new",
        PreferenceKind::PreferenceBoundary,
        Authority::User,
        LifecycleStatus::Active,
        ScopeRef::new(Scope::Task, "task-1"),
        pivot_at,
    );
    runtime
        .transition_preference_boundary(
            BoundaryTransition {
                boundary_id: old_boundary.id.clone(),
                transition: BoundaryTransitionKind::Narrow,
                replacement: Some(replacement.clone()),
                actor: Actor::User,
                source: Some(SourceReference::new("user_instruction", "turn-pivot")),
            },
            write_options("narrow-old-boundary", Actor::User, pivot_at),
        )
        .await
        .expect("narrow old boundary");
    runtime
        .transition_decision(
            &decision.id,
            DecisionStatus::Reopened,
            write_options("reopen-pivot-decision", Actor::User, reopen_at),
        )
        .await
        .expect("reopen decision");

    let current_old = runtime
        .get_preference_boundary(&old_boundary.id)
        .await
        .expect("read old boundary")
        .expect("old boundary exists");
    assert_eq!(current_old.lifecycle_status, LifecycleStatus::Narrowed);
    assert_eq!(current_old.superseded_by, Some(replacement.id.clone()));
    assert_eq!(
        runtime
            .get_preference_boundary(&replacement.id)
            .await
            .expect("read replacement")
            .expect("replacement exists"),
        replacement
    );
    assert_eq!(
        runtime
            .get_decision(&decision.id)
            .await
            .expect("read reopened decision")
            .expect("decision exists")
            .status,
        DecisionStatus::Reopened
    );
    assert_eq!(
        runtime
            .boundary_history(&old_boundary.id)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        runtime.decision_history(&decision.id).await.unwrap().len(),
        2
    );

    let before_pivot = runtime
        .decision_why_at(&decision.id, at(1_700_000_105))
        .await
        .expect("read pre-pivot why")
        .expect("decision exists before pivot");
    assert_eq!(before_pivot.decision.status, DecisionStatus::Accepted);
    assert_eq!(
        before_pivot.boundaries[0].lifecycle_status,
        LifecycleStatus::Active
    );
    let after_reopen = runtime
        .decision_why_at(&decision.id, at(1_700_000_125))
        .await
        .expect("read post-pivot why")
        .expect("decision exists after pivot");
    assert_eq!(after_reopen.decision.status, DecisionStatus::Reopened);
    assert_eq!(
        after_reopen.boundaries[0].lifecycle_status,
        LifecycleStatus::Narrowed
    );
    assert!(after_reopen.relationships.iter().any(|relationship| {
        relationship.relation == RelationshipKind::Supersedes
            && relationship.from_id == old_boundary.id
            && relationship.to_id == replacement.id
    }));

    runtime.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
}

#[tokio::test]
async fn sensitive_content_is_redacted_in_canonical_state_and_projection() {
    let (runtime, codex_home) = runtime().await;
    let boundary = PreferenceBoundary {
        id: "boundary-sensitive".to_string(),
        kind: PreferenceKind::PreferenceBoundary,
        statement: "token=super-secret-value".to_string(),
        scope: ScopeRef::global(),
        strength: PreferenceStrength::Confirmation,
        authority: Authority::User,
        source: SourceReference {
            source_type: "user".to_string(),
            reference: "private context".to_string(),
            label: Some("private label".to_string()),
            privacy: PrivacyClass::Sensitive,
        },
        rationale: Some("password=another-secret".to_string()),
        confidence: None,
        lifecycle_status: LifecycleStatus::Confirmed,
        timestamps: timestamps(at(1_700_000_200)),
        related_memory_record_id: None,
        superseded_by: None,
        privacy: PrivacyClass::Sensitive,
    };
    runtime
        .record_preference_boundary(
            boundary,
            write_options("record-sensitive", Actor::User, at(1_700_000_200)),
        )
        .await
        .expect("record sensitive boundary");
    let stored = runtime
        .get_preference_boundary("boundary-sensitive")
        .await
        .expect("read sensitive boundary")
        .expect("sensitive boundary exists");
    assert_eq!(stored.statement, "[redacted]");
    assert_eq!(stored.rationale.as_deref(), Some("[redacted]"));
    assert_eq!(stored.source.reference, "[redacted]");
    assert_eq!(stored.source.label.as_deref(), Some("[redacted]"));
    let projection = runtime
        .read_provenance_projection()
        .await
        .expect("read sensitive projection");
    assert_eq!(projection.preference_boundaries[0], stored);
    let projection_json = tokio::fs::read_to_string(runtime.provenance_projection_path())
        .await
        .expect("read projection bytes");
    assert!(!projection_json.contains("super-secret-value"));
    assert!(!projection_json.contains("another-secret"));

    runtime.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
}

#[tokio::test]
async fn sensitive_nested_provenance_payloads_are_redacted() {
    let (runtime, codex_home) = runtime().await;
    let timestamp = at(1_700_000_250);
    let crossroad = Crossroad {
        id: "crossroad-sensitive".to_string(),
        request_ref: Some("request-secret".to_string()),
        task_ref: Some("task-secret".to_string()),
        project_ref: Some("project-secret".to_string()),
        session_id: Some("session-secret".to_string()),
        question: "Which token=secret path should be selected?".to_string(),
        options: vec![CrossroadOption {
            id: "option-1".to_string(),
            label: "Use password=secret path".to_string(),
            summary: Some("api_key=secret summary".to_string()),
            tradeoffs: vec!["token=secret tradeoff".to_string()],
        }],
        recommended_option: Some("secret-option-label".to_string()),
        affected_boundary_ids: vec!["boundary-sensitive".to_string()],
        constraint_ids: vec!["constraint-sensitive".to_string()],
        expected_tradeoffs: vec!["secret expected tradeoff".to_string()],
        authority_required: Some(Authority::User),
        status: CrossroadStatus::Open,
        actor: Actor::System,
        source_refs: vec![SourceReference {
            source_type: "private".to_string(),
            reference: "sensitive-reference".to_string(),
            label: None,
            privacy: PrivacyClass::Sensitive,
        }],
        linked_scratchpad_wait_id: Some("wait-secret".to_string()),
        timestamps: timestamps(timestamp),
        privacy: PrivacyClass::Sensitive,
    };
    runtime
        .record_crossroad(
            crossroad,
            write_options("record-sensitive-crossroad", Actor::System, timestamp),
        )
        .await
        .expect("record sensitive crossroad");
    let stored_crossroad = runtime
        .get_crossroad("crossroad-sensitive")
        .await
        .expect("read sensitive crossroad")
        .expect("sensitive crossroad exists");
    assert_eq!(stored_crossroad.question, "[redacted]");
    assert_eq!(stored_crossroad.request_ref, None);
    assert_eq!(stored_crossroad.options[0].label, "[redacted]");
    assert_eq!(
        stored_crossroad.options[0].summary.as_deref(),
        Some("[redacted]")
    );
    assert_eq!(stored_crossroad.options[0].tradeoffs, vec!["[redacted]"]);
    assert_eq!(
        stored_crossroad.recommended_option.as_deref(),
        Some("[redacted]")
    );
    assert_eq!(stored_crossroad.expected_tradeoffs, vec!["[redacted]"]);
    assert_eq!(stored_crossroad.linked_scratchpad_wait_id, None);

    let decision = Decision {
        id: "decision-sensitive".to_string(),
        parent_crossroad_id: Some("crossroad-sensitive".to_string()),
        selected_option: "secret selection".to_string(),
        unselected_options: vec!["secret rejection".to_string()],
        actor: Actor::System,
        approval_state: ApprovalState::Approved,
        authority_basis: Authority::System,
        summary: "secret decision summary".to_string(),
        rationale: Some("secret rationale".to_string()),
        assumptions: vec!["secret assumption".to_string()],
        tradeoffs: vec!["secret decision tradeoff".to_string()],
        request_ref: Some("request-secret".to_string()),
        task_ref: Some("task-secret".to_string()),
        project_ref: Some("project-secret".to_string()),
        repository: Some("repository-secret".to_string()),
        source_session_id: Some("session-secret".to_string()),
        source_turn_id: Some("turn-secret".to_string()),
        related_preference_boundary_ids: Vec::new(),
        related_constraint_ids: Vec::new(),
        warrant_id: None,
        change_set_ids: Vec::new(),
        status: DecisionStatus::Accepted,
        timestamps: timestamps(timestamp),
        superseded_by: None,
        reopened_as: None,
        privacy: PrivacyClass::Sensitive,
    };
    runtime
        .record_decision(
            decision,
            write_options("record-sensitive-decision", Actor::System, timestamp),
        )
        .await
        .expect("record sensitive decision");
    let stored_decision = runtime
        .get_decision("decision-sensitive")
        .await
        .expect("read sensitive decision")
        .expect("sensitive decision exists");
    assert_eq!(stored_decision.selected_option, "[redacted]");
    assert_eq!(stored_decision.unselected_options, vec!["[redacted]"]);
    assert_eq!(stored_decision.assumptions, vec!["[redacted]"]);
    assert_eq!(stored_decision.tradeoffs, vec!["[redacted]"]);
    assert_eq!(stored_decision.source_session_id, None);

    let projection_json = tokio::fs::read_to_string(runtime.provenance_projection_path())
        .await
        .expect("read sensitive nested projection");
    assert!(!projection_json.contains("secret"));

    runtime.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
}

#[tokio::test]
async fn concurrent_identical_events_materialize_once() {
    let (runtime, codex_home) = runtime().await;
    let notification = ProvenanceNotification {
        id: "notification-concurrent".to_string(),
        category: NotificationCategory::Informational,
        message: "concurrent event".to_string(),
        preference_boundary_id: None,
        crossroad_id: None,
        decision_id: None,
        authority_required: None,
        choice: None,
        will_record: false,
        created_at: at(1_700_000_300),
        source_refs: Vec::new(),
        privacy: PrivacyClass::Private,
    };
    let event = ProvenanceEvent {
        schema_version: crate::decision_provenance::PROVENANCE_EVENT_VERSION,
        event_id: "event-concurrent".to_string(),
        idempotency_key: Some("key-concurrent".to_string()),
        event_type: ProvenanceEventType::NotificationRecorded,
        aggregate_type: EntityType::Notification,
        aggregate_id: notification.id.clone(),
        occurred_at: notification.created_at,
        actor: Actor::System,
        privacy: PrivacyClass::Private,
        payload: ProvenanceEventPayload::Notification(notification),
    };
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let runtime = runtime.clone();
        let event = event.clone();
        tasks.push(tokio::spawn(async move {
            runtime.append_provenance_event(event).await
        }));
    }
    for task in tasks {
        task.await
            .expect("concurrent writer task")
            .expect("concurrent append");
    }
    let notifications = runtime
        .list_provenance_notifications(20)
        .await
        .expect("list concurrent notifications");
    assert_eq!(notifications.len(), 1);
    let event_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provenance_events")
        .fetch_one(runtime.pool.as_ref())
        .await
        .expect("count concurrent events");
    assert_eq!(event_count, 1);

    runtime.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
}

#[tokio::test]
async fn invalid_projection_is_rebuilt_atomically_from_materialized_state() {
    let (runtime, codex_home) = runtime().await;
    let stored_boundary = boundary(
        "boundary-repair",
        PreferenceKind::HardConstraint,
        Authority::Repository,
        LifecycleStatus::Active,
        ScopeRef::global(),
        at(1_700_000_400),
    );
    runtime
        .record_preference_boundary(
            stored_boundary.clone(),
            write_options("record-repair", Actor::System, at(1_700_000_400)),
        )
        .await
        .expect("record repair boundary");
    let projection_path = runtime.provenance_projection_path();
    tokio::fs::write(&projection_path, b"not-json")
        .await
        .expect("corrupt projection");
    let repaired = runtime
        .read_provenance_projection()
        .await
        .expect("repair projection");
    assert_eq!(repaired.preference_boundaries, vec![stored_boundary]);
    assert!(repaired.read_only);
    assert!(
        serde_json::from_slice::<serde_json::Value>(
            &tokio::fs::read(&projection_path)
                .await
                .expect("read repaired projection")
        )
        .is_ok()
    );

    runtime.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
}

#[tokio::test]
async fn stale_valid_projection_is_rebuilt_from_event_watermark() {
    let (runtime, codex_home) = runtime().await;
    let first = boundary(
        "boundary-watermark-first",
        PreferenceKind::PreferenceBoundary,
        Authority::User,
        LifecycleStatus::Active,
        ScopeRef::global(),
        at(1_700_000_500),
    );
    runtime
        .record_preference_boundary(
            first.clone(),
            write_options("record-watermark-first", Actor::User, at(1_700_000_500)),
        )
        .await
        .expect("record first watermark boundary");
    let projection_path = runtime.provenance_projection_path();
    let stale_projection = tokio::fs::read(&projection_path)
        .await
        .expect("read initial projection");

    let second = boundary(
        "boundary-watermark-second",
        PreferenceKind::PreferenceBoundary,
        Authority::User,
        LifecycleStatus::Active,
        ScopeRef::global(),
        at(1_700_000_501),
    );
    runtime
        .record_preference_boundary(
            second.clone(),
            write_options("record-watermark-second", Actor::User, at(1_700_000_501)),
        )
        .await
        .expect("record second watermark boundary");
    tokio::fs::write(&projection_path, stale_projection)
        .await
        .expect("restore stale projection");

    let repaired = runtime
        .read_provenance_projection()
        .await
        .expect("repair stale projection");
    assert_eq!(repaired.preference_boundaries, vec![second, first]);
    assert!(repaired.source_event_id.is_some());
    assert!(repaired.source_event_recorded_at.is_some());

    runtime.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
}
