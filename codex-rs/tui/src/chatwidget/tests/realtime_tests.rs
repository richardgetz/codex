use super::*;

use crate::history_cell::HistoryCell;
use codex_app_server_protocol::ThreadRealtimeItemAddedNotification;
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
    assert!(rx.try_recv().is_err());
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
async fn disabled_preambles_allow_realtime_answer_after_handoff_turn_completion() {
    let (mut chat, _app_event_tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
    chat.config.realtime.enable_preambles = false;
    let thread_id = ThreadId::new();

    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeItemAdded(ThreadRealtimeItemAddedNotification {
            thread_id: thread_id.to_string(),
            item: serde_json::json!({
                "type": "handoff_request",
                "handoff_id": "handoff-unmarked-answer"
            }),
        }),
        /*replay_kind*/ None,
    );
    handle_turn_completed(&mut chat, "turn-1", /*duration_ms*/ None);
    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDone(
            ThreadRealtimeTranscriptDoneNotification {
                thread_id: thread_id.to_string(),
                role: "assistant".to_string(),
                text: "The direct GPT-Live answer remains visible.".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );

    match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            let rendered = render_lines(cell.as_ref());
            insta::assert_snapshot!("realtime_unmarked_answer_after_handoff", rendered);
            assert!(rendered.contains("direct GPT-Live answer remains visible"));
        }
        other => panic!("expected unmarked realtime answer cell, got {other:?}"),
    }
}

#[tokio::test]
async fn disabled_preambles_do_not_mute_direct_realtime_conversation() {
    let (mut chat, _app_event_tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
    chat.config.realtime.enable_preambles = false;
    let thread_id = ThreadId::new();

    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDone(
            ThreadRealtimeTranscriptDoneNotification {
                thread_id: thread_id.to_string(),
                role: "assistant".to_string(),
                text: "This direct GPT-Live answer remains visible.".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );

    match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            assert!(render_lines(cell.as_ref()).contains("direct GPT-Live answer remains visible"));
        }
        other => panic!("expected direct realtime answer cell, got {other:?}"),
    }
}

#[tokio::test]
async fn disabled_preambles_keep_direct_realtime_output_after_handoff() {
    let (mut chat, _app_event_tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
    chat.config.realtime.enable_preambles = false;
    let thread_id = ThreadId::new();

    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeItemAdded(ThreadRealtimeItemAddedNotification {
            thread_id: thread_id.to_string(),
            item: serde_json::json!({
                "type": "handoff_request",
                "handoff_id": "handoff-1"
            }),
        }),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::TurnStarted(TurnStartedNotification {
            thread_id: thread_id.to_string(),
            turn: AppServerTurn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: AppServerTurnStatus::InProgress,
                error: None,
                started_at: Some(1),
                completed_at: None,
                duration_ms: None,
            },
        }),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
            item: AppServerThreadItem::UserMessage {
                id: "voice-user-1".to_string(),
                client_id: None,
                content: vec![AppServerUserInput::Text {
                    text: "<realtime_delegation>branch?</realtime_delegation>".to_string(),
                    text_elements: Vec::new(),
                }],
            },
            completed_at_ms: 2,
        }),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
            item: AppServerThreadItem::AgentMessage {
                id: "commentary-1".to_string(),
                text: String::new(),
                phase: Some(MessagePhase::Commentary),
                memory_citation: None,
            },
            started_at_ms: 3,
        }),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::AgentMessageDelta(
            codex_app_server_protocol::AgentMessageDeltaNotification {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "commentary-1".to_string(),
                delta: "Checking that now.".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
            item: AppServerThreadItem::AgentMessage {
                id: "commentary-1".to_string(),
                text: "Checking that now.".to_string(),
                phase: Some(MessagePhase::Commentary),
                memory_citation: None,
            },
            completed_at_ms: 4,
        }),
        /*replay_kind*/ None,
    );
    assert!(chat.active_cell_transcript_lines(/*width*/ 80).is_none());
    assert!(rx.try_recv().is_err());

    handle_turn_completed(&mut chat, "turn-1", /*duration_ms*/ None);
    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
            item: AppServerThreadItem::AgentMessage {
                id: "final-1".to_string(),
                text: String::new(),
                phase: Some(MessagePhase::FinalAnswer),
                memory_citation: None,
            },
            started_at_ms: 5,
        }),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDelta(
            ThreadRealtimeTranscriptDeltaNotification {
                thread_id: thread_id.to_string(),
                role: "assistant".to_string(),
                delta: "We're on agent/realtime-preamble-fix.".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDone(
            ThreadRealtimeTranscriptDoneNotification {
                thread_id: thread_id.to_string(),
                role: "assistant".to_string(),
                text: "We're on agent/realtime-preamble-fix.".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            assert!(render_lines(cell.as_ref()).contains("agent/realtime-preamble-fix"));
        }
        other => panic!("expected final realtime transcript cell, got {other:?}"),
    }
}

#[tokio::test]
async fn realtime_output_remains_visible_after_interrupted_handoff() {
    let (mut chat, _app_event_tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
    chat.config.realtime.enable_preambles = false;
    let thread_id = ThreadId::new();

    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeItemAdded(ThreadRealtimeItemAddedNotification {
            thread_id: thread_id.to_string(),
            item: serde_json::json!({
                "type": "handoff_request",
                "handoff_id": "handoff-1"
            }),
        }),
        /*replay_kind*/ None,
    );
    chat.on_interrupted_turn(TurnAbortReason::Interrupted);
    while rx.try_recv().is_ok() {}

    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDone(
            ThreadRealtimeTranscriptDoneNotification {
                thread_id: thread_id.to_string(),
                role: "assistant".to_string(),
                text: "The interrupted turn is no longer muted.".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            assert!(render_lines(cell.as_ref()).contains("no longer muted"));
        }
        other => panic!("expected post-interruption transcript cell, got {other:?}"),
    }
}

#[tokio::test]
async fn voice_history_command_renders_recent_transcript_entries() {
    let (mut chat, _app_event_tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);

    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDone(
            ThreadRealtimeTranscriptDoneNotification {
                thread_id: thread_id.to_string(),
                role: "user".to_string(),
                text: "what did you change?".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::ThreadRealtimeTranscriptDone(
            ThreadRealtimeTranscriptDoneNotification {
                thread_id: thread_id.to_string(),
                role: "assistant".to_string(),
                text: "I added a voice history command.".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    while rx.try_recv().is_ok() {}

    chat.dispatch_command_with_args(SlashCommand::Voice, "history 2".to_string(), Vec::new());

    match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            let rendered = render_lines(cell.as_ref());
            insta::assert_snapshot!("voice_history_command", rendered);
            assert!(rendered.contains("Recent GPT-Live transcript (2 entries)"));
            assert!(rendered.contains("› what did you change?"));
            assert!(rendered.contains("• I added a voice history command."));
        }
        other => panic!("expected realtime history output, got {other:?}"),
    }
}

#[tokio::test]
async fn realtime_help_commands_render_usage() {
    let (mut chat, _app_event_tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;

    chat.dispatch_command_with_args(SlashCommand::Mic, "help".to_string(), Vec::new());
    let mic_help = match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => render_lines(cell.as_ref()),
        other => panic!("expected mic help output, got {other:?}"),
    };

    chat.dispatch_command_with_args(SlashCommand::Voice, "help".to_string(), Vec::new());
    let voice_help = match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => render_lines(cell.as_ref()),
        other => panic!("expected voice help output, got {other:?}"),
    };

    chat.dispatch_command_with_args(SlashCommand::Mic, "alias help".to_string(), Vec::new());
    match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            assert!(render_lines(cell.as_ref()).contains("Usage: /mic"));
        }
        other => panic!("expected reserved mic alias help output, got {other:?}"),
    }

    chat.dispatch_command_with_args(SlashCommand::Mic, "alias ?".to_string(), Vec::new());
    match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            assert!(render_lines(cell.as_ref()).contains("Usage: /mic"));
        }
        other => panic!("expected reserved mic alias question-mark output, got {other:?}"),
    }

    insta::assert_snapshot!("realtime_help_commands", format!("{mic_help}{voice_help}"));
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
