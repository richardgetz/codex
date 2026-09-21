use super::*;
use crate::app::test_support::make_test_app_with_channels;
use crate::app_event::AppEvent;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SlashCommandExecuteParams;
use codex_app_server_protocol::SlashCommandExecuteResponse;
use codex_app_server_protocol::SlashCommandResultKind;
use codex_app_server_protocol::SlashCommandResultNotification;
use codex_app_server_protocol::SlashCommandResultPayload;
use codex_app_server_protocol::SlashCommandOutput;
use codex_app_server_protocol::SlashCommandReloadResult;
use codex_protocol::ThreadId;

fn reload(state: &str) -> SlashCommandReloadResult {
    SlashCommandReloadResult {
        eligible: true,
        state: state.to_string(),
        reason: None,
    }
}

fn completed_reload_response() -> SlashCommandExecuteResponse {
    SlashCommandExecuteResponse {
        command: "reload".to_string(),
        ok: true,
        result_kind: SlashCommandResultKind::Reload,
        output: SlashCommandOutput {
            format: "text".to_string(),
            text: "reload complete".to_string(),
        },
        reload: Some(reload("completed")),
    }
}

fn completed_reload_notification(
    thread_id: ThreadId,
    request_id: RequestId,
) -> SlashCommandResultNotification {
    SlashCommandResultNotification {
        thread_id: thread_id.to_string(),
        command: "reload".to_string(),
        request_id,
        result: SlashCommandResultPayload {
            command: "reload".to_string(),
            ok: true,
            result_kind: SlashCommandResultKind::Reload,
            output: SlashCommandOutput {
                format: "text".to_string(),
                text: "reload complete".to_string(),
            },
            reload: Some(reload("completed")),
        },
    }
}

fn count_reload_history_and_completion_events(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> (usize, usize) {
    let mut history_cells = 0;
    let mut completions = 0;
    while let Ok(event) = events.try_recv() {
        match event {
            AppEvent::InsertHistoryCell(_) => history_cells += 1,
            AppEvent::RemoteReloadCompleted { .. } => completions += 1,
            _ => {}
        }
    }
    (history_cells, completions)
}

#[test]
fn remote_reload_state_keeps_accepted_and_in_progress_recoverable() {
    assert_eq!(
        remote_reload_state(Some(&reload("accepted")), true),
        RemoteReloadState::Accepted
    );
    assert_eq!(
        remote_reload_state(Some(&reload("in_progress")), true),
        RemoteReloadState::InProgress
    );
    assert!(!remote_reload_state(Some(&reload("accepted")), true).is_terminal());
    assert!(!remote_reload_state(Some(&reload("in_progress")), true).is_terminal());
}

#[test]
fn remote_reload_state_only_server_terminal_states_clear_pending_work() {
    for state in ["completed", "failed", "unavailable"] {
        assert!(remote_reload_state(Some(&reload(state)), false).is_terminal());
    }
    assert!(remote_reload_state(None, false).is_terminal());
    assert!(!remote_reload_state(None, true).is_terminal());
}

#[test]
fn remote_reload_bridge_preserves_status_and_recover_arguments() {
    for args in ["", "status", "recover"] {
        let params = SlashCommandExecuteParams {
            thread_id: "thread-1".to_string(),
            command: "reload".to_string(),
            args: args.to_string(),
        };
        let value = serde_json::to_value(params).expect("slash command params serialize");
        assert_eq!(value["threadId"], "thread-1");
        assert_eq!(value["command"], "reload");
        assert_eq!(value["args"], args);
    }
}

#[test]
fn remote_reload_status_keeps_operation_notification_correlation() {
    let operation_request_id = RequestId::Integer(7);
    let status_request_id = RequestId::Integer(9);
    let pending = PendingRemoteReload {
        thread_id: ThreadId::new(),
        request_id: operation_request_id.clone(),
        status_request_id: Some(status_request_id.clone()),
        allow_frontend_refresh: true,
    };

    assert!(pending_reload_request_matches(
        &pending,
        &operation_request_id
    ));
    assert!(pending_reload_request_matches(&pending, &status_request_id));
    assert!(!pending_reload_request_matches(
        &pending,
        &RequestId::Integer(11)
    ));
}

#[test]
fn remote_reload_suppresses_only_duplicate_mutations() {
    assert!(suppress_duplicate_reload(false, true));
    assert!(!suppress_duplicate_reload(true, true));
    assert!(!suppress_duplicate_reload(false, false));
}

#[test]
fn remote_reload_terminal_events_are_deduplicated_after_pending_clear() {
    assert_eq!(
        remote_reload_terminal_action(
            RemoteReloadState::Completed,
            /*operation_request_match*/ true,
            /*status_request_match*/ false,
            /*allow_frontend_refresh*/ true,
        ),
        RemoteReloadTerminalAction::Completed
    );
    assert_eq!(
        remote_reload_terminal_action(
            RemoteReloadState::Completed,
            /*operation_request_match*/ false,
            /*status_request_match*/ false,
            /*allow_frontend_refresh*/ true,
        ),
        RemoteReloadTerminalAction::Ignore
    );
}

#[tokio::test]
async fn remote_reload_terminal_handlers_present_one_result_in_either_arrival_order() {
    let thread_id = ThreadId::new();
    let request_id = RequestId::Integer(7);

    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    app.reconnect.pending_remote_reload = Some(PendingRemoteReload {
        thread_id,
        request_id: request_id.clone(),
        status_request_id: None,
        allow_frontend_refresh: true,
    });
    app.apply_remote_reload_response(
        thread_id,
        request_id.clone(),
        completed_reload_response(),
        /*allow_new_pending*/ true,
        /*display_progress*/ true,
        /*is_status_request*/ false,
    );
    app.handle_remote_reload_notification(&completed_reload_notification(
        thread_id,
        request_id.clone(),
    ));
    assert_eq!(
        count_reload_history_and_completion_events(&mut events),
        (1, 1)
    );

    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    app.reconnect.pending_remote_reload = Some(PendingRemoteReload {
        thread_id,
        request_id: request_id.clone(),
        status_request_id: None,
        allow_frontend_refresh: true,
    });
    app.handle_remote_reload_notification(&completed_reload_notification(
        thread_id,
        request_id.clone(),
    ));
    app.apply_remote_reload_response(
        thread_id,
        request_id,
        completed_reload_response(),
        /*allow_new_pending*/ true,
        /*display_progress*/ true,
        /*is_status_request*/ false,
    );
    assert_eq!(
        count_reload_history_and_completion_events(&mut events),
        (1, 1)
    );
}

#[test]
fn remote_reload_unknown_status_keeps_the_original_operation_pending() {
    assert_eq!(
        remote_reload_terminal_action(
            RemoteReloadState::Unknown,
            /*operation_request_match*/ false,
            /*status_request_match*/ true,
            /*allow_frontend_refresh*/ true,
        ),
        RemoteReloadTerminalAction::KeepPending
    );
    assert_eq!(
        remote_reload_terminal_action(
            RemoteReloadState::Unavailable,
            /*operation_request_match*/ false,
            /*status_request_match*/ true,
            /*allow_frontend_refresh*/ false,
        ),
        RemoteReloadTerminalAction::ClearPending
    );
}
