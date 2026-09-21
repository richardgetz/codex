use super::*;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SlashCommandExecuteParams;
use codex_app_server_protocol::SlashCommandReloadResult;
use codex_protocol::ThreadId;

fn reload(state: &str) -> SlashCommandReloadResult {
    SlashCommandReloadResult {
        eligible: true,
        state: state.to_string(),
        reason: None,
    }
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
