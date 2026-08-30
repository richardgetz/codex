use super::ProvenancePreflightOutcome;
use super::record_turn_provenance_preflight;
use super::request_has_explicit_override;
use super::request_honors_boundary;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context_with_auth_config_home_and_rx;
use crate::session::turn_context::TurnContext;
use codex_login::CodexAuth;
use codex_protocol::user_input::UserInput;
use codex_state::StateRuntime;
use codex_state::decision_provenance::Actor;
use codex_state::decision_provenance::Authority;
use codex_state::decision_provenance::LifecycleStatus;
use codex_state::decision_provenance::PreferenceBoundary;
use codex_state::decision_provenance::PreferenceKind;
use codex_state::decision_provenance::PreferenceStrength;
use codex_state::decision_provenance::PrivacyClass;
use codex_state::decision_provenance::ProvenanceWriteOptions;
use codex_state::decision_provenance::ScopeRef;
use codex_state::decision_provenance::SourceReference;
use codex_state::decision_provenance::Timestamps;
use codex_state::decision_provenance::now;
use std::sync::Arc;
use tempfile::TempDir;

async fn provenance_fixture(
    enabled: bool,
) -> (Arc<Session>, Arc<TurnContext>, Arc<StateRuntime>, TempDir) {
    let codex_home = tempfile::tempdir().expect("create provenance test home");
    let (mut session, turn_context, _rx) = make_session_and_context_with_auth_config_home_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        codex_home.path(),
        move |config| config.decision_provenance.enabled = enabled,
    )
    .await;
    let state_db = StateRuntime::init(
        turn_context.config.sqlite_config().clone(),
        turn_context.config.model_provider_id.clone(),
    )
    .await
    .expect("initialize provenance state runtime");
    Arc::get_mut(&mut session)
        .expect("test session should be uniquely owned")
        .services
        .state_db = Some(Arc::clone(&state_db));
    (session, turn_context, state_db, codex_home)
}

fn confirmed_boundary() -> PreferenceBoundary {
    PreferenceBoundary {
        id: "test-boundary".to_string(),
        kind: PreferenceKind::PreferenceBoundary,
        statement: "Never change generated files without confirmation".to_string(),
        scope: ScopeRef::global(),
        strength: PreferenceStrength::Confirmation,
        authority: Authority::User,
        source: SourceReference::new("test", "turn-boundary"),
        rationale: None,
        confidence: None,
        lifecycle_status: LifecycleStatus::Confirmed,
        timestamps: Timestamps::now(),
        related_memory_record_id: None,
        superseded_by: None,
        privacy: PrivacyClass::Private,
    }
}

async fn record_test_boundary(state_db: &StateRuntime) {
    state_db
        .record_preference_boundary(
            confirmed_boundary(),
            ProvenanceWriteOptions {
                idempotency_key: Some("test-boundary".to_string()),
                actor: Actor::User,
                occurred_at: now(),
            },
        )
        .await
        .expect("record test boundary");
}

#[test]
fn repeating_a_pause_boundary_does_not_look_like_a_conflict() {
    assert!(request_honors_boundary(
        "ask before changing generated files",
        "please ask before changing generated files"
    ));
    assert!(!request_has_explicit_override(
        "please ask before changing generated files"
    ));
}

#[test]
fn repeating_a_prohibition_does_not_look_like_a_conflict() {
    assert!(request_honors_boundary(
        "never modify generated files",
        "do not modify generated files"
    ));
}

#[test]
fn an_explicit_override_is_not_treated_as_honoring_a_boundary() {
    assert!(request_has_explicit_override(
        "override that preference and proceed without asking"
    ));
}

#[tokio::test]
async fn disabled_preflight_does_not_record_a_crossroad_or_notification() {
    let (session, turn_context, state_db, _codex_home) = provenance_fixture(false).await;
    record_test_boundary(&state_db).await;

    let outcome = record_turn_provenance_preflight(
        &session,
        &turn_context,
        &[UserInput::Text {
            text: "please change generated files".to_string(),
            text_elements: Vec::new(),
        }],
    )
    .await;

    assert!(matches!(outcome, ProvenancePreflightOutcome::Continue));
    assert!(
        state_db
            .list_open_crossroads(20)
            .await
            .expect("list crossroads")
            .is_empty()
    );
    assert!(
        state_db
            .list_provenance_notifications(20)
            .await
            .expect("list notifications")
            .is_empty()
    );
}

#[tokio::test]
async fn enabled_preflight_records_a_crossroad_and_notification() {
    let (session, turn_context, state_db, _codex_home) = provenance_fixture(true).await;
    record_test_boundary(&state_db).await;

    let outcome = record_turn_provenance_preflight(
        &session,
        &turn_context,
        &[UserInput::Text {
            text: "please change generated files".to_string(),
            text_elements: Vec::new(),
        }],
    )
    .await;

    assert!(matches!(outcome, ProvenancePreflightOutcome::Blocked));
    assert_eq!(
        state_db
            .list_open_crossroads(20)
            .await
            .expect("list crossroads")
            .len(),
        1
    );
    assert_eq!(
        state_db
            .list_provenance_notifications(20)
            .await
            .expect("list notifications")
            .len(),
        1
    );
}
