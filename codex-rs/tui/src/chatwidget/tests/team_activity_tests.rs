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
        direct_workers: 2,
        subagents: 0,
        worker_max_concurrent: None,
        pause_state: TeamPauseState::Running,
        in_flight_operations: 2,
    }));

    assert!(!chat.is_task_running_for_test());
    assert_eq!(
        chat.team_activity_status_header(),
        Some("Lead: idle · Team: 2 working".to_string())
    );
    assert_eq!(
        chat.run_state_status_text(),
        "Lead: idle · Team: 2 working · Workers: 2 · Subagents: 0"
    );
    assert!(
        chat.terminal_title_spinner_text_at(std::time::Instant::now())
            .is_some()
    );
    let status = chat.bottom_pane.status_widget().expect("team status row");
    assert!(status.animations_enabled());
    assert!(!status.elapsed_visible());
    insta::assert_snapshot!(render_bottom_rows(&chat, /*width*/ 80));
}

#[tokio::test]
async fn lead_and_workers_activity_share_the_animated_row() {
    let (mut chat, _events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_team_activity(Some(TeamActivityStatus {
        lead: TeamRoleActivity::Working,
        workers_working: 1,
        workers_waiting: 1,
        direct_workers: 1,
        subagents: 1,
        worker_max_concurrent: None,
        pause_state: TeamPauseState::Running,
        in_flight_operations: 2,
    }));

    assert_eq!(
        chat.team_activity_status_header(),
        Some("Lead: working · Team: 1 working, 1 waiting".to_string())
    );
    assert!(
        chat.bottom_pane
            .status_widget()
            .expect("team status row")
            .animations_enabled()
    );
    insta::assert_snapshot!(render_bottom_rows(&chat, /*width*/ 80));
}

#[tokio::test]
async fn pause_states_animate_drain_then_show_static_resume_copy() {
    let (mut chat, _events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_team_activity(Some(TeamActivityStatus {
        lead: TeamRoleActivity::Idle,
        workers_working: 0,
        workers_waiting: 0,
        direct_workers: 0,
        subagents: 0,
        worker_max_concurrent: None,
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

#[test]
fn running_rows_align_direct_cap_and_nested_counts() {
    let status = TeamActivityStatus {
        lead: TeamRoleActivity::Idle,
        workers_working: 14,
        workers_waiting: 2,
        direct_workers: 10,
        subagents: 6,
        worker_max_concurrent: Some(10),
        pause_state: TeamPauseState::Running,
        in_flight_operations: 0,
    };

    assert_eq!(
        status.lines(/*width*/ 80),
        vec![
            "Lead: idle     · Team: 14 working, 2 waiting".to_string(),
            "Workers: 10/10 · Subagents: 6".to_string(),
        ]
    );
    assert_eq!(
        status.title(),
        "Lead: idle · Team: 14 working, 2 waiting · Workers: 10/10 · Subagents: 6"
    );
}

fn render_bottom_rows(chat: &ChatWidget, width: u16) -> String {
    let height = chat.desired_height(width);
    let area = ratatui::layout::Rect::new(0, 0, width, height);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    chat.render(area, &mut buffer);
    (0..area.height)
        .filter_map(|y| {
            let row = (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            (!row.trim().is_empty()).then(|| row.trim_end().to_string())
        })
        .take(2)
        .collect::<Vec<_>>()
        .join("\n")
}
