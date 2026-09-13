use super::rejects_new_work_method;
use super::ServerLifecycle;
use codex_app_server_protocol::ServerLifecyclePhase;
use pretty_assertions::assert_eq;

#[test]
fn starts_ready_with_a_process_identity() {
    let lifecycle = ServerLifecycle::new();
    let response = lifecycle.read();

    assert_eq!(response.phase, ServerLifecyclePhase::Ready);
    assert!(response.transition_id.is_none());
    assert_eq!(response.running_assistant_turns, 0);
    assert!(!response.daemon_instance_id.is_empty());
}

#[test]
fn drain_and_force_share_one_transition_id() {
    let lifecycle = ServerLifecycle::new();
    let draining = lifecycle.begin_drain().expect("first drain should transition");
    let duplicate = lifecycle.begin_drain();
    let forced = lifecycle.force_drain().expect("force should transition");

    assert!(duplicate.is_none());
    assert_eq!(draining.phase, ServerLifecyclePhase::Draining);
    assert_eq!(draining.running_assistant_turns, 0);
    assert_eq!(forced.phase, ServerLifecyclePhase::Forced);
    assert_eq!(draining.transition_id, forced.transition_id);
    assert_eq!(draining.daemon_instance_id, forced.daemon_instance_id);
}

#[test]
fn turn_count_updates_are_only_notified_while_draining() {
    let lifecycle = ServerLifecycle::new();

    assert!(
        lifecycle
            .update_running_assistant_turns(/*running_assistant_turns*/ 1)
            .is_none()
    );
    lifecycle.begin_drain().expect("drain should transition");
    let update = lifecycle
        .update_running_assistant_turns(/*running_assistant_turns*/ 1)
        .expect("draining turn count should notify");
    assert_eq!(update.running_assistant_turns, 1);
    assert!(
        lifecycle
            .update_running_assistant_turns(/*running_assistant_turns*/ 1)
            .is_none()
    );
}

#[test]
fn only_new_work_methods_are_rejected_during_drain() {
    assert!(rejects_new_work_method("turn/start"));
    assert!(rejects_new_work_method("command/exec"));
    assert!(rejects_new_work_method("process/spawn"));
    assert!(rejects_new_work_method("windowsSandbox/setupStart"));
    assert!(rejects_new_work_method("thread/usage/resume"));
    assert!(rejects_new_work_method("thread/goal/set"));
    assert!(rejects_new_work_method("thread/activity/continue"));
    assert!(rejects_new_work_method("thread/inject_items"));
    assert!(rejects_new_work_method("thread/shellCommand"));
    assert!(rejects_new_work_method("thread/realtime/appendAudio"));
    assert!(rejects_new_work_method("thread/realtime/appendText"));
    assert!(rejects_new_work_method("thread/realtime/appendSpeech"));
    assert!(rejects_new_work_method("mcpServer/event/stream/start"));
    assert!(rejects_new_work_method("mcpServer/tool/call"));
    assert!(!rejects_new_work_method("turn/interrupt"));
    assert!(!rejects_new_work_method("thread/read"));
    assert!(!rejects_new_work_method("item/commandExecution/requestApproval"));
}
