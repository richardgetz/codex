use super::*;
use codex_app_server_protocol::ThreadActivityUpdatedNotification;
use pretty_assertions::assert_eq;

fn notification(
    thread_id: ThreadId,
    root_thread_id: ThreadId,
    activity: ThreadActivity,
    pause_state: ThreadPauseState,
    in_flight_operations: u32,
) -> ThreadActivityUpdatedNotification {
    ThreadActivityUpdatedNotification {
        thread_id: thread_id.to_string(),
        root_thread_id: root_thread_id.to_string(),
        activity,
        pause_state,
        wait_reason: None,
        in_flight_operations,
    }
}

#[test]
fn projection_counts_nested_workers_and_ignores_other_roots() {
    let root = ThreadId::new();
    let worker = ThreadId::new();
    let nested_worker = ThreadId::new();
    let unrelated_root = ThreadId::new();
    let unrelated_worker = ThreadId::new();
    let mut projection = TeamActivityProjection::default();

    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    projection.observe(&notification(
        worker,
        root,
        ThreadActivity::Working,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 1,
    ));
    projection.observe(&notification(
        nested_worker,
        root,
        ThreadActivity::Waiting,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    projection.observe(&notification(
        unrelated_root,
        unrelated_root,
        ThreadActivity::Idle,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    projection.observe(&notification(
        unrelated_worker,
        unrelated_root,
        ThreadActivity::Working,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 1,
    ));

    let status = projection
        .status_for_root(root)
        .expect("active worker projection");
    assert_eq!(
        status,
        TeamActivityStatus {
            lead: TeamRoleActivity::Idle,
            workers_working: 1,
            workers_waiting: 1,
            pause_state: UiPauseState::Running,
            in_flight_operations: 1,
        }
    );
    assert_eq!(
        status.header(),
        "Lead: idle · Workers: 1 working, 1 waiting"
    );
    assert!(status.is_animated());

    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Paused,
        /*in_flight_operations*/ 0,
    ));
    projection.observe(&notification(
        worker,
        root,
        ThreadActivity::Working,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    let status = projection.status_for_root(root).expect("root activity");
    assert_eq!(status.pause_state, UiPauseState::Paused);
    assert_eq!(
        status.header(),
        "Paused · Lead + 2 workers · /continue to resume"
    );
}

#[test]
fn projection_renders_pause_transition_and_clears_when_idle() {
    let root = ThreadId::new();
    let worker = ThreadId::new();
    let mut projection = TeamActivityProjection::default();

    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Working,
        ThreadPauseState::Pausing,
        /*in_flight_operations*/ 2,
    ));
    projection.observe(&notification(
        worker,
        root,
        ThreadActivity::Working,
        ThreadPauseState::Pausing,
        /*in_flight_operations*/ 1,
    ));
    let pausing = projection
        .status_for_root(root)
        .expect("pausing projection");
    assert_eq!(
        pausing.header(),
        "Pausing — finishing 3 in-flight operations"
    );
    assert!(pausing.is_animated());

    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Paused,
        /*in_flight_operations*/ 0,
    ));
    let still_pausing = projection
        .status_for_root(root)
        .expect("draining projection");
    assert_eq!(
        still_pausing.header(),
        "Pausing — finishing 1 in-flight operation"
    );
    assert!(still_pausing.is_animated());

    projection.observe(&notification(
        worker,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Paused,
        /*in_flight_operations*/ 0,
    ));
    let paused = projection.status_for_root(root).expect("paused projection");
    assert_eq!(paused.header(), "Paused — /continue to resume");
    assert!(!paused.is_animated());

    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    assert_eq!(projection.status_for_root(root), None);
}
