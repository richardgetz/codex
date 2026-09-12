use super::*;
use super::disconnect::serve_reconnect_requests;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::ThreadParamsMode;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::ThreadActivityUpdatedNotification;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::net::TcpListener;

#[tokio::test]
async fn late_resumed_worker_activity_hydrates_parent_metadata() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let root_thread_id = ThreadId::new();
    let worker_thread_id = ThreadId::new();
    app.primary_thread_id = Some(root_thread_id);
    app.team_activity
        .replace_thread_metadata(Some(root_thread_id), [(root_thread_id, None)]);
    app.team_activity.observe(&ThreadActivityUpdatedNotification {
        thread_id: root_thread_id.to_string(),
        root_thread_id: root_thread_id.to_string(),
        activity: ThreadActivity::Idle,
        pause_state: ThreadPauseState::Running,
        wait_reason: None,
        in_flight_operations: 0,
    });

    let worker_source = serde_json::to_value(SessionSource::SubAgent(
        SubAgentSource::ThreadSpawn {
            parent_thread_id: root_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        },
    ))?;
    let cwd = app.config.cwd.clone();
    let worker = json!({
        "id": worker_thread_id,
        "sessionId": root_thread_id,
        "preview": "resumed worker",
        "ephemeral": false,
        "modelProvider": "test-provider",
        "createdAt": 1,
        "updatedAt": 2,
        "status": serde_json::to_value(ThreadStatus::Active { active_flags: Vec::new() })?,
        "cwd": cwd,
        "cliVersion": "0.0.0",
        "source": worker_source,
        "turns": []
    });

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        serve_reconnect_requests(tokio_tungstenite::accept_async(stream).await?, |request| {
            let worker = worker.clone();
            std::future::ready(match request.method.as_str() {
                "thread/read" => {
                    assert_eq!(
                        request.params.as_ref().unwrap()["threadId"],
                        worker_thread_id.to_string()
                    );
                    Some(json!({"result": {"thread": worker}}))
                }
                method => panic!("unexpected hydration request: {method}"),
            })
        })
        .await
    });
    let session = AppServerSession::new(
        crate::connect_remote_app_server(endpoint).await?,
        ThreadParamsMode::Remote,
    );

    app.handle_app_server_event(
        &session,
        codex_app_server_client::AppServerEvent::ServerNotification(Box::new(
            ServerNotification::ThreadActivityUpdated(ThreadActivityUpdatedNotification {
                thread_id: worker_thread_id.to_string(),
                root_thread_id: root_thread_id.to_string(),
                activity: ThreadActivity::Working,
                pause_state: ThreadPauseState::Running,
                wait_reason: None,
                in_flight_operations: 1,
            }),
        )),
    )
    .await;

    let status = app
        .team_activity
        .status_for_root(root_thread_id, None)
        .expect("root activity should remain visible");
    assert_eq!(status.workers_working, 1);
    assert_eq!(status.direct_workers, 1);

    session.shutdown().await?;
    assert_eq!(server.await??, vec!["initialize", "thread/read"]);
    Ok(())
}
