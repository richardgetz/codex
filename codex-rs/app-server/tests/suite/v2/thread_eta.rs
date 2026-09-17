use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use chrono::Duration as ChronoDuration;
use chrono::Utc;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadEtaAccuracy;
use codex_app_server_protocol::ThreadEtaAction;
use codex_app_server_protocol::ThreadEtaListParams;
use codex_app_server_protocol::ThreadEtaListResponse;
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
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_state::StateRuntime;
use codex_state::TaskEstimateAction;
use codex_state::TaskEstimateMutation;
use codex_state::TaskEstimateRange;
use codex_state::ThreadMetadataBuilder;
use codex_utils_absolute_path::test_support::PathExt;
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
        owner_thread_id: None,
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

async fn list(app: &mut TestAppServer, include_nested: bool) -> Result<ThreadEtaListResponse> {
    app.request(|request_id| ClientRequest::ThreadEtaList {
        request_id,
        params: ThreadEtaListParams {
            cursor: None,
            limit: Some(10),
            include_nested,
        },
    })
    .await
}

fn state_create_task(
    task_id: &str,
    title: &str,
    lower_seconds: i64,
    upper_seconds: i64,
) -> TaskEstimateMutation {
    TaskEstimateMutation {
        action: TaskEstimateAction::Create,
        task_id: Some(task_id.to_string()),
        title: Some(title.to_string()),
        parent_task_id: None,
        depends_on_task_ids: None,
        estimate: Some(TaskEstimateRange {
            lower_seconds: Some(lower_seconds),
            upper_seconds: Some(upper_seconds),
        }),
        reason: None,
        owner_thread_id: None,
    }
}

fn state_start_task(task_id: &str) -> TaskEstimateMutation {
    TaskEstimateMutation {
        action: TaskEstimateAction::Start,
        task_id: Some(task_id.to_string()),
        title: None,
        parent_task_id: None,
        depends_on_task_ids: None,
        estimate: None,
        reason: None,
        owner_thread_id: None,
    }
}

fn assert_terminal_history(task: &ThreadEtaTask) {
    assert_eq!(task.status, ThreadEtaStatus::Completed);
    assert_eq!(task.original_lower_seconds, Some(10));
    assert_eq!(task.original_upper_seconds, Some(20));
    assert!(task.started_at.is_some());
    assert!(task.terminal_at.is_some());
    assert!(
        task.started_at
            .zip(task.terminal_at)
            .is_some_and(|(started_at, terminal_at)| started_at <= terminal_at)
    );
    let actual = task
        .actual_elapsed_seconds
        .expect("completion should include harness elapsed seconds");
    assert!((0..=20).contains(&actual));
    assert!(matches!(
        task.accuracy,
        ThreadEtaAccuracy::Early | ThreadEtaAccuracy::Within
    ));
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
    let unknown_read_id = app
        .send_request(
            "thread/eta/read",
            Some(serde_json::to_value(ThreadEtaReadParams {
                thread_id: unknown_thread.to_string(),
                cursor: None,
                limit: Some(1),
            })?),
        )
        .await?;
    let unknown_read_error = app
        .read_stream_until_error_message(RequestId::Integer(unknown_read_id))
        .await?;
    assert_eq!(
        unknown_read_error.error.message,
        "ETA root thread was not found"
    );

    let unknown_update_id = app
        .send_request(
            "thread/eta/update",
            Some(serde_json::to_value(ThreadEtaUpdateParams {
                thread_id: unknown_thread.to_string(),
                operations: vec![operation(
                    ThreadEtaAction::Create,
                    Some("orphan"),
                    Some("Orphan task"),
                    Some(1),
                    Some(1),
                    None,
                )],
            })?),
        )
        .await?;
    let unknown_update_error = app
        .read_stream_until_error_message(RequestId::Integer(unknown_update_id))
        .await?;
    assert_eq!(
        unknown_update_error.error.message,
        "ETA root thread was not found"
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
    let started_at = started.changed_tasks[0].started_at;
    assert!(started_at.is_some());

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
    assert_eq!(
        completed.changed_tasks[0].status,
        ThreadEtaStatus::Completed
    );
    assert_eq!(completed.changed_tasks[0].started_at, started_at);
    assert_eq!(
        completed.changed_tasks[0].terminal_at,
        Some(completed.generated_at)
    );

    let current = read(&mut app, &thread_id).await?;
    assert!(current.snapshot.active.is_empty());
    assert_eq!(current.snapshot.history.len(), 1);
    assert_terminal_history(&current.snapshot.history[0]);
    assert_eq!(current.snapshot.sequence, completed.sequence);
    let all_sessions = list(&mut app, false).await?;
    assert_eq!(all_sessions.data.len(), 1);
    assert_eq!(all_sessions.data[0].task_id, "compile");
    assert_eq!(all_sessions.data[0].session.thread_id, thread_id);

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
    assert_eq!(persisted.snapshot.history[0].started_at, started_at);
    assert_eq!(
        persisted.snapshot.history[0].terminal_at,
        completed.changed_tasks[0].terminal_at
    );
    assert_eq!(list(&mut restarted, false).await?.data.len(), 1);

    let requests = responses_server
        .received_requests()
        .await
        .expect("mock response server should expose requests");
    assert!(requests.is_empty(), "ETA RPCs must not start a model turn");

    Ok(())
}

#[tokio::test]
async fn thread_eta_list_seeds_missing_root_freshness_from_config() -> Result<()> {
    let responses_server = create_mock_responses_server_repeating_assistant("unused").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&responses_server.uri())
        .with_root_config(
            "suppress_unstable_features_warning = true\n[eta]\nfreshness_minimum_minutes = 1",
        )
        .enable_feature(Feature::Sqlite)
        .write(codex_home.path())?;

    let root_without_policy = ThreadId::new();
    let root_with_policy = ThreadId::new();
    let started_at = Utc::now() - ChronoDuration::seconds(120);
    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    for root_thread_id in [root_without_policy, root_with_policy] {
        let mut metadata = ThreadMetadataBuilder::new(
            root_thread_id,
            codex_home
                .path()
                .join("sessions")
                .join(format!("{root_thread_id}.jsonl")),
            started_at,
            SessionSource::Cli,
        );
        metadata.cwd = codex_home.path().to_path_buf();
        state_db
            .upsert_thread(&metadata.build("mock_provider"))
            .await?;
    }
    state_db
        .initialize_eta_freshness_minimum_seconds(root_with_policy, 300)
        .await?;
    state_db
        .apply_task_estimate_mutations(
            root_without_policy,
            root_without_policy,
            &[
                state_create_task("without-policy", "Without policy", 10, 20),
                state_start_task("without-policy"),
            ],
            started_at,
        )
        .await?;
    state_db
        .apply_task_estimate_mutations(
            root_with_policy,
            root_with_policy,
            &[
                state_create_task("with-policy", "With policy", 10, 20),
                state_start_task("with-policy"),
            ],
            started_at,
        )
        .await?;
    state_db.close().await;

    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let response = list(&mut app, false).await?;
    let without_policy = response
        .data
        .iter()
        .find(|task| task.root_thread_id == root_without_policy.to_string())
        .expect("root without a policy should be listed");
    assert_eq!(
        (
            without_policy.is_stale,
            without_policy.current_lower_seconds,
            without_policy.current_upper_seconds,
        ),
        (true, Some(10), Some(20))
    );
    let with_policy = response
        .data
        .iter()
        .find(|task| task.root_thread_id == root_with_policy.to_string())
        .expect("root with a policy should be listed");
    assert_eq!(
        (
            with_policy.is_stale,
            with_policy.current_lower_seconds,
            with_policy.current_upper_seconds,
        ),
        (false, Some(0), Some(0))
    );

    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    assert_eq!(
        state_db
            .eta_freshness_minimum_seconds(root_without_policy)
            .await?,
        Some(60)
    );
    assert_eq!(
        state_db
            .eta_freshness_minimum_seconds(root_with_policy)
            .await?,
        Some(300)
    );
    state_db.close().await;
    Ok(())
}

#[tokio::test]
async fn thread_eta_read_does_not_prune_terminal_history() -> Result<()> {
    let responses_server = create_mock_responses_server_repeating_assistant("unused").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&responses_server.uri())
        .with_root_config(
            "suppress_unstable_features_warning = true\n[eta]\nhistory_retention_days = 1",
        )
        .enable_feature(Feature::Sqlite)
        .write(codex_home.path())?;

    let root_thread_id = ThreadId::new();
    let old_now = Utc::now() - ChronoDuration::days(2);
    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    let mut metadata = ThreadMetadataBuilder::new(
        root_thread_id,
        codex_home.path().join("sessions").join("old-root.jsonl"),
        old_now,
        SessionSource::Cli,
    );
    metadata.cwd = codex_home.path().to_path_buf();
    state_db
        .upsert_thread(&metadata.build("mock_provider"))
        .await?;
    state_db
        .apply_task_estimate_mutations(
            root_thread_id,
            root_thread_id,
            &[
                state_create_task("old", "Old task", 1, 2),
                state_start_task("old"),
                TaskEstimateMutation {
                    action: TaskEstimateAction::Complete,
                    task_id: Some("old".to_string()),
                    title: None,
                    parent_task_id: None,
                    depends_on_task_ids: None,
                    estimate: None,
                    reason: None,
                    owner_thread_id: None,
                },
            ],
            old_now,
        )
        .await?;
    state_db.close().await;

    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    assert!(read(&mut app, &ThreadId::new().to_string()).await.is_err());
    drop(app);

    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    let snapshot = state_db
        .read_task_estimate_snapshot(root_thread_id, Utc::now(), None, None)
        .await?;
    assert_eq!(snapshot.history.len(), 1);
    assert_eq!(snapshot.history[0].task_id, "old");
    state_db.close().await;
    Ok(())
}

#[tokio::test]
async fn thread_eta_cold_root_read_uses_persisted_freshness_policy() -> Result<()> {
    let responses_server = create_mock_responses_server_repeating_assistant("unused").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&responses_server.uri())
        .with_root_config(
            "suppress_unstable_features_warning = true\n[eta]\nfreshness_minimum_minutes = 1",
        )
        .enable_feature(Feature::Sqlite)
        .write(codex_home.path())?;

    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = app.start_thread(ThreadStartParams::default()).await?;
    let thread_id = thread.id.clone();
    let root_thread_id = ThreadId::from_string(&thread_id)?;
    update(
        &mut app,
        &thread_id,
        operation(
            ThreadEtaAction::Create,
            Some("cold-root"),
            Some("Cold root task"),
            Some(10),
            Some(20),
            None,
        ),
    )
    .await?;
    let _: ThreadEtaUpdatedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_notification("thread/eta/updated"),
    )
    .await??;
    update(
        &mut app,
        &thread_id,
        operation(
            ThreadEtaAction::Start,
            Some("cold-root"),
            None,
            None,
            None,
            None,
        ),
    )
    .await?;
    let _: ThreadEtaUpdatedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_notification("thread/eta/updated"),
    )
    .await??;

    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    assert_eq!(
        state_db
            .eta_freshness_minimum_seconds(root_thread_id)
            .await?,
        Some(60)
    );
    drop(app);

    state_db
        .apply_task_estimate_mutations(
            root_thread_id,
            root_thread_id,
            &[TaskEstimateMutation {
                action: TaskEstimateAction::Revise,
                task_id: Some("cold-root".to_string()),
                title: None,
                parent_task_id: None,
                depends_on_task_ids: None,
                estimate: Some(TaskEstimateRange {
                    lower_seconds: Some(20),
                    upper_seconds: Some(30),
                }),
                reason: Some("backdated cold-root regression".to_string()),
                owner_thread_id: None,
            }],
            Utc::now() - ChronoDuration::seconds(61),
        )
        .await?;
    state_db.close().await;

    let mut cold_app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let snapshot = read(&mut cold_app, &thread_id).await?.snapshot;
    assert!(snapshot.active[0].is_stale);
    assert_eq!(
        (
            snapshot.active[0].current_lower_seconds,
            snapshot.active[0].current_upper_seconds,
        ),
        (Some(20), Some(30))
    );
    assert_eq!(
        snapshot.overall.unknown_reason.as_deref(),
        Some("stale task update")
    );
    Ok(())
}

#[tokio::test]
async fn thread_eta_first_cold_root_read_seeds_configured_freshness_policy() -> Result<()> {
    let responses_server = create_mock_responses_server_repeating_assistant("unused").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&responses_server.uri())
        .with_root_config(
            "suppress_unstable_features_warning = true\n[eta]\nfreshness_minimum_minutes = 1",
        )
        .enable_feature(Feature::Sqlite)
        .write(codex_home.path())?;

    let root_thread_id = ThreadId::new();
    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    let mut metadata = ThreadMetadataBuilder::new(
        root_thread_id,
        codex_home.path().join("sessions").join("cold-root.jsonl"),
        Utc::now(),
        SessionSource::Cli,
    );
    metadata.cwd = codex_home.path().to_path_buf();
    state_db
        .upsert_thread(&metadata.build("mock_provider"))
        .await?;
    state_db
        .apply_task_estimate_mutations(
            root_thread_id,
            root_thread_id,
            &[
                TaskEstimateMutation {
                    action: TaskEstimateAction::Create,
                    task_id: Some("cold-root".to_string()),
                    title: Some("Cold root task".to_string()),
                    parent_task_id: None,
                    depends_on_task_ids: None,
                    estimate: Some(TaskEstimateRange {
                        lower_seconds: Some(20),
                        upper_seconds: Some(30),
                    }),
                    reason: None,
                    owner_thread_id: None,
                },
                TaskEstimateMutation {
                    action: TaskEstimateAction::Start,
                    task_id: Some("cold-root".to_string()),
                    title: None,
                    parent_task_id: None,
                    depends_on_task_ids: None,
                    estimate: None,
                    reason: None,
                    owner_thread_id: None,
                },
            ],
            Utc::now() - ChronoDuration::seconds(61),
        )
        .await?;
    assert_eq!(
        state_db
            .eta_freshness_minimum_seconds(root_thread_id)
            .await?,
        None
    );
    drop(state_db);

    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let snapshot = read(&mut app, &root_thread_id.to_string()).await?.snapshot;
    assert_eq!(snapshot.active.len(), 1);
    assert!(snapshot.active[0].is_stale);
    assert_eq!(
        snapshot.overall.unknown_reason.as_deref(),
        Some("stale task update")
    );

    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    assert_eq!(
        state_db
            .eta_freshness_minimum_seconds(root_thread_id)
            .await?,
        Some(60)
    );
    Ok(())
}
