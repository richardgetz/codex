use super::AGENT_FINAL_MESSAGE_PREFIX;
use super::ConversationState;
use super::HANDOFF_STREAM_TRUNCATION_MARKER;
use super::RealtimeConversationManager;
use super::RealtimeConversationManagerState;
use super::RealtimeHandoffAdmission;
use super::RealtimeHandoffAdmissions;
use super::RealtimeHandoffDeduper;
use super::RealtimeHandoffState;
use super::RealtimeInputTaskExit;
use super::RealtimeOutbound;
use super::RealtimePendingOutbound;
use super::RealtimeSessionKind;
use super::RealtimeStreamedItem;
use super::classify_realtime_input_error;
use super::classify_realtime_input_error_with_pending;
use super::handoff::REALTIME_HANDOFF_DEDUPE_CAPACITY;
use super::realtime_delegation_from_handoff;
use super::realtime_delegation_with_routing_input;
use super::realtime_request_headers;
use super::realtime_text_from_handoff_request;
use super::wrap_realtime_delegation_input;
use crate::context::REALTIME_DELEGATION_MAX_ESTIMATED_TOKENS;
use crate::context::RealtimeDelegationSource;
use async_channel::bounded;
use codex_api::ApiError;
use codex_api::RealtimeEventParser;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::CodexResponseHandoffMode;
use codex_protocol::protocol::ConversationTextParams;
use codex_protocol::protocol::ConversationTextRole;
use codex_protocol::protocol::RealtimeHandoffRequested;
use codex_protocol::protocol::RealtimeTranscriptEntry;
use codex_utils_string::approx_token_count;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn shutdown_cancels_realtime_start_before_state_installation() {
    let manager = RealtimeConversationManager::new();
    let stop_token = CancellationToken::new();
    *manager
        .starting_stop_token
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(stop_token.clone());

    manager.shutdown().await.expect("shutdown should succeed");
    assert!(stop_token.is_cancelled());
}

#[test]
fn deduplicates_repeated_realtime_handoff_ids() {
    let mut deduper = RealtimeHandoffDeduper::default();

    assert!(!deduper.is_duplicate(""));
    assert!(!deduper.is_duplicate(""));
    assert!(!deduper.is_duplicate("handoff-1"));
    assert!(deduper.is_duplicate("handoff-1"));
    assert!(!deduper.is_duplicate("handoff-2"));
    for index in 0..256 {
        assert!(!deduper.is_duplicate(&format!("handoff-extra-{index}")));
    }
    assert!(deduper.is_duplicate("handoff-1"));
}

#[test]
fn realtime_handoff_dedupe_evicts_old_ids() {
    let mut deduper = RealtimeHandoffDeduper::default();

    assert!(!deduper.is_duplicate("handoff-1"));
    for index in 0..REALTIME_HANDOFF_DEDUPE_CAPACITY {
        assert!(!deduper.is_duplicate(&format!("handoff-extra-{index}")));
    }
    assert!(!deduper.is_duplicate("handoff-1"));
}

#[tokio::test]
async fn turn_retirement_rejects_late_handoff_with_held_route_permit() {
    let admissions = RealtimeHandoffAdmissions::default();
    admissions.retire_all().await;

    let gate = Arc::new(RealtimeHandoffAdmission::new());
    let held_permit = gate
        .acquire_route_permit(RealtimeDelegationSource::Handoff)
        .await
        .expect("route permit is acquired before retirement wins");

    assert!(!admissions.register(Arc::clone(&gate)).await);
    drop(held_permit);
    assert!(
        gate.acquire_route_permit(RealtimeDelegationSource::Handoff)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn misalignment_retirement_stays_on_originating_session_and_shutdown_retires_current_gate() {
    let (output_tx, _output_rx) = bounded(1);
    let route_handoffs = Arc::new(RealtimeHandoffAdmission::new());
    let handoff = RealtimeHandoffState {
        output_tx,
        output_send_gate: Arc::new(Semaphore::new(1)),
        last_output: Arc::new(Mutex::new(None)),
        stream: Arc::new(Mutex::new(Default::default())),
        suppress_preambles: false,
        suppress_non_final_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        client_managed_handoffs: false,
        codex_responses_as_items: false,
        codex_response_item_prefix: None,
        backend_reasoning_status: false,
        codex_response_handoff_mode: CodexResponseHandoffMode::Thinking,
        codex_response_handoff_channel_prefixes: Arc::new(BTreeMap::new()),
        session_kind: RealtimeSessionKind::V1,
        event_parser: RealtimeEventParser::V1,
    };
    let manager = RealtimeConversationManager {
        state: Mutex::new(RealtimeConversationManagerState {
            conversation: Some(ConversationState {
                audio_tx: bounded(1).0,
                text_tx: bounded(1).0,
                session_kind: RealtimeSessionKind::V1,
                handoff,
                input_task: tokio::spawn(async {}),
                fanout_task: None,
                realtime_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                route_handoffs: Arc::clone(&route_handoffs),
                flush_transcript_tail_on_session_end: false,
                stop_token: CancellationToken::new(),
            }),
            mode_instructions: None,
        }),
        realtime_submission_sender: Mutex::new(None),
        starting_stop_token: std::sync::Mutex::new(None),
    };

    assert!(manager.running_state().await.is_some());
    let originating_gate = Arc::clone(&route_handoffs);
    let replacement_gate = Arc::new(RealtimeHandoffAdmission::new());
    manager
        .state
        .lock()
        .await
        .conversation
        .as_mut()
        .expect("conversation remains installed")
        .route_handoffs = Arc::clone(&replacement_gate);

    originating_gate.retire().await;
    assert!(
        originating_gate
            .acquire_route_permit(RealtimeDelegationSource::Handoff)
            .await
            .is_none()
    );
    let replacement_permit = replacement_gate
        .acquire_route_permit(RealtimeDelegationSource::Handoff)
        .await
        .expect("late old-session failures must not retire the replacement gate");
    drop(replacement_permit);

    manager.shutdown().await.expect("shutdown should succeed");
    assert!(
        replacement_gate
            .retired
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert!(
        replacement_gate
            .acquire_route_permit(RealtimeDelegationSource::TranscriptTailFlush)
            .await
            .is_none()
    );
    manager
        .shutdown()
        .await
        .expect("repeated shutdown should succeed");
}

#[tokio::test]
async fn shutdown_allows_one_final_transcript_tail_and_rejects_handoffs() {
    let route_handoffs = RealtimeHandoffAdmission::new();
    route_handoffs.begin_shutdown(true).await;

    assert!(
        route_handoffs
            .acquire_route_permit(RealtimeDelegationSource::Handoff)
            .await
            .is_none()
    );
    let tail_permit = route_handoffs
        .acquire_route_permit(RealtimeDelegationSource::TranscriptTailFlush)
        .await
        .expect("configured shutdown must admit the final transcript tail");
    drop(tail_permit);
    assert!(
        route_handoffs
            .acquire_route_permit(RealtimeDelegationSource::TranscriptTailFlush)
            .await
            .is_none()
    );

    route_handoffs.retire().await;
    assert!(
        route_handoffs
            .acquire_route_permit(RealtimeDelegationSource::TranscriptTailFlush)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn replacement_closes_old_handoff_admission_before_waiting_for_in_flight_route() {
    let route_handoffs = Arc::new(RealtimeHandoffAdmission::new());
    let in_flight_route = route_handoffs
        .acquire_route_permit(RealtimeDelegationSource::Handoff)
        .await
        .expect("the old conversation starts with open handoff admission");

    let shutdown_gate = Arc::clone(&route_handoffs);
    let shutdown = tokio::spawn(async move {
        shutdown_gate.begin_shutdown(false).await;
    });
    while !route_handoffs
        .shutting_down
        .load(std::sync::atomic::Ordering::Acquire)
    {
        tokio::task::yield_now().await;
    }

    assert!(
        route_handoffs
            .retired
            .load(std::sync::atomic::Ordering::Acquire)
    );
    let late_old_route_gate = Arc::clone(&route_handoffs);
    let late_old_route = tokio::spawn(async move {
        late_old_route_gate
            .acquire_route_permit(RealtimeDelegationSource::Handoff)
            .await
            .is_none()
    });

    drop(in_flight_route);
    shutdown.await.expect("shutdown task should complete");
    assert!(
        late_old_route
            .await
            .expect("late route task should complete")
    );
}

#[tokio::test]
async fn shutdown_without_transcript_tail_enabled_rejects_all_handoffs() {
    let route_handoffs = RealtimeHandoffAdmission::new();
    route_handoffs.begin_shutdown(false).await;

    assert!(
        route_handoffs
            .acquire_route_permit(RealtimeDelegationSource::Handoff)
            .await
            .is_none()
    );
    assert!(
        route_handoffs
            .acquire_route_permit(RealtimeDelegationSource::TranscriptTailFlush)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn misalignment_retirement_revokes_a_pending_shutdown_tail() {
    let route_handoffs = RealtimeHandoffAdmission::new();
    route_handoffs.begin_shutdown(true).await;
    route_handoffs.retire().await;

    assert!(
        route_handoffs
            .acquire_route_permit(RealtimeDelegationSource::TranscriptTailFlush)
            .await
            .is_none()
    );
}

#[test]
fn prefers_handoff_input_transcript_over_active_transcript() {
    let handoff = RealtimeHandoffRequested {
        handoff_id: "handoff_1".to_string(),
        item_id: "item_1".to_string(),
        input_transcript: "ignored".to_string(),
        active_transcript: vec![
            RealtimeTranscriptEntry {
                role: "user".to_string(),
                text: "hello".to_string(),
            },
            RealtimeTranscriptEntry {
                role: "assistant".to_string(),
                text: "hi there".to_string(),
            },
        ],
        routing: None,
    };
    assert_eq!(
        realtime_text_from_handoff_request(&handoff),
        Some("ignored".to_string())
    );
}

#[test]
fn extracts_text_from_handoff_request_active_transcript_if_input_missing() {
    let handoff = RealtimeHandoffRequested {
        handoff_id: "handoff_1".to_string(),
        item_id: "item_1".to_string(),
        input_transcript: String::new(),
        active_transcript: vec![RealtimeTranscriptEntry {
            role: "user".to_string(),
            text: "hello".to_string(),
        }],
        routing: None,
    };
    assert_eq!(
        realtime_text_from_handoff_request(&handoff),
        Some("user: hello".to_string())
    );
}

#[test]
fn does_not_use_active_transcript_as_handoff_routing_input() {
    let handoff = RealtimeHandoffRequested {
        handoff_id: "handoff_1".to_string(),
        item_id: "item_1".to_string(),
        input_transcript: String::new(),
        active_transcript: vec![RealtimeTranscriptEntry {
            role: "user".to_string(),
            text: "What time is it?".to_string(),
        }],
        routing: None,
    };
    let (_, routing_input) = realtime_delegation_with_routing_input(&handoff)
        .expect("active transcript should still produce the delegated text");
    assert_eq!(routing_input, None);
}

#[test]
fn wraps_handoff_with_transcript_delta() {
    let handoff = RealtimeHandoffRequested {
        handoff_id: "handoff_1".to_string(),
        item_id: "item_1".to_string(),
        input_transcript: "delegate this".to_string(),
        active_transcript: vec![
            RealtimeTranscriptEntry {
                role: "user".to_string(),
                text: "hello".to_string(),
            },
            RealtimeTranscriptEntry {
                role: "assistant".to_string(),
                text: "hi there".to_string(),
            },
        ],
        routing: None,
    };
    assert_eq!(
        realtime_delegation_from_handoff(&handoff),
        Some(
            "<realtime_delegation>\n  <input>delegate this</input>\n  <transcript_delta>user: hello\nassistant: hi there</transcript_delta>\n</realtime_delegation>"
                .to_string()
        )
    );
}

#[test]
fn extracts_text_from_handoff_request_input_transcript_if_messages_missing() {
    let handoff = RealtimeHandoffRequested {
        handoff_id: "handoff_1".to_string(),
        item_id: "item_1".to_string(),
        input_transcript: "ignored".to_string(),
        active_transcript: vec![],
        routing: None,
    };
    assert_eq!(
        realtime_text_from_handoff_request(&handoff),
        Some("ignored".to_string())
    );
}

#[test]
fn ignores_empty_handoff_request_input_transcript() {
    let handoff = RealtimeHandoffRequested {
        handoff_id: "handoff_1".to_string(),
        item_id: "item_1".to_string(),
        input_transcript: String::new(),
        active_transcript: vec![],
        routing: None,
    };
    assert_eq!(realtime_text_from_handoff_request(&handoff), None);
}

#[test]
fn wraps_realtime_delegation_input() {
    assert_eq!(
        wrap_realtime_delegation_input(
            "hello",
            /*transcript_delta*/ None,
            RealtimeDelegationSource::Handoff,
        ),
        "<realtime_delegation>\n  <input>hello</input>\n</realtime_delegation>"
    );
}

#[test]
fn wraps_realtime_delegation_input_with_xml_escaping() {
    assert_eq!(
        wrap_realtime_delegation_input(
            "use a < b && c > d",
            Some("saw <that>"),
            RealtimeDelegationSource::Handoff,
        ),
        "<realtime_delegation>\n  <input>use a &lt; b &amp;&amp; c &gt; d</input>\n  <transcript_delta>saw &lt;that&gt;</transcript_delta>\n</realtime_delegation>"
    );
}

#[test]
fn wraps_realtime_delegation_input_with_xml_escaping_without_transcript() {
    assert_eq!(
        wrap_realtime_delegation_input(
            "use a < b && c > d",
            /*transcript_delta*/ None,
            RealtimeDelegationSource::Handoff,
        ),
        "<realtime_delegation>\n  <input>use a &lt; b &amp;&amp; c &gt; d</input>\n</realtime_delegation>"
    );
}

#[test]
fn bounds_oversized_realtime_delegation_and_preserves_transcript_tail() {
    let input = "delegate & verify <everything> ".repeat(2_000);
    let transcript_tail = "assistant: newest transcript tail";
    let transcript_delta = format!(
        "{}\n{transcript_tail}",
        "user: old & verbose <transcript>".repeat(4_000)
    );

    let rendered = wrap_realtime_delegation_input(
        &input,
        Some(&transcript_delta),
        RealtimeDelegationSource::Handoff,
    );

    assert!(
        approx_token_count(&rendered) <= REALTIME_DELEGATION_MAX_ESTIMATED_TOKENS,
        "expected bounded realtime delegation, got {} estimated tokens",
        approx_token_count(&rendered)
    );
    assert!(rendered.contains("input truncated"));
    assert!(rendered.contains("earlier transcript truncated"));
    assert!(rendered.contains(transcript_tail));
}

#[test]
fn bounds_realtime_delegation_fields_and_keeps_latest_transcript() {
    let input = format!("start{}input-end", "x".repeat(8 * 1024));
    let transcript = format!("transcript-start{}latest", "y".repeat(8 * 1024));
    let rendered = wrap_realtime_delegation_input(
        &input,
        Some(&transcript),
        RealtimeDelegationSource::Handoff,
    );

    assert!(rendered.len() < 9 * 1024);
    assert!(rendered.contains("<input>start"));
    assert!(!rendered.contains("input-end"));
    assert!(!rendered.contains("transcript-start"));
    assert!(rendered.contains("latest</transcript_delta>"));
}

#[test]
fn classifies_outbound_api_failures_as_transport_loss() {
    for pending_outbound in [
        RealtimePendingOutbound::Text(ConversationTextParams {
            text: "retry me".to_string(),
            role: ConversationTextRole::User,
        }),
        RealtimePendingOutbound::Handoff(RealtimeOutbound::StandaloneHandoff {
            text: "retry this handoff".to_string(),
            phase: Some(MessagePhase::FinalAnswer),
        }),
    ] {
        let exit = classify_realtime_input_error_with_pending(
            ApiError::Stream("failed to send realtime request".to_string()).into(),
            Some(Box::new(pending_outbound.clone())),
        );
        let RealtimeInputTaskExit::TransportLost {
            err: ApiError::Stream(_),
            pending_outbound: Some(actual_pending_outbound),
        } = exit
        else {
            panic!("outbound API failure should preserve pending output for reconnect");
        };
        match (actual_pending_outbound.as_ref(), &pending_outbound) {
            (RealtimePendingOutbound::Text(actual), RealtimePendingOutbound::Text(expected)) => {
                assert_eq!(actual, expected)
            }
            (
                RealtimePendingOutbound::Handoff(RealtimeOutbound::StandaloneHandoff {
                    text: actual_text,
                    phase: actual_phase,
                }),
                RealtimePendingOutbound::Handoff(RealtimeOutbound::StandaloneHandoff {
                    text: expected_text,
                    phase: expected_phase,
                }),
            ) => assert_eq!((actual_text, actual_phase), (expected_text, expected_phase)),
            _ => panic!("reconnect preserved a different pending outbound variant"),
        }
    }

    assert!(matches!(
        classify_realtime_input_error(anyhow::anyhow!("input channel closed")),
        RealtimeInputTaskExit::Terminal
    ));
}

#[tokio::test]
async fn clears_active_handoff_explicitly() {
    let (tx, _rx) = bounded(1);
    let state = RealtimeHandoffState {
        output_tx: tx,
        output_send_gate: Arc::new(Semaphore::new(1)),
        last_output: Arc::new(Mutex::new(None)),
        stream: Arc::new(Mutex::new(Default::default())),
        suppress_preambles: false,
        suppress_non_final_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        client_managed_handoffs: false,
        codex_responses_as_items: false,
        codex_response_item_prefix: None,
        backend_reasoning_status: false,
        codex_response_handoff_mode: CodexResponseHandoffMode::Thinking,
        codex_response_handoff_channel_prefixes: Arc::new(BTreeMap::new()),
        session_kind: RealtimeSessionKind::V2,
        event_parser: RealtimeEventParser::V1,
    };

    state.stream.lock().await.active_handoff = Some("handoff_1".to_string());
    assert_eq!(
        state.stream.lock().await.active_handoff.clone(),
        Some("handoff_1".to_string())
    );

    state.stream.lock().await.active_handoff = None;
    assert_eq!(state.stream.lock().await.active_handoff.clone(), None);
}

#[tokio::test]
async fn handoff_complete_preserves_pending_streamed_final_output() {
    let (output_tx, output_rx) = bounded(8);
    let handoff = RealtimeHandoffState {
        output_tx,
        output_send_gate: Arc::new(Semaphore::new(1)),
        last_output: Arc::new(Mutex::new(None)),
        stream: Arc::new(Mutex::new(Default::default())),
        suppress_preambles: false,
        suppress_non_final_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        client_managed_handoffs: false,
        codex_responses_as_items: false,
        codex_response_item_prefix: None,
        backend_reasoning_status: false,
        codex_response_handoff_mode: CodexResponseHandoffMode::Thinking,
        codex_response_handoff_channel_prefixes: Arc::new(BTreeMap::new()),
        session_kind: RealtimeSessionKind::V1,
        event_parser: RealtimeEventParser::FramelessBidi,
    };
    let mut streamed_item = RealtimeStreamedItem {
        handoff_id: "handoff_1".to_string(),
        phase: Some(MessagePhase::FinalAnswer),
        bem_channel_parser: None,
        prefix_final_message: false,
        sent_bytes: 0,
        buffered_text: String::new(),
        tail_text: String::new(),
        truncated: false,
        last_flush_at: Instant::now(),
        flush_scheduled: false,
    };
    streamed_item.push_text("final answer");
    let mut earlier_item = RealtimeStreamedItem {
        handoff_id: "handoff_1".to_string(),
        phase: Some(MessagePhase::FinalAnswer),
        bem_channel_parser: None,
        prefix_final_message: false,
        sent_bytes: 0,
        buffered_text: String::new(),
        tail_text: String::new(),
        truncated: false,
        last_flush_at: Instant::now(),
        flush_scheduled: false,
    };
    earlier_item.push_text("first answer");
    {
        let mut stream = handoff.stream.lock().await;
        stream.active_handoff = Some("handoff_1".to_string());
        stream.items.insert("item_1".to_string(), streamed_item);
        stream.items.insert("item_2".to_string(), earlier_item);
        stream
            .item_order
            .extend(["item_2".to_string(), "item_1".to_string()]);
    }

    let manager = RealtimeConversationManager {
        state: Mutex::new(RealtimeConversationManagerState {
            conversation: Some(ConversationState {
                audio_tx: bounded(1).0,
                text_tx: bounded(1).0,
                session_kind: RealtimeSessionKind::V1,
                handoff,
                input_task: tokio::spawn(async {}),
                fanout_task: None,
                realtime_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                route_handoffs: Arc::new(RealtimeHandoffAdmission::new()),
                flush_transcript_tail_on_session_end: false,
                stop_token: CancellationToken::new(),
            }),
            mode_instructions: None,
        }),
        realtime_submission_sender: Mutex::new(None),
        starting_stop_token: std::sync::Mutex::new(None),
    };
    let output_task = tokio::spawn(async move {
        let mut append_texts = Vec::new();
        for _ in 0..2 {
            match output_rx
                .recv()
                .await
                .expect("handoff output should be sent")
            {
                RealtimeOutbound::HandoffAppend { text, .. } => append_texts.push(text),
                output => panic!("unexpected realtime output: {output:?}"),
            }
        }
        append_texts
    });

    manager
        .handoff_complete()
        .await
        .expect("handoff completion should succeed");

    assert_eq!(
        output_task.await.expect("output task should finish"),
        ["first answer".to_string(), "final answer".to_string()]
    );
}

#[tokio::test]
async fn disabled_preambles_suppress_commentary_and_defer_unphased_output_until_completion() {
    let (output_tx, output_rx) = bounded(8);
    let handoff = RealtimeHandoffState {
        output_tx,
        output_send_gate: Arc::new(Semaphore::new(1)),
        last_output: Arc::new(Mutex::new(None)),
        stream: Arc::new(Mutex::new(Default::default())),
        suppress_preambles: true,
        suppress_non_final_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        client_managed_handoffs: false,
        codex_responses_as_items: false,
        codex_response_item_prefix: None,
        backend_reasoning_status: false,
        codex_response_handoff_mode: CodexResponseHandoffMode::Thinking,
        codex_response_handoff_channel_prefixes: Arc::new(BTreeMap::new()),
        session_kind: RealtimeSessionKind::V1,
        event_parser: RealtimeEventParser::FramelessBidi,
    };
    let manager = RealtimeConversationManager {
        state: Mutex::new(RealtimeConversationManagerState {
            conversation: Some(ConversationState {
                audio_tx: bounded(1).0,
                text_tx: bounded(1).0,
                session_kind: RealtimeSessionKind::V1,
                handoff,
                input_task: tokio::spawn(async {}),
                fanout_task: None,
                realtime_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                route_handoffs: Arc::new(RealtimeHandoffAdmission::new()),
                flush_transcript_tail_on_session_end: false,
                stop_token: CancellationToken::new(),
            }),
            mode_instructions: None,
        }),
        realtime_submission_sender: Mutex::new(None),
        starting_stop_token: std::sync::Mutex::new(None),
    };
    let handoff = manager
        .state
        .lock()
        .await
        .conversation
        .as_ref()
        .expect("realtime state should be present")
        .handoff
        .clone();
    handoff.stream.lock().await.active_handoff = Some("handoff_1".to_string());

    manager
        .handoff_out(
            "let me take a look".to_string(),
            Some(MessagePhase::Commentary),
        )
        .await
        .expect("commentary handoff output should be accepted");
    assert!(output_rx.try_recv().is_err());

    manager
        .handoff_out("direct answer".to_string(), None)
        .await
        .expect("phase-less final handoff output should be accepted");
    assert!(output_rx.try_recv().is_err());

    manager
        .register_handoff_stream_item(
            "commentary-item".to_string(),
            Some(MessagePhase::Commentary),
            "one sec".to_string(),
        )
        .await;
    assert!(!manager.finish_handoff_stream_item("commentary-item").await);

    manager
        .register_handoff_stream_item(
            "final-item".to_string(),
            None,
            "streamed answer".to_string(),
        )
        .await;
    assert!(manager.finish_handoff_stream_item("final-item").await);
    assert!(output_rx.try_recv().is_err());

    let output_task = tokio::spawn(async move {
        match output_rx
            .recv()
            .await
            .expect("handoff output should be sent")
        {
            RealtimeOutbound::HandoffAppend { text, .. } => vec![text],
            output => panic!("unexpected realtime output: {output:?}"),
        }
    });

    manager
        .handoff_complete()
        .await
        .expect("handoff completion should succeed");

    assert_eq!(
        output_task.await.expect("output task should finish"),
        ["streamed answer".to_string()]
    );
}

#[tokio::test]
async fn disabled_preambles_drop_phase_less_bridge_before_preserving_final_output() {
    let (output_tx, output_rx) = bounded(8);
    let handoff = RealtimeHandoffState {
        output_tx,
        output_send_gate: Arc::new(Semaphore::new(1)),
        last_output: Arc::new(Mutex::new(None)),
        stream: Arc::new(Mutex::new(Default::default())),
        suppress_preambles: true,
        suppress_non_final_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        client_managed_handoffs: false,
        codex_responses_as_items: false,
        codex_response_item_prefix: None,
        backend_reasoning_status: false,
        codex_response_handoff_mode: CodexResponseHandoffMode::Thinking,
        codex_response_handoff_channel_prefixes: Arc::new(BTreeMap::new()),
        session_kind: RealtimeSessionKind::V1,
        event_parser: RealtimeEventParser::FramelessBidi,
    };
    let manager = RealtimeConversationManager {
        state: Mutex::new(RealtimeConversationManagerState {
            conversation: Some(ConversationState {
                audio_tx: bounded(1).0,
                text_tx: bounded(1).0,
                session_kind: RealtimeSessionKind::V1,
                handoff,
                input_task: tokio::spawn(async {}),
                fanout_task: None,
                realtime_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                route_handoffs: Arc::new(RealtimeHandoffAdmission::new()),
                flush_transcript_tail_on_session_end: false,
                stop_token: CancellationToken::new(),
            }),
            mode_instructions: None,
        }),
        realtime_submission_sender: Mutex::new(None),
        starting_stop_token: std::sync::Mutex::new(None),
    };
    let handoff = manager
        .state
        .lock()
        .await
        .conversation
        .as_ref()
        .expect("realtime state should be present")
        .handoff
        .clone();
    handoff.stream.lock().await.active_handoff = Some("handoff_1".to_string());

    manager
        .register_handoff_stream_item(
            "bridge-1".to_string(),
            None,
            "Just a sec, checking that.".to_string(),
        )
        .await;
    assert!(manager.finish_handoff_stream_item("bridge-1").await);
    assert!(
        output_rx.try_recv().is_err(),
        "phase-less bridge output must stay local until the turn boundary identifies it"
    );
    // A non-agent item is the lifecycle evidence that the phase-less item was bridge commentary,
    // not the final answer. This is source/event correlation, never text matching.
    manager.discard_pending_unphased_handoff_output().await;

    let output_task = tokio::spawn(async move {
        match output_rx
            .recv()
            .await
            .expect("handoff output should be sent")
        {
            RealtimeOutbound::HandoffAppend { text, .. } => vec![text],
            output => panic!("unexpected realtime output: {output:?}"),
        }
    });

    manager
        .register_handoff_stream_item(
            "final-1".to_string(),
            None,
            "We're on agent/realtime-preamble-fix.".to_string(),
        )
        .await;
    assert!(manager.finish_handoff_stream_item("final-1").await);
    manager
        .handoff_complete()
        .await
        .expect("handoff completion should succeed");

    assert_eq!(
        output_task.await.expect("output task should finish"),
        ["We're on agent/realtime-preamble-fix.".to_string()]
    );
}

#[test]
fn internal_continuation_suppression_keeps_final_realtime_output() {
    let (tx, _rx) = bounded(1);
    let state = RealtimeHandoffState {
        output_tx: tx,
        output_send_gate: Arc::new(Semaphore::new(1)),
        last_output: Arc::new(Mutex::new(None)),
        stream: Arc::new(Mutex::new(Default::default())),
        suppress_preambles: false,
        suppress_non_final_output: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        client_managed_handoffs: false,
        codex_responses_as_items: false,
        codex_response_item_prefix: None,
        backend_reasoning_status: false,
        codex_response_handoff_mode: CodexResponseHandoffMode::Thinking,
        codex_response_handoff_channel_prefixes: Arc::new(BTreeMap::new()),
        session_kind: RealtimeSessionKind::V1,
        event_parser: RealtimeEventParser::V1,
    };

    assert!(state.suppresses_output(Some(&MessagePhase::Commentary)));
    assert!(!state.suppresses_output(Some(&MessagePhase::FinalAnswer)));
    assert!(!state.suppresses_output(None));
}

#[test]
fn streamed_handoff_preserves_a_bounded_final_tail() {
    let mut item = RealtimeStreamedItem {
        handoff_id: "handoff_1".to_string(),
        phase: Some(MessagePhase::FinalAnswer),
        bem_channel_parser: None,
        prefix_final_message: true,
        sent_bytes: 0,
        buffered_text: String::new(),
        tail_text: String::new(),
        truncated: false,
        last_flush_at: Instant::now(),
        flush_scheduled: false,
    };
    item.push_text(&format!("HEAD{}TAIL", "x".repeat(/*n*/ 5_000)));

    let first = item
        .drain_stream_chunk()
        .expect("oversized output should retain a streamable head");
    let final_chunk = item
        .drain_final_chunk()
        .expect("oversized output should retain a final tail");
    let output = format!("{first}{final_chunk}");

    assert!(output.len() <= 4_000);
    assert!(output.starts_with(&format!("{AGENT_FINAL_MESSAGE_PREFIX}HEAD")));
    assert!(output.contains(HANDOFF_STREAM_TRUNCATION_MARKER));
    assert!(output.ends_with("TAIL"));
}

#[test]
fn streamed_v3_handoff_omits_the_final_message_prefix() {
    let mut item = RealtimeStreamedItem {
        handoff_id: "handoff_1".to_string(),
        phase: Some(MessagePhase::FinalAnswer),
        bem_channel_parser: None,
        prefix_final_message: false,
        sent_bytes: 0,
        buffered_text: String::new(),
        tail_text: String::new(),
        truncated: false,
        last_flush_at: Instant::now(),
        flush_scheduled: false,
    };
    item.push_text("done");

    assert_eq!(item.drain_final_chunk(), Some("done".to_string()));
}

#[test]
fn uses_quicksilver_alpha_header_for_realtime_v1() {
    let headers = realtime_request_headers(
        Some("session_1"),
        Some("sk-test"),
        RealtimeEventParser::V1,
        "codex_work_desktop",
    )
    .expect("headers")
    .expect("headers");

    assert_eq!(
        headers
            .get("openai-alpha")
            .and_then(|value| value.to_str().ok()),
        Some("quicksilver=v1")
    );
}

#[test]
fn omits_quicksilver_alpha_header_for_realtime_v2() {
    let headers = realtime_request_headers(
        Some("session_1"),
        Some("sk-test"),
        RealtimeEventParser::RealtimeV2,
        "codex_work_desktop",
    )
    .expect("headers")
    .expect("headers");

    assert!(headers.get("openai-alpha").is_none());
}

#[test]
fn uses_frameless_alpha_header_for_realtime_v3() {
    let headers = realtime_request_headers(
        Some("session_1"),
        Some("sk-test"),
        RealtimeEventParser::FramelessBidi,
        "codex_work_desktop",
    )
    .expect("headers")
    .expect("headers");

    assert_eq!(
        headers
            .get("openai-alpha")
            .and_then(|value| value.to_str().ok()),
        Some("quicksilver=v2")
    );
}

#[test]
fn realtime_headers_include_only_non_default_originator() {
    let default_originator = codex_login::default_client::originator();
    for (originator, expected_header) in [
        ("codex_work_desktop", Some("codex_work_desktop")),
        (default_originator.value.as_str(), None),
    ] {
        let headers = realtime_request_headers(
            Some("session_1"),
            Some("sk-test"),
            RealtimeEventParser::RealtimeV2,
            originator,
        )
        .expect("headers")
        .expect("headers");

        assert_eq!(
            headers
                .get("originator")
                .and_then(|value| value.to_str().ok()),
            expected_header
        );
    }
}
