use super::ProvenancePreflightOutcome;
use super::record_turn_provenance_preflight;
use super::request_has_explicit_override;
use super::request_honors_boundary;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context_with_auth_config_home_and_rx;
use crate::session::turn_context::TurnContext;
use codex_login::CodexAuth;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use codex_state::StateRuntime;
use codex_state::decision_provenance::Actor;
use codex_state::decision_provenance::Authority;
use codex_state::decision_provenance::CrossroadFilter;
use codex_state::decision_provenance::DecisionFilter;
use codex_state::decision_provenance::EntityType;
use codex_state::decision_provenance::LifecycleStatus;
use codex_state::decision_provenance::PreferenceBoundary;
use codex_state::decision_provenance::PreferenceKind;
use codex_state::decision_provenance::PreferenceStrength;
use codex_state::decision_provenance::PrivacyClass;
use codex_state::decision_provenance::ProvenanceWriteOptions;
use codex_state::decision_provenance::RelationshipKind;
use codex_state::decision_provenance::ScopeRef;
use codex_state::decision_provenance::SourceReference;
use codex_state::decision_provenance::Timestamps;
use codex_state::decision_provenance::now;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

async fn provenance_fixture(
    enabled: bool,
    model_provider_base_url: Option<String>,
) -> (Arc<Session>, Arc<TurnContext>, Arc<StateRuntime>, TempDir) {
    provenance_fixture_with_options(
        enabled,
        /*git_intent_bridge*/ false,
        model_provider_base_url,
        None,
    )
    .await
}

async fn provenance_fixture_with_options(
    enabled: bool,
    git_intent_bridge: bool,
    model_provider_base_url: Option<String>,
    cwd: Option<PathBuf>,
) -> (Arc<Session>, Arc<TurnContext>, Arc<StateRuntime>, TempDir) {
    let codex_home = tempfile::tempdir().expect("create provenance test home");
    let (mut session, turn_context, _rx) = make_session_and_context_with_auth_config_home_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        codex_home.path(),
        move |config| {
            config.decision_provenance.enabled = enabled;
            config.decision_provenance.git_intent_bridge = git_intent_bridge;
            config.model_provider.base_url = model_provider_base_url;
            if let Some(cwd) = cwd {
                config.cwd = AbsolutePathBuf::from_absolute_path(cwd)
                    .expect("provenance test cwd should be absolute");
            }
        },
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
    let (session, turn_context, state_db, _codex_home) = provenance_fixture(false, None).await;
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
async fn disabled_run_turn_preserves_model_flow_without_provenance() {
    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-disabled"),
            ev_assistant_message("msg-disabled", "normal response"),
            ev_completed("resp-disabled"),
        ]),
    )
    .await;
    let (session, turn_context, state_db, _codex_home) =
        provenance_fixture(false, Some(format!("{}/v1", server.uri()))).await;
    record_test_boundary(&state_db).await;

    let result = crate::session::turn::run_turn(
        session.clone(),
        turn_context,
        vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "please change generated files".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }],
        /*prewarmed_client_session*/ None,
        CancellationToken::new(),
    )
    .await
    .expect("disabled turn should complete through the normal model path");

    assert_eq!(result, Some("normal response".to_string()));
    let _ = response_mock.single_request();
    let history = session.clone_history().await;
    assert!(
        history
            .raw_items()
            .any(|item| { matches!(item, ResponseItem::Message { role, .. } if role == "user") })
    );
    assert!(
        history.raw_items().any(|item| {
            matches!(item, ResponseItem::Message { role, .. } if role == "assistant")
        })
    );
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
async fn enabled_run_turn_records_a_crossroad_and_notification_before_model_work() {
    let (session, turn_context, state_db, _codex_home) = provenance_fixture(true, None).await;
    record_test_boundary(&state_db).await;

    let result = crate::session::turn::run_turn(
        session,
        turn_context,
        vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "please change generated files".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }],
        /*prewarmed_client_session*/ None,
        CancellationToken::new(),
    )
    .await
    .expect("blocked turn should complete without model work");

    assert_eq!(result, None);
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

fn run_git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .current_dir(repo)
        .status()
        .expect("run Git command");
    assert_eq!(status.code(), Some(0), "Git command failed: {args:?}");
}

fn create_must_intent_note(repo: &Path) -> String {
    run_git(repo, &["init", "-q", "--initial-branch=main"]);
    run_git(
        repo,
        &[
            "-c",
            "user.name=Codex Tests",
            "-c",
            "user.email=codex-tests@example.com",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "preserve generated API contract",
        ],
    );
    let commit = String::from_utf8(
        Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .expect("read commit")
            .stdout,
    )
    .expect("commit should be UTF-8")
    .trim()
    .to_string();
    run_git(
        repo,
        &[
            "notes",
            "--ref=refs/notes/intention",
            "add",
            "-m",
            "intent_priority: must\nsummary: Preserve generated files for the API contract\ndecision: Keep the generated API contract unchanged unless explicitly approved.",
            &commit,
        ],
    );
    commit
}

#[tokio::test]
async fn enabled_git_intent_bridge_records_a_crossroad_before_model_work() {
    let repo = tempfile::tempdir().expect("create Git repository");
    let commit = create_must_intent_note(repo.path());
    let (session, turn_context, state_db, _codex_home) = provenance_fixture_with_options(
        /*enabled*/ true,
        /*git_intent_bridge*/ true,
        None,
        Some(repo.path().to_path_buf()),
    )
    .await;

    let result = crate::session::turn::run_turn(
        session,
        turn_context,
        vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "please modify generated files for the API contract".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }],
        /*prewarmed_client_session*/ None,
        CancellationToken::new(),
    )
    .await
    .expect("Git intent bridge should pause before model work");

    assert_eq!(result, None);
    let crossroads = state_db
        .list_open_crossroads(20)
        .await
        .expect("list Git intent crossroads");
    assert_eq!(crossroads.len(), 1);
    let crossroad = &crossroads[0];
    assert!(
        crossroad
            .source_refs
            .iter()
            .any(|source| { source.reference == format!("refs/notes/intention@{commit}") })
    );
    assert!(crossroad.question.contains("prior must-level Git intent"));
    assert!(
        state_db
            .relationships_for(EntityType::Crossroad, &crossroad.id)
            .await
            .expect("list Git intent relationships")
            .iter()
            .any(|relationship| {
                relationship.relation == RelationshipKind::ConstrainedBy
                    && relationship.to_type == EntityType::Commit
                    && relationship.to_id == commit
            })
    );
    assert_eq!(
        state_db
            .list_provenance_notifications(20)
            .await
            .expect("list Git intent notifications")
            .len(),
        1
    );
}

#[tokio::test]
async fn git_intent_bridge_requires_both_opt_in_settings() {
    let repo = tempfile::tempdir().expect("create Git repository");
    create_must_intent_note(repo.path());
    let (session, turn_context, state_db, _codex_home) = provenance_fixture_with_options(
        /*enabled*/ false,
        /*git_intent_bridge*/ true,
        None,
        Some(repo.path().to_path_buf()),
    )
    .await;

    let outcome = record_turn_provenance_preflight(
        &session,
        &turn_context,
        &[UserInput::Text {
            text: "please modify generated files for the API contract".to_string(),
            text_elements: Vec::new(),
        }],
    )
    .await;

    assert!(matches!(outcome, ProvenancePreflightOutcome::Continue));
    assert!(
        state_db
            .list_open_crossroads(20)
            .await
            .expect("list disabled crossroads")
            .is_empty()
    );
}

#[tokio::test]
async fn explicit_git_intent_override_records_a_user_decision_and_resolves_the_crossroad() {
    let repo = tempfile::tempdir().expect("create Git repository");
    let commit = create_must_intent_note(repo.path());
    let (session, turn_context, state_db, _codex_home) = provenance_fixture_with_options(
        /*enabled*/ true,
        /*git_intent_bridge*/ true,
        None,
        Some(repo.path().to_path_buf()),
    )
    .await;

    let outcome = record_turn_provenance_preflight(
        &session,
        &turn_context,
        &[UserInput::Text {
            text: "please override the prior Git intent and modify generated files for the API contract"
                .to_string(),
            text_elements: Vec::new(),
        }],
    )
    .await;

    assert!(matches!(outcome, ProvenancePreflightOutcome::Continue));
    let decisions = state_db
        .list_decisions(DecisionFilter::default())
        .await
        .expect("list override decision");
    assert_eq!(decisions.len(), 1);
    let decision = &decisions[0];
    assert_eq!(decision.actor, Actor::User);
    assert_eq!(
        decision.approval_state,
        codex_state::decision_provenance::ApprovalState::Approved
    );
    assert!(
        decision
            .summary
            .contains("overrides the prior must-level Git intent")
    );
    assert!(
        state_db
            .relationships_for(EntityType::Commit, &commit)
            .await
            .expect("list override relationships")
            .iter()
            .any(|relationship| {
                relationship.from_type == EntityType::Decision
                    && relationship.from_id == decision.id
                    && relationship.relation == RelationshipKind::ConflictsWith
            })
    );
    assert!(
        state_db
            .list_crossroads(CrossroadFilter {
                status: Some(codex_state::decision_provenance::CrossroadStatus::Resolved),
                ..CrossroadFilter::default()
            })
            .await
            .expect("list resolved crossroad")
            .iter()
            .any(|crossroad| crossroad.id == decision.parent_crossroad_id.clone().unwrap())
    );
}
