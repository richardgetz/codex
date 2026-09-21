use super::*;
use codex_app_server_protocol::SlashCommandExecuteParams;
use codex_app_server_protocol::SlashCommandReloadResult;

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
