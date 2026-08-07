use super::*;

use crate::history_cell::HistoryCell;
use codex_app_server_protocol::ThreadRealtimeTranscriptDeltaNotification;
use codex_app_server_protocol::ThreadRealtimeTranscriptDoneNotification;

fn render_lines(cell: &dyn HistoryCell) -> String {
    lines_to_single_string(&cell.display_lines(/*width*/ 80))
}

#[tokio::test]
async fn realtime_transcript_notifications_render_live_and_finalize() {
    let (mut chat, _app_event_tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);

    while rx.try_recv().is_ok() {}

    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDelta(
            ThreadRealtimeTranscriptDeltaNotification {
                thread_id: thread_id.to_string(),
                role: "user".to_string(),
                delta: "hello from the microphone".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    let lines = chat
        .active_cell_transcript_lines(/*width*/ 80)
        .expect("user transcript should be visible while streaming");
    assert_eq!(
        lines_to_single_string(&lines),
        "› hello from the microphone\n"
    );
    assert_matches!(rx.try_recv(), Err(TryRecvError::Empty));

    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDone(
            ThreadRealtimeTranscriptDoneNotification {
                thread_id: thread_id.to_string(),
                role: "user".to_string(),
                text: "hello from the microphone".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            assert_eq!(render_lines(cell.as_ref()), "› hello from the microphone\n");
        }
        other => panic!("expected finalized user transcript cell, got {other:?}"),
    }
    assert!(chat.active_cell_transcript_lines(/*width*/ 80).is_none());

    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDelta(
            ThreadRealtimeTranscriptDeltaNotification {
                thread_id: thread_id.to_string(),
                role: "assistant".to_string(),
                delta: "hello back from live voice".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    let lines = chat
        .active_cell_transcript_lines(/*width*/ 80)
        .expect("assistant transcript should be visible while streaming");
    assert_eq!(
        lines_to_single_string(&lines),
        "• hello back from live voice\n"
    );

    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDone(
            ThreadRealtimeTranscriptDoneNotification {
                thread_id: thread_id.to_string(),
                role: "assistant".to_string(),
                text: "hello back from live voice".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            assert_eq!(
                render_lines(cell.as_ref()),
                "• hello back from live voice\n"
            );
        }
        other => panic!("expected finalized assistant transcript cell, got {other:?}"),
    }
}

#[tokio::test]
async fn realtime_handoff_turn_hides_normal_codex_response_but_keeps_live_transcript() {
    let (mut chat, _app_event_tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.on_task_started();
    while rx.try_recv().is_ok() {}

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
            item: AppServerThreadItem::UserMessage {
                id: "voice-user-1".to_string(),
                client_id: None,
                content: vec![AppServerUserInput::Text {
                    text: "<realtime_delegation>\n  <input>hello</input>\n</realtime_delegation>"
                        .to_string(),
                    text_elements: Vec::new(),
                }],
            },
        }),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::AgentMessageDelta(
            codex_app_server_protocol::AgentMessageDeltaNotification {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "agent-1".to_string(),
                delta: "normal Codex response".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    assert!(chat.active_cell_transcript_lines(/*width*/ 80).is_none());

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
            item: AppServerThreadItem::AgentMessage {
                id: "agent-1".to_string(),
                text: "normal Codex response".to_string(),
                phase: Some(MessagePhase::FinalAnswer),
                memory_citation: None,
            },
        }),
        /*replay_kind*/ None,
    );
    assert!(rx.try_recv().is_err());

    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDelta(
            ThreadRealtimeTranscriptDeltaNotification {
                thread_id: thread_id.to_string(),
                role: "assistant".to_string(),
                delta: "spoken response".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    assert_eq!(
        lines_to_single_string(
            &chat
                .active_cell_transcript_lines(/*width*/ 80)
                .expect("realtime transcript should remain visible")
        ),
        "• spoken response\n"
    );
}
