use super::*;
use crate::chatwidget::TeamActivityStatus;
use crate::chatwidget::TeamPauseState;
use crate::chatwidget::TeamRoleActivity;

#[tokio::test]
async fn worker_only_activity_keeps_an_animated_row_without_a_local_turn() {
    let (mut chat, _events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_team_activity(Some(TeamActivityStatus {
        lead: TeamRoleActivity::Idle,
        workers_working: 2,
        workers_waiting: 0,
        pause_state: TeamPauseState::Running,
        in_flight_operations: 2,
    }));

    assert!(!chat.is_task_running_for_test());
    assert_eq!(
        chat.team_activity_status_header(),
        Some("Lead: idle · Workers: 2 working".to_string())
    );
    assert_eq!(
        chat.run_state_status_text(),
        "Lead: idle · Workers: 2 working"
    );
    assert!(
        chat.terminal_title_spinner_text_at(std::time::Instant::now())
            .is_some()
    );
    let status = chat.bottom_pane.status_widget().expect("team status row");
    assert!(status.animations_enabled());
    assert!(!status.elapsed_visible());
    insta::assert_snapshot!(render_bottom_first_row(&chat, /*width*/ 80));
}

#[tokio::test]
async fn lead_and_workers_activity_share_the_animated_row() {
    let (mut chat, _events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_team_activity(Some(TeamActivityStatus {
        lead: TeamRoleActivity::Working,
        workers_working: 1,
        workers_waiting: 1,
        pause_state: TeamPauseState::Running,
        in_flight_operations: 2,
    }));

    assert_eq!(
        chat.team_activity_status_header(),
        Some("Lead: working · Workers: 1 working, 1 waiting".to_string())
    );
    assert!(
        chat.bottom_pane
            .status_widget()
            .expect("team status row")
            .animations_enabled()
    );
    insta::assert_snapshot!(render_bottom_first_row(&chat, /*width*/ 80));
}

#[tokio::test]
async fn pause_states_animate_drain_then_show_static_resume_copy() {
    let (mut chat, _events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_team_activity(Some(TeamActivityStatus {
        lead: TeamRoleActivity::Idle,
        workers_working: 0,
        workers_waiting: 0,
        pause_state: TeamPauseState::Pausing,
        in_flight_operations: 1,
    }));
    assert_eq!(
        chat.team_activity_status_header(),
        Some("Pausing — finishing 1 in-flight operation".to_string())
    );
    assert_eq!(
        chat.run_state_status_text(),
        "Pausing — finishing 1 in-flight operation"
    );
    assert!(
        chat.terminal_title_spinner_text_at(std::time::Instant::now())
            .is_some()
    );
    assert!(
        chat.bottom_pane
            .status_widget()
            .expect("pausing status row")
            .animations_enabled()
    );
    insta::assert_snapshot!(render_bottom_first_row(&chat, /*width*/ 80));

    chat.set_team_activity(Some(TeamActivityStatus {
        pause_state: TeamPauseState::Paused,
        workers_working: 2,
        workers_waiting: 1,
        ..TeamActivityStatus::default()
    }));
    assert_eq!(
        chat.team_activity_status_header(),
        Some("Paused · Lead + 3 workers · /continue to resume".to_string())
    );
    assert_eq!(
        chat.run_state_status_text(),
        "Paused · Lead + 3 workers · /continue to resume"
    );
    assert!(
        chat.terminal_title_spinner_text_at(std::time::Instant::now())
            .is_none()
    );
    assert!(
        !chat
            .bottom_pane
            .status_widget()
            .expect("paused status row")
            .animations_enabled()
    );
    insta::assert_snapshot!(render_bottom_first_row(&chat, /*width*/ 80));
}
