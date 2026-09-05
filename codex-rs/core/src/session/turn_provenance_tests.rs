use super::provenance_issue_key;
use super::record_turn_provenance_preflight;
use super::request_fingerprint;
use super::request_honors_boundary;
use super::stable_provenance_id;
use super::user_input_text_for_provenance;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context_with_auth_config_home_and_rx;
use crate::session::turn_context::TurnContext;
use codex_login::CodexAuth;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::ByteRange;
use codex_protocol::user_input::TextElement;
use codex_protocol::user_input::UserInput;
use codex_state::StateRuntime;
use codex_state::decision_provenance::Actor;
use codex_state::decision_provenance::Authority;
use codex_state::decision_provenance::Crossroad;
use codex_state::decision_provenance::CrossroadFilter;
use codex_state::decision_provenance::CrossroadOption;
use codex_state::decision_provenance::CrossroadStatus;
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
}

#[test]
fn repeating_a_prohibition_does_not_look_like_a_conflict() {
    assert!(request_honors_boundary(
        "never modify generated files",
        "do not modify generated files"
    ));
}

#[test]
fn provenance_input_truncation_preserves_utf8() {
    let input = format!("{}終", "é".repeat(2_100));
    let text = user_input_text_for_provenance(&[UserInput::Text {
        text: input,
        text_elements: Vec::new(),
    }]);

    assert!(text.len() <= 4_096);
    assert!(text.is_char_boundary(text.len()));
    assert!(text.starts_with("éé"));
}

#[test]
fn provenance_input_truncation_stays_within_byte_cap_for_wide_chars() {
    let input = format!("{}€", "x".repeat(4_095));
    let text = user_input_text_for_provenance(&[UserInput::Text {
        text: input,
        text_elements: Vec::new(),
    }]);

    assert_eq!(text.len(), 4_095);
    assert!(text.is_char_boundary(text.len()));
}

#[test]
fn provenance_issue_key_frames_types_and_delimiters() {
    let request_key = "request-hash";
    let boundary_git_name =
        provenance_issue_key("task", &["git:abc".to_string()], &[], request_key);
    let git_commit_name = provenance_issue_key("task", &[], &["abc".to_string()], request_key);
    let one_comma_id = provenance_issue_key("task", &["a,b".to_string()], &[], request_key);
    let two_ids = provenance_issue_key(
        "task",
        &["a".to_string(), "b".to_string()],
        &[],
        request_key,
    );

    assert_ne!(boundary_git_name, git_commit_name);
    assert_ne!(one_comma_id, two_ids);
}

#[test]
fn request_fingerprint_preserves_text_normalization_and_hashes_structured_inputs() {
    fn text(value: &str) -> UserInput {
        UserInput::Text {
            text: value.to_string(),
            text_elements: Vec::new(),
        }
    }

    fn with_structured_input(input: UserInput) -> Vec<UserInput> {
        vec![text("please change generated files"), input]
    }

    assert_eq!(
        request_fingerprint(&[text(" please\nchange   generated files ")]),
        request_fingerprint(&[text("please change generated files")]),
    );

    let image_a = with_structured_input(UserInput::Image {
        image_url: "data:image/png;base64,dataAA".to_string(),
        detail: Some(ImageDetail::Auto),
    });
    let image_b = with_structured_input(UserInput::Image {
        image_url: "data:image/png;base64,dataAg".to_string(),
        detail: Some(ImageDetail::Auto),
    });
    assert_ne!(request_fingerprint(&image_a), request_fingerprint(&image_b));

    let image_detail_a = with_structured_input(UserInput::Image {
        image_url: "data:image/png;base64,same".to_string(),
        detail: Some(ImageDetail::Low),
    });
    let image_detail_b = with_structured_input(UserInput::Image {
        image_url: "data:image/png;base64,same".to_string(),
        detail: Some(ImageDetail::High),
    });
    assert_ne!(
        request_fingerprint(&image_detail_a),
        request_fingerprint(&image_detail_b)
    );

    let skill_a = with_structured_input(UserInput::Skill {
        name: "testflight-feedback".to_string(),
        path: "/skills/testflight-feedback/SKILL.md".into(),
    });
    let skill_b = with_structured_input(UserInput::Skill {
        name: "other-skill".to_string(),
        path: "/skills/other-skill/SKILL.md".into(),
    });
    assert_ne!(request_fingerprint(&skill_a), request_fingerprint(&skill_b));

    let mention_a = with_structured_input(UserInput::Mention {
        name: "first-app".to_string(),
        path: "app://first".to_string(),
    });
    let mention_b = with_structured_input(UserInput::Mention {
        name: "second-app".to_string(),
        path: "app://second".to_string(),
    });
    assert_ne!(
        request_fingerprint(&mention_a),
        request_fingerprint(&mention_b)
    );

    let text_element_a = vec![UserInput::Text {
        text: "please change generated files".to_string(),
        text_elements: vec![TextElement::new(
            ByteRange { start: 0, end: 6 },
            Some("first".to_string()),
        )],
    }];
    let text_element_b = vec![UserInput::Text {
        text: "please change generated files".to_string(),
        text_elements: vec![TextElement::new(
            ByteRange { start: 0, end: 6 },
            Some("second".to_string()),
        )],
    }];
    assert_ne!(
        request_fingerprint(&text_element_a),
        request_fingerprint(&text_element_b)
    );

    let exhaustive_a = with_structured_input(UserInput::Audio {
        audio_url: "data:audio/wav;base64,AA".to_string(),
    });
    let exhaustive_b = with_structured_input(UserInput::Audio {
        audio_url: "data:audio/wav;base64,Ag".to_string(),
    });
    assert_ne!(
        request_fingerprint(&exhaustive_a),
        request_fingerprint(&exhaustive_b)
    );

    let local_image_a = with_structured_input(UserInput::LocalImage {
        path: "/tmp/first.png".into(),
        detail: Some(ImageDetail::Original),
    });
    let local_image_b = with_structured_input(UserInput::LocalImage {
        path: "/tmp/second.png".into(),
        detail: Some(ImageDetail::Original),
    });
    assert_ne!(
        request_fingerprint(&local_image_a),
        request_fingerprint(&local_image_b)
    );

    let local_audio_a = with_structured_input(UserInput::LocalAudio {
        path: "/tmp/first.wav".into(),
    });
    let local_audio_b = with_structured_input(UserInput::LocalAudio {
        path: "/tmp/second.wav".into(),
    });
    assert_ne!(
        request_fingerprint(&local_audio_a),
        request_fingerprint(&local_audio_b)
    );

    let image = UserInput::Image {
        image_url: "data:image/png;base64,same".to_string(),
        detail: Some(ImageDetail::Auto),
    };
    let partitioned_a = vec![text("move"), image.clone(), text("A to B")];
    let partitioned_b = vec![text("move A"), image, text("to B")];
    assert_ne!(
        request_fingerprint(&partitioned_a),
        request_fingerprint(&partitioned_b)
    );
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

    assert!(outcome.advisory.is_none());
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
async fn failed_crossroad_persistence_does_not_inject_a_nonexistent_advisory() {
    let (session, turn_context, state_db, _codex_home) = provenance_fixture(true, None).await;
    record_test_boundary(&state_db).await;
    let user_input = vec![UserInput::Text {
        text: "please change generated files".to_string(),
        text_elements: Vec::new(),
    }];
    let task_id = session.thread_id().to_string();
    let issue_key = provenance_issue_key(
        &task_id,
        &["test-boundary".to_string()],
        &[],
        &request_fingerprint(&user_input),
    );
    state_db
        .record_crossroad(
            Crossroad {
                id: "unrelated-crossroad".to_string(),
                request_ref: None,
                task_ref: None,
                project_ref: None,
                session_id: None,
                question: "Existing unrelated record".to_string(),
                options: vec![CrossroadOption {
                    id: "option".to_string(),
                    label: "Discuss".to_string(),
                    summary: None,
                    tradeoffs: Vec::new(),
                }],
                recommended_option: None,
                affected_boundary_ids: Vec::new(),
                constraint_ids: Vec::new(),
                expected_tradeoffs: Vec::new(),
                authority_required: None,
                status: CrossroadStatus::Open,
                actor: Actor::System,
                source_refs: Vec::new(),
                linked_scratchpad_wait_id: None,
                timestamps: Timestamps::now(),
                privacy: PrivacyClass::Private,
            },
            ProvenanceWriteOptions {
                idempotency_key: Some(format!("preflight-crossroad:{task_id}:{issue_key}")),
                actor: Actor::System,
                occurred_at: now(),
            },
        )
        .await
        .expect("seed conflicting idempotency key");

    let outcome = record_turn_provenance_preflight(&session, &turn_context, &user_input).await;

    assert!(outcome.advisory.is_none());
    let expected_id =
        stable_provenance_id("crossroad", &format!("preflight:{task_id}:{issue_key}"));
    assert!(
        state_db
            .get_crossroad(&expected_id)
            .await
            .expect("read failed crossroad")
            .is_none()
    );
}

#[tokio::test]
async fn enabled_run_turn_records_advisory_and_continues_model_work() {
    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-advisory"),
            ev_assistant_message("msg-advisory", "normal response"),
            ev_completed("resp-advisory"),
        ]),
    )
    .await;
    let (session, turn_context, state_db, _codex_home) =
        provenance_fixture(true, Some(format!("{}/v1", server.uri()))).await;
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
    .expect("advisory turn should complete through the normal model path");

    assert_eq!(result, Some("normal response".to_string()));
    let _ = response_mock.single_request();
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
async fn enabled_git_intent_bridge_records_advisory_before_model_work() {
    let repo = tempfile::tempdir().expect("create Git repository");
    let commit = create_must_intent_note(repo.path());
    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-git-advisory"),
            ev_assistant_message("msg-git-advisory", "normal response"),
            ev_completed("resp-git-advisory"),
        ]),
    )
    .await;
    let (session, turn_context, state_db, _codex_home) = provenance_fixture_with_options(
        /*enabled*/ true,
        /*git_intent_bridge*/ true,
        Some(format!("{}/v1", server.uri())),
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
    .expect("Git intent bridge should continue through model work");

    assert_eq!(result, Some("normal response".to_string()));
    let _ = response_mock.single_request();
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
    assert!(crossroad.question.contains("prior recorded Git intent"));
    assert!(
        state_db
            .relationships_for(EntityType::Crossroad, &crossroad.id)
            .await
            .expect("list Git intent relationships")
            .iter()
            .any(|relationship| {
                relationship.relation == RelationshipKind::ConsideredNotDecisive
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

    assert!(outcome.advisory.is_none());
    assert!(
        state_db
            .list_open_crossroads(20)
            .await
            .expect("list disabled crossroads")
            .is_empty()
    );
}

#[tokio::test]
async fn override_words_do_not_create_a_fake_user_decision() {
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

    assert!(outcome.advisory.is_some());
    let decisions = state_db
        .list_decisions(DecisionFilter::default())
        .await
        .expect("list override decision");
    assert!(decisions.is_empty());
    assert!(
        state_db
            .relationships_for(EntityType::Commit, &commit)
            .await
            .expect("list override relationships")
            .iter()
            .any(|relationship| relationship.to_id == commit)
    );
    assert_eq!(
        state_db
            .list_open_crossroads(20)
            .await
            .expect("list open crossroad")
            .len(),
        1
    );
}

#[tokio::test]
async fn repeated_requests_deduplicate_but_distinct_requests_remain_separate() {
    let (session, turn_context, state_db, _codex_home) = provenance_fixture(true, None).await;
    record_test_boundary(&state_db).await;

    let input = |text: &str| {
        vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }]
    };
    let first = record_turn_provenance_preflight(
        &session,
        &turn_context,
        &input("please change generated files"),
    )
    .await;
    let repeated = record_turn_provenance_preflight(
        &session,
        &turn_context,
        &input("please change generated files"),
    )
    .await;
    let distinct = record_turn_provenance_preflight(
        &session,
        &turn_context,
        &input("please delete generated files"),
    )
    .await;

    assert!(first.advisory.is_some());
    assert!(repeated.advisory.is_some());
    assert!(distinct.advisory.is_some());
    assert_eq!(
        state_db
            .list_crossroads(CrossroadFilter::default())
            .await
            .expect("list crossroads")
            .len(),
        2
    );
    assert_eq!(
        state_db
            .list_provenance_notifications(20)
            .await
            .expect("list notifications")
            .len(),
        2
    );
}

#[tokio::test]
async fn long_requests_with_shared_retrieval_prefix_remain_distinct() {
    let (session, turn_context, state_db, _codex_home) = provenance_fixture(true, None).await;
    record_test_boundary(&state_db).await;
    let shared_prefix = "please change generated files ".repeat(200);
    let input = |suffix: &str| {
        vec![UserInput::Text {
            text: format!("{shared_prefix}{suffix}"),
            text_elements: Vec::new(),
        }]
    };

    let first =
        record_turn_provenance_preflight(&session, &turn_context, &input("TargetFileAlpha.rs"))
            .await;
    let second =
        record_turn_provenance_preflight(&session, &turn_context, &input("TargetFileBeta.rs"))
            .await;

    assert!(first.advisory.is_some());
    assert!(second.advisory.is_some());
    assert_eq!(
        state_db
            .list_crossroads(CrossroadFilter::default())
            .await
            .expect("list long-request crossroads")
            .len(),
        2
    );
}

#[tokio::test]
async fn reviewed_crossroads_are_not_reinjected_until_revisited() {
    let (session, turn_context, state_db, _codex_home) = provenance_fixture(true, None).await;
    record_test_boundary(&state_db).await;
    let input = [UserInput::Text {
        text: "please change generated files".to_string(),
        text_elements: Vec::new(),
    }];

    let first = record_turn_provenance_preflight(&session, &turn_context, &input).await;
    assert!(first.advisory.is_some());
    let crossroad = state_db
        .list_open_crossroads(20)
        .await
        .expect("list open crossroad")
        .pop()
        .expect("crossroad recorded");
    state_db
        .transition_crossroad(
            &crossroad.id,
            CrossroadStatus::Resolved,
            ProvenanceWriteOptions {
                idempotency_key: Some("test-reviewed-crossroad".to_string()),
                actor: Actor::User,
                occurred_at: now(),
            },
        )
        .await
        .expect("review crossroad");

    let reviewed = record_turn_provenance_preflight(&session, &turn_context, &input).await;
    assert!(reviewed.advisory.is_none());
    assert_eq!(
        state_db
            .list_provenance_notifications(20)
            .await
            .expect("list notifications")
            .len(),
        1
    );
}
