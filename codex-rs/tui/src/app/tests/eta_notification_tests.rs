use super::*;
use crate::app::eta_view::ETA_ACTIVE_TAB_ID;
use crate::app::eta_view::ETA_ALL_SESSIONS_TAB_ID;
use crate::app::eta_view::ETA_HISTORY_TAB_ID;
use crate::app::eta_view::EtaSessionInfo;
use crate::app::eta_view::EtaSessionTask;
use crate::app::eta_view::EtaTaskStatus;
use crate::app::eta_view::EtaViewState;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadEtaAccuracy;
use codex_app_server_protocol::ThreadEtaOverall;
use codex_app_server_protocol::ThreadEtaListResponse;
use codex_app_server_protocol::ThreadEtaSessionInfo;
use codex_app_server_protocol::ThreadEtaSessionTask;
use codex_app_server_protocol::ThreadEtaStatus;
use codex_app_server_protocol::ThreadEtaTask;
use codex_app_server_protocol::ThreadEtaUpdatedNotification;
use codex_utils_path_uri::LegacyAppPathString;
use codex_protocol::ThreadId;

fn saved_state(tab_id: &str, selected_task_id: Option<&str>) -> EtaViewState {
    EtaViewState {
        tab_id: tab_id.to_string(),
        selected_task_id: selected_task_id.map(str::to_string),
        ..Default::default()
    }
}

fn known_session_task(root_thread_id: ThreadId) -> EtaSessionTask {
    EtaSessionTask {
        task_id: "target-task".to_string(),
        root_thread_id: root_thread_id.to_string(),
        parent_task_id: None,
        title: "Target session".to_string(),
        status: EtaTaskStatus::Active,
        current_lower_seconds: Some(1),
        current_upper_seconds: Some(2),
        is_stale: false,
        session: EtaSessionInfo {
            thread_id: root_thread_id.to_string(),
            title: "Target session".to_string(),
            name: None,
            preview: None,
            created_at: 1,
            updated_at: 1,
            archived_at: None,
            cwd: "/tmp".to_string(),
        },
        nested_task_count: 0,
        active_nested_task_count: 0,
        nested_lower_seconds: None,
        nested_upper_seconds: None,
    }
}

fn api_session_task(root_thread_id: ThreadId, task_id: &str) -> ThreadEtaSessionTask {
    ThreadEtaSessionTask {
        task_id: task_id.to_string(),
        root_thread_id: root_thread_id.to_string(),
        owner_thread_id: root_thread_id.to_string(),
        parent_task_id: None,
        title: "Updated task".to_string(),
        status: ThreadEtaStatus::Active,
        current_lower_seconds: Some(1),
        current_upper_seconds: Some(2),
        original_lower_seconds: Some(1),
        original_upper_seconds: Some(2),
        created_at: 1,
        started_at: Some(1),
        terminal_at: None,
        actual_elapsed_seconds: None,
        updated_at: 2,
        is_stale: false,
        accuracy: ThreadEtaAccuracy::Unknown,
        revisions: Vec::new(),
        session: ThreadEtaSessionInfo {
            thread_id: root_thread_id.to_string(),
            title: "Updated session".to_string(),
            name: None,
            preview: None,
            created_at: 1,
            updated_at: 2,
            archived_at: None,
            cwd: LegacyAppPathString::from_string("/tmp"),
        },
        nested_task_count: 0,
        active_nested_task_count: 0,
        nested_lower_seconds: None,
        nested_upper_seconds: None,
    }
}

#[tokio::test]
async fn eta_notification_refreshes_all_sessions_for_selected_root() -> color_eyre::Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let mut app_server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let started = app_server.start_thread(&app.config).await?;
    let root_thread_id = started.session.thread_id;
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    app.apply_eta_view_state(
        root_thread_id.to_string(),
        saved_state(ETA_ALL_SESSIONS_TAB_ID, None),
    );

    app.open_eta(&app_server);
    assert_eq!(
        app.chat_widget
            .active_tab_id_for_active_view(crate::app::eta_view::ETA_VIEW_ID),
        Some(ETA_ALL_SESSIONS_TAB_ID)
    );
    let previous_request_id = app
        .eta
        .all_sessions_request_id
        .expect("opening ETA should request All Sessions");
    app.handle_app_server_event(
        &app_server,
        AppServerEvent::ServerNotification(Box::new(
            ServerNotification::ThreadEtaUpdated(ThreadEtaUpdatedNotification {
                root_thread_id: root_thread_id.to_string(),
                generated_at: 1,
                sequence: 1,
                changed_tasks: vec![ThreadEtaTask {
                    task_id: "selected-task".to_string(),
                    root_thread_id: root_thread_id.to_string(),
                    owner_thread_id: root_thread_id.to_string(),
                    parent_task_id: None,
                    depends_on_task_ids: Vec::new(),
                    title: "Selected task".to_string(),
                    status: ThreadEtaStatus::Active,
                    current_lower_seconds: Some(30),
                    current_upper_seconds: Some(90),
                    original_lower_seconds: Some(60),
                    original_upper_seconds: Some(120),
                    created_at: 1,
                    started_at: Some(1),
                    terminal_at: None,
                    actual_elapsed_seconds: None,
                    updated_at: 1,
                    is_stale: true,
                    accuracy: ThreadEtaAccuracy::Unknown,
                    revisions: Vec::new(),
                }],
                overall: ThreadEtaOverall {
                    finish_at: None,
                    remaining_lower_seconds: None,
                    remaining_upper_seconds: None,
                    unknown_reason: Some("test update".to_string()),
                },
            }),
        )),
    )
    .await;

    assert_ne!(app.eta.all_sessions_request_id, Some(previous_request_id));
    assert_eq!(
        app.eta.snapshot.as_ref().map(|snapshot| (
            snapshot.root_thread_id.clone(),
            snapshot.sequence,
        )),
        Some((root_thread_id.to_string(), 1)),
    );
    let task = &app.eta.snapshot.as_ref().expect("ETA snapshot").active[0];
    assert_eq!(task.task_id, "selected-task");
    assert!(task.is_stale);

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn opening_saved_all_sessions_shows_loading_before_response() -> color_eyre::Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let mut app_server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let started = app_server.start_thread(&app.config).await?;
    let root_thread_id = started.session.thread_id;
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    app.apply_eta_view_state(
        root_thread_id.to_string(),
        saved_state(ETA_ALL_SESSIONS_TAB_ID, None),
    );

    app.open_eta(&app_server);

    let status = render_bottom_popup(&app.chat_widget, 80)
        .lines()
        .map(str::trim)
        .find(|line| line.contains("Loading retained sessions"))
        .expect("All Sessions should show loading before the first response");
    insta::assert_snapshot!(status, @"Loading retained sessions…");

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn eta_root_view_state_isolated_between_selected_roots() {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let first_root = ThreadId::new();
    let second_root = ThreadId::new();
    let all_sessions_state = EtaViewState {
        selected_session: Some((first_root.to_string(), "retained-task".to_string())),
        ..saved_state(ETA_ALL_SESSIONS_TAB_ID, None)
    };

    app.apply_eta_view_state(
        first_root.to_string(),
        saved_state(ETA_ACTIVE_TAB_ID, Some("first-task")),
    );
    app.apply_eta_view_state(first_root.to_string(), all_sessions_state.clone());
    app.apply_eta_view_state(
        second_root.to_string(),
        saved_state(ETA_HISTORY_TAB_ID, Some("second-task")),
    );

    assert_eq!(
        app.eta
            .view_state_for_root(first_root, ETA_ACTIVE_TAB_ID)
            .and_then(|state| state.selected_task_id),
        Some("first-task".to_string())
    );
    assert_eq!(
        app.eta
            .view_state_for_root(second_root, ETA_HISTORY_TAB_ID)
            .and_then(|state| state.selected_task_id),
        Some("second-task".to_string())
    );
    assert_eq!(
        app.eta
            .view_state_for_root(second_root, ETA_ACTIVE_TAB_ID),
        None
    );
    assert_eq!(
        app.eta
            .view_state_for_root(second_root, ETA_ALL_SESSIONS_TAB_ID),
        Some(all_sessions_state)
    );
}

#[tokio::test]
async fn eta_auto_refresh_replaces_loaded_depth_and_preserves_selection() {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let root_thread_id = ThreadId::new();
    let tail_root_thread_id = ThreadId::new();
    let removed_root_thread_id = ThreadId::new();
    app.eta.root_thread_id = Some(root_thread_id);
    app.apply_eta_view_state(
        root_thread_id.to_string(),
        EtaViewState {
            tab_id: ETA_ALL_SESSIONS_TAB_ID.to_string(),
            selected_session: Some((
                tail_root_thread_id.to_string(),
                "tail-task".to_string(),
            )),
            ..Default::default()
        },
    );

    let first_page_request_id = uuid::Uuid::new_v4();
    app.eta.all_sessions_request_id = Some(first_page_request_id);
    app.apply_eta_sessions(
        first_page_request_id,
        None,
        false,
        Ok(ThreadEtaListResponse {
            data: vec![
                api_session_task(root_thread_id, "head-task"),
                api_session_task(removed_root_thread_id, "removed-task"),
            ],
            next_cursor: Some("tail-cursor".to_string()),
        }),
    );

    let tail_page_request_id = uuid::Uuid::new_v4();
    app.eta.all_sessions_request_id = Some(tail_page_request_id);
    app.apply_eta_sessions(
        tail_page_request_id,
        Some("tail-cursor".to_string()),
        false,
        Ok(ThreadEtaListResponse {
            data: vec![api_session_task(tail_root_thread_id, "tail-task")],
            next_cursor: None,
        }),
    );

    let request_id = uuid::Uuid::new_v4();
    app.eta.all_sessions_request_id = Some(request_id);

    app.apply_eta_sessions(
        request_id,
        None,
        false,
        Ok(ThreadEtaListResponse {
            data: vec![
                api_session_task(tail_root_thread_id, "tail-task"),
                api_session_task(root_thread_id, "head-task"),
            ],
            next_cursor: None,
        }),
    );

    assert_eq!(app.eta.all_sessions.len(), 2);
    assert_eq!(
        app.eta
            .all_sessions
            .iter()
            .map(|task| task.task_id.as_str())
            .collect::<Vec<_>>(),
        vec!["tail-task", "head-task"]
    );
    assert_eq!(app.eta.all_sessions_next_cursor, None);
    assert_eq!(
        app.eta
            .all_sessions_view_state
            .as_ref()
            .and_then(|state| state.selected_session.clone()),
        Some((tail_root_thread_id.to_string(), "tail-task".to_string()))
    );

    let rows_before_error = app.eta.all_sessions.clone();
    let error_request_id = uuid::Uuid::new_v4();
    app.eta.all_sessions_request_id = Some(error_request_id);
    app.apply_eta_sessions(
        error_request_id,
        None,
        true,
        Err("refresh failed".to_string()),
    );
    assert_eq!(app.eta.all_sessions, rows_before_error);
    assert!(!app.eta.all_sessions_include_nested);
}

#[tokio::test]
async fn resume_eta_session_confirms_when_worker_is_working() -> color_eyre::Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let active_root = ThreadId::new();
    let displayed_worker = ThreadId::new();
    let worker_thread = ThreadId::new();
    let target_root = ThreadId::new();
    app.active_thread_id = Some(displayed_worker);
    app.primary_thread_id = Some(active_root);
    app.team_activity.replace_thread_metadata(
        Some(active_root),
        [
            (active_root, None),
            (displayed_worker, Some(active_root)),
            (worker_thread, Some(active_root)),
        ],
    );
    app.team_activity
        .observe(&ThreadActivityUpdatedNotification {
            thread_id: active_root.to_string(),
            root_thread_id: active_root.to_string(),
            activity: codex_app_server_protocol::ThreadActivity::Idle,
            pause_state: codex_app_server_protocol::ThreadPauseState::Running,
            wait_reason: None,
            in_flight_operations: 0,
        });
    app.team_activity
        .observe(&ThreadActivityUpdatedNotification {
            thread_id: displayed_worker.to_string(),
            root_thread_id: active_root.to_string(),
            activity: codex_app_server_protocol::ThreadActivity::Idle,
            pause_state: codex_app_server_protocol::ThreadPauseState::Running,
            wait_reason: None,
            in_flight_operations: 0,
        });
    app.team_activity
        .observe(&ThreadActivityUpdatedNotification {
            thread_id: worker_thread.to_string(),
            root_thread_id: active_root.to_string(),
            activity: codex_app_server_protocol::ThreadActivity::Working,
            pause_state: codex_app_server_protocol::ThreadPauseState::Running,
            wait_reason: None,
            in_flight_operations: 1,
        });
    app.eta.all_sessions = vec![known_session_task(target_root)];

    let mut app_server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let control = app
        .resume_eta_session_target(
            &mut tui,
            &mut app_server,
            crate::resume_picker::SessionTarget {
                path: None,
                thread_id: target_root,
                cwd: None,
                history_mode: None,
            },
            false,
        )
        .await?;

    assert!(matches!(control, AppRunControl::Continue));
    assert!(render_bottom_popup(&app.chat_widget, 80).contains("Resume another active session?"));

    app_server.shutdown().await?;
    Ok(())
}
