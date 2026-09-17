use super::*;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadEtaAccuracy;
use codex_app_server_protocol::ThreadEtaOverall;
use codex_app_server_protocol::ThreadEtaStatus;
use codex_app_server_protocol::ThreadEtaTask;
use codex_app_server_protocol::ThreadEtaUpdatedNotification;

#[tokio::test]
async fn eta_notification_refreshes_all_sessions_for_selected_root() -> color_eyre::Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let mut app_server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let started = app_server.start_thread(&app.config).await?;
    let root_thread_id = started.session.thread_id;
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;

    app.open_eta(&app_server);
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
