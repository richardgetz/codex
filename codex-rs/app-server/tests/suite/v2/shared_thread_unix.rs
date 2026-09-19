use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use app_test_support::DISABLE_PLUGIN_STARTUP_TASKS_ARG;
use app_test_support::MockResponsesConfig;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use futures::SinkExt;
use futures::StreamExt;
use serde_json::Value;
use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use tempfile::TempDir;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message;

type UnixWebSocket = WebSocketStream<UnixStream>;

#[cfg(target_os = "macos")]
const READ_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(not(target_os = "macos"))]
const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn two_unix_clients_share_one_thread_and_active_turn() -> Result<()> {
    let deadline = PhaseDeadline::new();
    let (release_response, response_gate) = tokio::sync::oneshot::channel();
    let (release_steer, steer_gate) = tokio::sync::oneshot::channel();
    let (release_follow_up, follow_up_gate) = tokio::sync::oneshot::channel();
    let (responses_server, _) = start_streaming_sse_server(vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("shared-1")]),
            },
            StreamingSseChunk {
                gate: Some(response_gate),
                body: responses::sse(vec![responses::ev_completed("shared-1")]),
            },
        ],
        vec![StreamingSseChunk {
            gate: Some(steer_gate),
            body: responses::sse(vec![
                responses::ev_response_created("shared-2"),
                responses::ev_completed("shared-2"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: Some(follow_up_gate),
            body: responses::sse(vec![
                responses::ev_response_created("shared-3"),
                responses::ev_completed("shared-3"),
            ]),
        }],
    ])
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(responses_server.uri()).write(codex_home.path())?;
    let socket_path = codex_home
        .path()
        .join("app-server-control")
        .join("shared-thread.sock");
    std::fs::create_dir_all(socket_path.parent().context("socket parent")?)?;
    let mut process = deadline
        .run(
            "spawn Unix app-server",
            spawn_unix_server(codex_home.path(), &socket_path),
        )
        .await?;

    let mut client_a = deadline
        .run("connect Unix client A", connect_unix(&socket_path))
        .await?;
    let mut client_b = deadline
        .run("connect Unix client B", connect_unix(&socket_path))
        .await?;
    deadline
        .run(
            "initialize Unix client A",
            initialize(&mut client_a, 1, "shared-client-a"),
        )
        .await?;
    deadline
        .run(
            "initialize Unix client B",
            initialize(&mut client_b, 2, "shared-client-b"),
        )
        .await?;

    let thread_id = deadline
        .run("client A thread/start", start_thread(&mut client_a, 3))
        .await?;
    let active_turn = deadline
        .run(
            "client A turn/start",
            start_turn(&mut client_a, 4, &thread_id, "start"),
        )
        .await?;
    deadline
        .run("waiting for the first model request", async {
            responses_server.wait_for_request_count(1).await;
            Ok(())
        })
        .await?;

    let read = deadline
        .run(
            "client B thread/read while active",
            read_thread(&mut client_b, 5, &thread_id, false),
        )
        .await?;
    assert_eq!(read.thread.id, thread_id);
    assert!(matches!(read.thread.status, ThreadStatus::Active { .. }));

    let resumed = deadline
        .run("client B thread/resume to subscribe while active", async {
            send_request(
                &mut client_b,
                "thread/resume",
                6,
                Some(serde_json::to_value(ThreadResumeParams {
                    thread_id: thread_id.clone(),
                    exclude_turns: true,
                    ..Default::default()
                })?),
            )
            .await?;
            let response = read_response_for_id(&mut client_b, 6).await?;
            Ok(serde_json::from_value::<ThreadResumeResponse>(
                response.result,
            )?)
        })
        .await?;
    assert_eq!(resumed.thread.id, thread_id);
    assert!(matches!(resumed.thread.status, ThreadStatus::Active { .. }));

    let steered_turn = deadline
        .run(
            "client B turn/start steering active turn",
            start_turn(&mut client_b, 7, &thread_id, "steer"),
        )
        .await?;
    assert_eq!(steered_turn.id, active_turn.id);

    release_response
        .send(())
        .expect("the first model response should still be gated");
    release_steer
        .send(())
        .expect("the steered model response should still be gated");
    deadline
        .run(
            "client B turn/completed after steering",
            wait_for_turn_notification(
                &mut client_b,
                "turn/completed",
                &thread_id,
                &active_turn.id,
            ),
        )
        .await?;

    let follow_up_turn = deadline
        .run(
            "client B follow-up turn/start",
            start_turn(&mut client_b, 8, &thread_id, "follow up"),
        )
        .await?;
    assert_ne!(follow_up_turn.id, active_turn.id);
    release_follow_up
        .send(())
        .expect("the follow-up model response should still be gated");
    deadline
        .run(
            "client B follow-up turn/completed",
            wait_for_turn_notification(
                &mut client_b,
                "turn/completed",
                &thread_id,
                &follow_up_turn.id,
            ),
        )
        .await?;

    let final_read = deadline
        .run(
            "client B final thread/read",
            read_thread(&mut client_b, 9, &thread_id, true),
        )
        .await?;
    assert_eq!(final_read.thread.id, thread_id);
    assert!(
        final_read
            .thread
            .turns
            .iter()
            .any(|turn| turn.id == active_turn.id)
    );
    assert!(
        final_read
            .thread
            .turns
            .iter()
            .any(|turn| turn.id == follow_up_turn.id)
    );
    assert_eq!(responses_server.requests().await.len(), 3);

    client_a.close(None).await.context("close client A")?;
    client_b.close(None).await.context("close client B")?;
    process.kill().await.context("stop Unix app-server")?;
    responses_server.shutdown().await;
    Ok(())
}

async fn spawn_unix_server(codex_home: &Path, socket_path: &Path) -> Result<Child> {
    let program = codex_utils_cargo_bin::cargo_bin("codex-app-server")
        .context("should find app-server binary")?;
    let mut process = Command::new(program);
    process
        .arg("--listen")
        .arg(format!("unix://{}", socket_path.display()))
        .arg(DISABLE_PLUGIN_STARTUP_TASKS_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("CODEX_HOME", codex_home)
        .env("HOME", codex_home)
        .env(
            "CODEX_APP_SERVER_MANAGED_CONFIG_PATH",
            codex_home.join("managed_config.toml"),
        )
        .env("RUST_LOG", "warn");
    let mut process = process
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn Unix app-server")?;

    let stderr = process
        .stderr
        .take()
        .context("failed to capture Unix app-server stderr")?;
    tokio::spawn(async move {
        let mut stderr_reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = stderr_reader.next_line().await {
            eprintln!("[Unix app-server stderr] {line}");
        }
    });

    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        if socket_path.exists() {
            return Ok(process);
        }
        if let Some(status) = process.try_wait()? {
            bail!("Unix app-server exited before binding socket: {status}");
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for Unix app-server socket");
        }
        sleep(Duration::from_millis(25)).await;
    }
}

#[derive(Clone, Copy)]
struct PhaseDeadline {
    deadline: Instant,
}

impl PhaseDeadline {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + READ_TIMEOUT.saturating_sub(Duration::from_secs(5)),
        }
    }

    async fn run<T, F>(&self, phase: &str, future: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out before {phase}");
        }
        timeout(remaining, future)
            .await
            .with_context(|| format!("timed out during {phase}"))?
            .with_context(|| format!("phase {phase} failed"))
    }
}

async fn connect_unix(socket_path: &Path) -> Result<UnixWebSocket> {
    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => match client_async("ws://localhost/rpc", stream).await {
                Ok((websocket, _response)) => return Ok(websocket),
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error).context("Unix websocket handshake failed"),
            },
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error).context("Unix app-server connection failed"),
        }
    }
}

async fn initialize(client: &mut UnixWebSocket, id: i64, name: &str) -> Result<()> {
    send_request(
        client,
        "initialize",
        id,
        Some(serde_json::to_value(InitializeParams {
            client_info: ClientInfo {
                name: name.to_string(),
                title: Some("shared thread Unix test".to_string()),
                version: "0.1.0".to_string(),
            },
            capabilities: None,
        })?),
    )
    .await?;
    read_response_for_id(client, id).await?;
    Ok(())
}

async fn start_thread(client: &mut UnixWebSocket, id: i64) -> Result<String> {
    send_request(
        client,
        "thread/start",
        id,
        Some(serde_json::to_value(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })?),
    )
    .await?;
    let response = read_response_for_id(client, id).await?;
    Ok(
        serde_json::from_value::<ThreadStartResponse>(response.result)?
            .thread
            .id,
    )
}

async fn start_turn(
    client: &mut UnixWebSocket,
    id: i64,
    thread_id: &str,
    text: &str,
) -> Result<codex_app_server_protocol::Turn> {
    send_request(
        client,
        "turn/start",
        id,
        Some(serde_json::to_value(TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })?),
    )
    .await?;
    let response = read_response_for_id(client, id).await?;
    Ok(serde_json::from_value::<TurnStartResponse>(response.result)?.turn)
}

async fn read_thread(
    client: &mut UnixWebSocket,
    id: i64,
    thread_id: &str,
    include_turns: bool,
) -> Result<ThreadReadResponse> {
    send_request(
        client,
        "thread/read",
        id,
        Some(serde_json::to_value(ThreadReadParams {
            thread_id: thread_id.to_string(),
            include_turns,
        })?),
    )
    .await?;
    let response = read_response_for_id(client, id).await?;
    Ok(serde_json::from_value(response.result)?)
}

async fn send_request(
    client: &mut UnixWebSocket,
    method: &str,
    id: i64,
    params: Option<Value>,
) -> Result<()> {
    let message = JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(id),
        method: method.to_string(),
        params,
        trace: None,
    });
    client
        .send(Message::Text(serde_json::to_string(&message)?.into()))
        .await
        .context("send Unix JSON-RPC request")?;
    Ok(())
}

async fn read_response_for_id(client: &mut UnixWebSocket, id: i64) -> Result<JSONRPCResponse> {
    let target_id = RequestId::Integer(id);
    loop {
        match read_jsonrpc_message(client).await? {
            JSONRPCMessage::Response(response) if response.id == target_id => return Ok(response),
            JSONRPCMessage::Error(error) if error.id == target_id => {
                bail!("request {id} failed: {}", error.error.message)
            }
            _ => {}
        }
    }
}

async fn wait_for_turn_notification(
    client: &mut UnixWebSocket,
    method: &str,
    thread_id: &str,
    turn_id: &str,
) -> Result<()> {
    loop {
        let JSONRPCMessage::Notification(JSONRPCNotification {
            method: candidate_method,
            params: Some(params),
        }) = read_jsonrpc_message(client).await?
        else {
            continue;
        };
        if candidate_method != method
            || params.get("threadId").and_then(Value::as_str) != Some(thread_id)
            || params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                != Some(turn_id)
        {
            continue;
        }
        return Ok(());
    }
}

async fn read_jsonrpc_message(client: &mut UnixWebSocket) -> Result<JSONRPCMessage> {
    loop {
        let frame = timeout(READ_TIMEOUT, client.next())
            .await
            .context("timed out reading Unix websocket")?
            .context("Unix websocket closed")?
            .context("Unix websocket read failed")?;
        match frame {
            Message::Text(text) => return Ok(serde_json::from_str(text.as_ref())?),
            Message::Binary(bytes) => return Ok(serde_json::from_slice(&bytes)?),
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(close) => bail!("Unix websocket closed: {close:?}"),
            Message::Frame(_) => continue,
        }
    }
}
