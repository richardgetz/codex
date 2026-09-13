use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadEtaAccuracy;
use codex_app_server_protocol::ThreadEtaAction;
use codex_app_server_protocol::ThreadEtaReadParams;
use codex_app_server_protocol::ThreadEtaReadResponse;
use codex_app_server_protocol::ThreadEtaStatus;
use codex_app_server_protocol::ThreadEtaTask;
use codex_app_server_protocol::ThreadEtaUpdateOperation;
use codex_app_server_protocol::ThreadEtaUpdateParams;
use codex_app_server_protocol::ThreadEtaUpdateResponse;
use codex_app_server_protocol::ThreadEtaUpdatedNotification;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_features::Feature;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn operation(
    action: ThreadEtaAction,
    task_id: Option<&str>,
    title: Option<&str>,
    lower_seconds: Option<i64>,
    upper_seconds: Option<i64>,
    reason: Option<&str>,
) -> ThreadEtaUpdateOperation {
    ThreadEtaUpdateOperation {
        action,
        task_id: task_id.map(str::to_string),
        title: title.map(str::to_string),
        parent_task_id: None,
        depends_on_task_ids: None,
        estimate_lower_seconds: lower_seconds,
        estimate_upper_seconds: upper_seconds,
        reason: reason.map(str::to_string),
    }
}

async fn update(
    app: &mut TestAppServer,
    thread_id: &str,
    operation: ThreadEtaUpdateOperation,
) -> Result<ThreadEtaUpdateResponse> {
    app.request(|request_id| ClientRequest::ThreadEtaUpdate {
        request_id,
        params: ThreadEtaUpdateParams {
            thread_id: thread_id.to_string(),
            operations: vec![operation],
        },
    })
    .await
}

async fn read(app: &mut TestAppServer, thread_id: &str) -> Result<ThreadEtaReadResponse> {
    app.request(|request_id| ClientRequest::ThreadEtaRead {
        request_id,
        params: ThreadEtaReadParams {
            thread_id: thread_id.to_string(),
            cursor: None,
            limit: Some(1),
        },
    })
    .await
}

fn assert_terminal_history(task: &ThreadEtaTask) {
    assert_eq!(task.status, ThreadEtaStatus::Completed);
    assert_eq!(task.original_lower_seconds, Some(10));
    assert_eq!(task.original_upper_seconds, Some(20));
    assert!(task.started_at.is_some());
    assert!(task.terminal_at.is_some());
    assert!(task
        .started_at
        .zip(task.terminal_at)
        .is_some_and(|(started_at, terminal_at)| started_at <= terminal_at));
    let actual = task
        .actual_elapsed_seconds
        .expect("completion should include harness elapsed seconds");
    assert!((0..=20).contains(&actual));
    assert!(matches!(task.accuracy, ThreadEtaAccuracy::Early | ThreadEtaAccuracy::Within));
}

#[tokio::test]
async fn thread_eta_rpc_persists_terminal_history_without_starting_a_turn() -> Result<()> {
    let responses_server = create_mock_responses_server_repeating_assistant("unused").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&responses_server.uri())
        .with_root_config("suppress_unstable_features_warning = true")
        .enable_feature(Feature::Sqlite)
        .write(codex_home.path())?;

    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = app.start_thread(ThreadStartParams::default()).await?;
    let thread_id = thread.id.clone();
    let unknown_thread = "00000000-0000-0000-0000-00000000dead";
    assert!(read(&mut app, unknown_thread).await.is_err());
    assert!(
        update(
            &mut app,
            unknown_thread,
            operation(
                ThreadEtaAction::Create,
                Some("orphan"),
                Some("Orphan task"),
                Some(1),
                Some(1),
                None,
            ),
        )
        .await
        .is_err()
    );

    let created = update(
        &mut app,
        &thread_id,
        operation(
            ThreadEtaAction::Create,
            Some("compile"),
            Some("Compile the project"),
            Some(10),
            Some(20),
            None,
        ),
    )
    .await?;
    assert_eq!(created.changed_tasks.len(), 1);
    assert_eq!(created.changed_tasks[0].status, ThreadEtaStatus::Pending);
    let notification: ThreadEtaUpdatedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_notification("thread/eta/updated"),
    )
    .await??;
    assert_eq!(notification.sequence, created.sequence);

    let started = update(
        &mut app,
        &thread_id,
        operation(
            ThreadEtaAction::Start,
            Some("compile"),
            None,
            None,
            None,
            None,
        ),
    )
    .await?;
    assert_eq!(started.changed_tasks[0].status, ThreadEtaStatus::Active);

    let completed = update(
        &mut app,
        &thread_id,
        operation(
            ThreadEtaAction::Complete,
            Some("compile"),
            None,
            None,
            None,
            Some("build passed"),
        ),
    )
    .await?;
    assert_eq!(completed.changed_tasks[0].status, ThreadEtaStatus::Completed);

    let current = read(&mut app, &thread_id).await?;
    assert!(current.snapshot.active.is_empty());
    assert_eq!(current.snapshot.history.len(), 1);
    assert_terminal_history(&current.snapshot.history[0]);
    assert_eq!(current.snapshot.sequence, completed.sequence);

    drop(app);
    let mut restarted = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let persisted = read(&mut restarted, &thread_id).await?;
    assert_eq!(persisted.snapshot.sequence, current.snapshot.sequence);
    assert!(persisted.snapshot.active.is_empty());
    assert_eq!(persisted.snapshot.history.len(), 1);
    assert_terminal_history(&persisted.snapshot.history[0]);

    let requests = responses_server
        .received_requests()
        .await
        .expect("mock response server should expose requests");
    assert!(requests.is_empty(), "ETA RPCs must not start a model turn");

    Ok(())
}
