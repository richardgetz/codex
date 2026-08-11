use super::AGENT_FINAL_MESSAGE_PREFIX;
use super::ConversationState;
use super::HANDOFF_STREAM_TRUNCATION_MARKER;
use super::REALTIME_HANDOFF_DEDUPE_CAPACITY;
use super::RealtimeConversationManager;
use super::RealtimeHandoffDeduper;
use super::RealtimeHandoffState;
use super::RealtimeOutbound;
use super::RealtimeSessionKind;
use super::RealtimeStreamedItem;
use super::realtime_delegation_from_handoff;
use super::realtime_delegation_with_routing_input;
use super::realtime_request_headers;
use super::realtime_text_from_handoff_request;
use super::wrap_realtime_delegation_input;
use crate::context::REALTIME_DELEGATION_MAX_ESTIMATED_TOKENS;
use crate::context::RealtimeDelegationSource;
use async_channel::bounded;
use codex_api::RealtimeEventParser;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::CodexResponseHandoffMode;
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

#[tokio::test]
async fn clears_active_handoff_explicitly() {
    let (tx, _rx) = bounded(1);
    let state = RealtimeHandoffState {
        output_tx: tx,
        output_send_gate: Arc::new(Semaphore::new(1)),
        last_output: Arc::new(Mutex::new(None)),
        stream: Arc::new(Mutex::new(Default::default())),
        transport_handoff_deduper: Arc::new(Mutex::new(RealtimeHandoffDeduper::default())),
        suppress_preambles: false,
        suppress_non_final_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        client_managed_handoffs: false,
        codex_responses_as_items: false,
        codex_response_item_prefix: None,
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
        transport_handoff_deduper: Arc::new(Mutex::new(RealtimeHandoffDeduper::default())),
        suppress_preambles: false,
        suppress_non_final_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        client_managed_handoffs: false,
        codex_responses_as_items: false,
        codex_response_item_prefix: None,
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
        state: Mutex::new(Some(ConversationState {
            audio_tx: bounded(1).0,
            text_tx: bounded(1).0,
            session_kind: RealtimeSessionKind::V1,
            handoff,
            input_task: tokio::spawn(async {}),
            fanout_task: None,
            realtime_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            stop_token: CancellationToken::new(),
        })),
    };
    let output_task = tokio::spawn(async move {
        let mut append_texts = Vec::new();
        while let Ok(output) = output_rx.recv().await {
            match output {
                RealtimeOutbound::HandoffAppend { text, .. } => append_texts.push(text),
                RealtimeOutbound::Flush { completion } => {
                    let _ = completion.send(());
                    break;
                }
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
async fn disabled_preambles_suppress_commentary_and_preserve_phase_less_final_output() {
    let (output_tx, output_rx) = bounded(8);
    let handoff = RealtimeHandoffState {
        output_tx,
        output_send_gate: Arc::new(Semaphore::new(1)),
        last_output: Arc::new(Mutex::new(None)),
        stream: Arc::new(Mutex::new(Default::default())),
        transport_handoff_deduper: Arc::new(Mutex::new(RealtimeHandoffDeduper::default())),
        suppress_preambles: true,
        suppress_non_final_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        client_managed_handoffs: false,
        codex_responses_as_items: false,
        codex_response_item_prefix: None,
        codex_response_handoff_mode: CodexResponseHandoffMode::Thinking,
        codex_response_handoff_channel_prefixes: Arc::new(BTreeMap::new()),
        session_kind: RealtimeSessionKind::V1,
        event_parser: RealtimeEventParser::FramelessBidi,
    };
    let manager = RealtimeConversationManager {
        state: Mutex::new(Some(ConversationState {
            audio_tx: bounded(1).0,
            text_tx: bounded(1).0,
            session_kind: RealtimeSessionKind::V1,
            handoff,
            input_task: tokio::spawn(async {}),
            fanout_task: None,
            realtime_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            stop_token: CancellationToken::new(),
        })),
    };
    let handoff = manager
        .state
        .lock()
        .await
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
    assert!(matches!(
        output_rx.recv().await.expect("direct answer should be forwarded"),
        RealtimeOutbound::HandoffAppend { text, phase: None, .. }
            if text == "direct answer"
    ));

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
    assert!(matches!(
        output_rx.recv().await.expect("streamed answer should be forwarded"),
        RealtimeOutbound::HandoffAppend { text, phase: None, .. }
            if text == "streamed answer"
    ));
    assert!(output_rx.try_recv().is_err());
}

#[test]
fn internal_continuation_suppression_keeps_final_realtime_output() {
    let (tx, _rx) = bounded(1);
    let state = RealtimeHandoffState {
        output_tx: tx,
        output_send_gate: Arc::new(Semaphore::new(1)),
        last_output: Arc::new(Mutex::new(None)),
        stream: Arc::new(Mutex::new(Default::default())),
        transport_handoff_deduper: Arc::new(Mutex::new(RealtimeHandoffDeduper::default())),
        suppress_preambles: false,
        suppress_non_final_output: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        client_managed_handoffs: false,
        codex_responses_as_items: false,
        codex_response_item_prefix: None,
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
