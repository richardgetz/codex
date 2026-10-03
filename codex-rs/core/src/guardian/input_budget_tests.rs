//! Exercises reviewer history safety at cancellation and budget boundaries.

use super::*;
use codex_guardian_context::ContextPresentation;
use codex_guardian_context::ContextProfile;
use codex_guardian_context::ContextSection;
use codex_guardian_context::ConversationTranscriptEntry;
use codex_guardian_context::ConversationTranscriptEntryKind;
use codex_guardian_context::PlannedAction;
use codex_guardian_context::PlannedActionKind;
use codex_guardian_context::SectionContributor;
use codex_guardian_context::SectionError;
use codex_guardian_context::SectionInput;
use codex_guardian_context::SectionRegistry;
use codex_guardian_context::SectionScope;
use codex_guardian_context::TranscriptContent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use pretty_assertions::assert_eq;
use std::sync::Arc;

struct NativeMessageContributor(ResponseItem);

impl SectionContributor for NativeMessageContributor {
    fn scope(&self) -> SectionScope {
        SectionScope::Shared
    }

    fn contribute(
        &self,
        _input: &SectionInput<'_>,
    ) -> Result<Option<ContextSection>, SectionError> {
        let original_bytes = serde_json::to_vec(&self.0)
            .expect("serialize native test message")
            .len();
        Ok(Some(ContextSection::ConversationTranscript {
            items: vec![ConversationTranscriptEntry {
                kind: ConversationTranscriptEntryKind::User,
                content: TranscriptContent::AgentMessage(Box::new(self.0.clone())),
                original_bytes,
                retained_source: None,
            }],
        }))
    }
}

fn required_context(text: String) -> ComposedContext {
    let action = PlannedAction {
        tool_descriptions: None,
        json: text,
        kind: PlannedActionKind::Command,
        reason: None,
    };
    let context = super::super::prompt::collect_guardian_context(
        &Vec::<ResponseItem>::new(),
        super::super::GUARDIAN_MAX_TOOL_ENTRY_TOKENS,
        &[],
        &[],
        Some(&action),
        /*permissions*/ None,
        /*node_repl*/ None,
    )
    .expect("collect required context");
    let transcript = ContextProfile::synchronous()
        .render_transcript(context.transcript_entries(), /*entry_number_offset*/ 0);
    context
        .compose(
            ContextPresentation::SyncFull {
                session_id: "test-parent",
            },
            transcript,
        )
        .expect("compose required context")
}

#[tokio::test]
async fn cancelled_startup_does_not_record_unselected_review_evidence() {
    let (session, turn, events) = crate::session::tests::make_session_and_context_with_rx().await;
    let context = required_context("unselected review evidence ".repeat(/*n*/ 10_000));
    let content = context.clone().into_user_inputs().unwrap();
    session
        .services
        .thread_extension_data
        .insert(PendingReviewContext(context));
    session
        .set_session_startup_prewarm(
            crate::session::startup_prewarm::SessionStartupPrewarmHandle::new(
                tokio::spawn(std::future::pending()),
                std::time::Instant::now(),
                crate::client::WEBSOCKET_CONNECT_TIMEOUT,
            ),
        )
        .await;
    session
        .spawn_task(
            turn,
            vec![TurnInput::UserInput {
                metadata: Default::default(),
                content,
                client_id: None,
            }],
            crate::tasks::RegularTask::new(),
        )
        .await;
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(started.msg, EventMsg::TurnStarted(_)));
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;

    let history = session.clone_history().await;
    let recorded = history.raw_items().collect::<Vec<_>>();
    assert!(
        !serde_json::to_string(&recorded)
            .unwrap()
            .contains("unselected review evidence")
    );
    assert!(
        session
            .services
            .thread_extension_data
            .get::<PendingReviewContext>()
            .is_some()
    );
}

#[tokio::test]
async fn feasibility_rejects_required_evidence_inside_the_reserved_margin() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let context = required_context("required action".to_owned());
    let base = session.get_prompt_base_instructions().await;
    let prefix = codex_protocol::protocol::TruncationPolicy::Bytes(base.text.len()).token_budget();
    let model = Arc::make_mut(&mut Arc::make_mut(&mut turn.initial_settings).model_info);
    model.effective_context_window_percent = 100;
    Arc::make_mut(&mut turn.config).model_context_window =
        Some(i64::try_from(context.estimated_tokens() + prefix + 128).unwrap());
    session
        .services
        .thread_extension_data
        .insert(PendingReviewContext(context));

    assert!(check_pending(&session, &turn).await.is_err());
}

#[tokio::test]
async fn finalization_overflow_marks_the_reviewer_exhausted() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config).model_context_window = Some(1);
    let session = Arc::new(session);
    let step = session
        .capture_step_context(Arc::new(turn), &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let context = required_context("required action".to_owned());
    let mut input = vec![TurnInput::UserInput {
        metadata: Default::default(),
        content: context.clone().into_user_inputs().unwrap(),
        client_id: None,
    }];
    session
        .services
        .thread_extension_data
        .insert(PendingReviewContext(context));

    assert!(
        finalize(&session, &step, &mut input, HistoryTruncation::Preserve)
            .await
            .is_err()
    );
    assert!(
        session
            .services
            .thread_extension_data
            .get::<super::super::request_budget::ExhaustedReviewBudget>()
            .is_some()
    );
}

#[tokio::test]
async fn finalization_preserves_native_encrypted_user_message_in_request() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let step = session
        .capture_step_context(Arc::new(turn), &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let encrypted_message = ResponseItem::Message {
        id: None,
        role: "user".to_owned(),
        content: vec![ContentItem::EncryptedContent {
            encrypted_content: "opaque-guardian-context".to_owned(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    // Inject a native user envelope to exercise the defensive branch; the normal
    // transcript collector omits encrypted user content before composition.
    let profile = ContextProfile::synchronous();
    let history = Vec::<ResponseItem>::new();
    let mut registry = SectionRegistry::default();
    registry.register(NativeMessageContributor(encrypted_message.clone()));
    let sections = registry
        .prepare(&SectionInput {
            target: profile.target,
            history: &history,
            transcript: &profile.transcript,
            root_conversation: &[],
            trusted_user_answers: &[],
            planned_action: None,
            permissions: None,
            previous_reviews: None,
            trusted_tool: None,
            trusted_skill_paths: &[],
            images: None,
            node_repl: None,
        })
        .unwrap();
    let transcript = profile.render_transcript(
        sections.transcript_entries(),
        /*entry_number_offset*/ 0,
    );
    let context = sections
        .compose(
            ContextPresentation::SyncFull {
                session_id: "test-parent",
            },
            transcript,
        )
        .unwrap();
    session
        .services
        .thread_extension_data
        .insert(PendingReviewContext(context));
    let mut input = vec![TurnInput::UserInput {
        metadata: Default::default(),
        content: Vec::new(),
        client_id: None,
    }];

    finalize(&session, &step, &mut input, HistoryTruncation::Preserve)
        .await
        .unwrap();
    let prompt_input = input
        .into_iter()
        .filter_map(|item| match item {
            TurnInput::ResponseItem(envelope) => Some(envelope.item),
            TurnInput::UserInput { .. }
            | TurnInput::FunctionCallOutput(_)
            | TurnInput::InterAgentCommunication(_) => None,
        })
        .collect::<Vec<_>>();
    let prompt = build_prompt(
        prompt_input,
        &step,
        session.get_prompt_base_instructions().await,
    );
    let metadata = session
        .responses_metadata(&step, CodexResponsesRequestKind::Turn)
        .await;
    let request = session
        .services
        .model_client
        .build_responses_request(
            &prompt,
            &step.settings.model_info,
            /*effort*/ None,
            ReasoningSummary::None,
            /*service_tier*/ None,
            &metadata,
            /*include_internal*/ true,
        )
        .unwrap();

    let delivered = request
        .input
        .iter()
        .filter(|item| *item == &encrypted_message)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(delivered, vec![encrypted_message]);
}

#[tokio::test]
async fn compacted_review_restores_originals_once_and_persists_the_request_prefix() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let step = session
        .capture_step_context(Arc::new(turn), &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    session
        .record_user_prompt_and_emit_turn_item(
            &step.turn,
            &step.settings.model_info,
            &[codex_protocol::user_input::UserInput::Text {
                text: "Do not publish.".to_owned(),
                text_elements: vec![],
            }],
            /*client_id*/ None,
            crate::session::UserInputMetadata {
                acceptance_order: Some(0),
                ..Default::default()
            },
            codex_thread_store::PersistContext::Standard,
        )
        .await;
    let context = super::super::prompt::build_guardian_prompt_items(
        &session,
        /*retry_reason*/ None,
        super::super::GuardianApprovalRequest::RequestPermissions {
            id: "review".to_owned(),
            environment_id: "local".to_owned(),
            turn_id: "parent".to_owned(),
            reason: None,
            permissions: Default::default(),
        },
        super::super::prompt::GuardianPromptMode::Full,
    )
    .await
    .unwrap()
    .context;
    step.turn.extension_data.insert(RetainedReviewContext {
        context: context.retained_instructions(),
        history_version: session.clone_history().await.history_version(),
    });
    // Simulate a reviewer compaction that discarded the earlier user-message block.
    session
        .replace_history(vec![], /*reference_context_item*/ None)
        .await;
    let mut prompt = build_prompt(vec![], &step, session.get_prompt_base_instructions().await);
    let metadata = session
        .responses_metadata(&step, CodexResponsesRequestKind::Turn)
        .await;
    super::super::request_budget::prepare_prompt(&session, &mut prompt, &step, &metadata)
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&prompt.input)
            .unwrap()
            .contains("Do not publish.")
    );
    let persisted = session
        .clone_history()
        .await
        .for_prompt(&step.settings.model_info.input_modalities);
    assert_eq!(prompt.input, persisted);
    assert!(
        persisted
            .iter()
            .all(crate::context::is_guardian_context_message)
    );
    assert!(
        !persisted
            .iter()
            .any(crate::context::is_user_authorization_message)
    );
    super::super::request_budget::prepare_prompt(&session, &mut prompt, &step, &metadata)
        .await
        .unwrap();
    assert_eq!(prompt.input, persisted);
}
