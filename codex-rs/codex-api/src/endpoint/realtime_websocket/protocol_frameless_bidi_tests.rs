use super::parse_frameless_bidi_event;
use crate::endpoint::realtime_websocket::protocol_v1::parse_realtime_event_v1;
use codex_protocol::protocol::RealtimeEvent;
use codex_protocol::protocol::RealtimeHandoffRequested;

#[test]
fn legacy_and_frameless_delegations_decode_to_the_same_handoff() {
    let expected = Some(RealtimeEvent::HandoffRequested(RealtimeHandoffRequested {
        handoff_id: "handoff-123".to_string(),
        item_id: "handoff-123".to_string(),
        input_transcript: "check the weather".to_string(),
        active_transcript: Vec::new(),
        routing: None,
    }));
    let legacy = r#"{
        "type": "conversation.handoff.requested",
        "handoff_id": "handoff-123",
        "item_id": "handoff-123",
        "input_transcript": "check the weather"
    }"#;
    let frameless = r#"{
        "type": "delegation.created",
        "offset_ms": 1000,
        "item": {
            "id": "handoff-123",
            "type": "delegation",
            "target": "client",
            "content": [{"type": "input_text", "text": "check the weather"}]
        }
    }"#;

    assert_eq!(parse_realtime_event_v1(legacy), expected);
    assert_eq!(parse_frameless_bidi_event(frameless), expected);
}

#[test]
fn frameless_transcript_and_audio_events_reuse_existing_internal_events() {
    let input = r#"{
        "type": "input_transcript.added",
        "item": {"id": "input-1", "type": "input_transcript", "text": "hello"}
    }"#;
    let done = r#"{
        "type": "turn.done",
        "turn": {"id": "turn-1", "role": "user", "transcript": "hello"}
    }"#;
    let audio = r#"{
        "type": "output_audio.delta",
        "audio": "AAE=",
        "item_id": "output-1",
        "start_ms": 0,
        "end_ms": 100
    }"#;

    assert!(matches!(
        parse_frameless_bidi_event(input),
        Some(RealtimeEvent::InputTranscriptDelta(_))
    ));
    assert!(matches!(
        parse_frameless_bidi_event(done),
        Some(RealtimeEvent::InputTranscriptDone(_))
    ));
    match parse_frameless_bidi_event(audio) {
        Some(RealtimeEvent::AudioOut(frame)) => {
            assert_eq!(frame.item_id.as_deref(), Some("output-1"));
        }
        other => panic!("expected audio output event, got {other:?}"),
    }
}

#[test]
fn frameless_response_events_preserve_response_ids_for_diagnostics() {
    let created = r#"{
        "type": "response.created",
        "response": {"id": "response-1"}
    }"#;
    let done = r#"{
        "type": "response.done",
        "response_id": "response-1"
    }"#;

    assert_eq!(
        parse_frameless_bidi_event(created),
        Some(RealtimeEvent::ResponseCreated(
            codex_protocol::protocol::RealtimeResponseCreated {
                response_id: Some("response-1".to_string()),
            }
        ))
    );
    assert_eq!(
        parse_frameless_bidi_event(done),
        Some(RealtimeEvent::ResponseDone(
            codex_protocol::protocol::RealtimeResponseDone {
                response_id: Some("response-1".to_string()),
            }
        ))
    );
}
