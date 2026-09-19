//! Drives automatic reconnect through the real binary and terminal event loop.

use super::focus_palette::PtyCodex;
use super::focus_palette::write_test_config;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::JSONRPCMessage;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use std::time::Instant;
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::Message;

const RECONNECT_MODEL: &str = "gpt-5.6-terra";
const ACCOUNT_SWITCH_RESPONSE: &str = "{}";
const ACCOUNT_READ_RESPONSE: &str = r#"{"account":{"type":"apiKey"},"requiresOpenaiAuth":false}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_reconnect_restores_draft_and_routes_new_notifications() -> Result<()> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    // macOS's default temporary directory leaves too little room for the control socket path.
    let codex_home = tempfile::tempdir_in("/tmp")?;
    write_test_config(codex_home.path(), &repo_root)?;
    let config_provenance = std::fs::read_to_string(codex_home.path().join("config.toml"))?;
    let socket = codex_app_server_client::app_server_control_socket_path(codex_home.path())?;
    std::fs::create_dir_all(socket.parent().unwrap())?;
    let listener = UnixListener::bind(socket.as_path())?;
    let (disconnect_tx, mut disconnect_rx) = tokio::sync::oneshot::channel();
    let (restore_tx, restore_rx) = tokio::sync::oneshot::channel();
    let server_cwd = repo_root.clone();
    let server = tokio::spawn(async move {
        let mut methods = Vec::new();
        let mut restore_rx = Some(restore_rx);
        let id = "00000000-0000-0000-0000-000000000001";
        let thread = json!({
            "id": id, "sessionId": id, "preview": "", "ephemeral": false,
            "modelProvider": "openai", "createdAt": 1, "updatedAt": 2,
            "status": {"type": "active", "activeFlags": []}, "cwd": server_cwd,
            "cliVersion": "0.0.0", "source": "cli", "turns": [{"id": "running", "items": [], "status": "inProgress", "error": null}]
        });
        for connection in 0..3 {
            let mut socket = loop {
                let (stream, _) = listener.accept().await?;
                // Startup probes the default daemon socket before opening its WebSocket.
                if let Ok(socket) = tokio_tungstenite::accept_async(stream).await {
                    break socket;
                }
            };
            loop {
                let frame = tokio::select! {
                    _ = &mut disconnect_rx, if connection == 0 => {
                        socket.close(/*msg*/ None).await?;
                        break;
                    }
                    frame = socket.next() => frame,
                };
                let Some(Ok(Message::Text(text))) = frame else {
                    break;
                };
                let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                    continue;
                };
                methods.push(request.method.clone());
                if connection == 1 && request.method == "initialize" {
                    restore_rx.take().unwrap().await?;
                }
                if connection == 1 && request.method == "thread/resume" {
                    socket
                        .send(Message::Text(
                            json!({"id": request.id, "error": {
                                "code": -32600,
                                "message": format!("thread {id} is closing; retry after the thread is closed")
                            }})
                            .to_string()
                            .into(),
                        ))
                        .await?;
                    continue;
                }
                let result = match request.method.as_str() {
                    "initialize" => json!({"userAgent": "reconnect-pty"}),
                    "account/switch" => json!({}),
                    "account/read" => {
                        json!({"account": {"type": "apiKey"}, "requiresOpenaiAuth": false})
                    }
                    "model/list" => json!({"data": [], "nextCursor": null}),
                    "configRequirements/read" => json!({"requirements": null}),
                    "thread/start" | "thread/resume" => {
                        json!({"thread": thread, "model": RECONNECT_MODEL, "modelProvider": "openai",
                            "cwd": server_cwd, "approvalPolicy": "never", "approvalsReviewer": "user",
                            "sandbox": {"type": "readOnly"}, "reasoningEffort": null})
                    }
                    "thread/read" => json!({"thread": thread}),
                    "thread/goal/get" => json!({"goal": null}),
                    "skills/list" => json!({"data": []}),
                    _ => {
                        socket
                            .send(Message::Text(
                                json!({"id": request.id, "error": {
                                    "code": -32601, "message": "method not found"
                                }})
                                .to_string()
                                .into(),
                            ))
                            .await?;
                        continue;
                    }
                };
                socket
                    .send(Message::Text(
                        json!({"id": request.id, "result": result})
                            .to_string()
                            .into(),
                    ))
                    .await?;
                if connection == 2 && request.method == "thread/resume" {
                    // Keep the recovered turn running: its output must appear without waiting
                    // for turn/completed or rebuilding the transcript.
                    socket
                        .send(Message::Text(
                            json!({
                                "method": "item/agentMessage/delta", "params": {
                                    "threadId": id, "turnId": "running", "itemId": "live-item",
                                    "delta": "fresh-notification-after-reconnect\n"
                                }
                            })
                            .to_string()
                            .into(),
                        ))
                        .await?;
                }
            }
        }
        Ok::<_, anyhow::Error>(methods)
    });
    let mut terminal = PtyCodex::start(&repo_root, codex_home, &[])?;
    terminal.wait_for_startup()?;
    let mut disconnect_tx = Some(disconnect_tx);
    let mut restore_tx = Some(restore_tx);
    for expected in [
        RECONNECT_MODEL,
        "preserved-draft",
        "Reconnecting",
        "preserved-draft!",
        "fresh-notification-after-reconnect",
    ] {
        let deadline = Instant::now() + Duration::from_secs(/*secs*/ 30);
        while !terminal.screen_contains(expected) && Instant::now() < deadline {
            terminal.read_output(Duration::from_millis(/*millis*/ 20))?;
        }
        ensure!(
            terminal.screen_contains(expected),
            "missing {expected}; config fixture={config_provenance:?}; model fixture={RECONNECT_MODEL:?}; account/switch response={ACCOUNT_SWITCH_RESPONSE}; account/read response={ACCOUNT_READ_RESPONSE}; screen:\n{}",
            terminal.screen_contents()
        );
        match expected {
            RECONNECT_MODEL => terminal.write_input(b"preserved-draft")?,
            "preserved-draft" => {
                disconnect_tx.take().unwrap().send(()).unwrap();
            }
            "Reconnecting" => terminal.write_input(b"!")?,
            "preserved-draft!" => {
                restore_tx.take().unwrap().send(()).unwrap();
            }
            _ => {}
        }
    }
    ensure!(
        terminal.screen_contains("preserved-draft!"),
        "draft was lost after recovery"
    );
    drop(terminal);
    let methods = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), server).await???;
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "thread/resume")
            .count(),
        2
    );
    assert!(!methods.iter().any(|method| method == "turn/start"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn implicit_daemon_resume_picker_and_direct_id_share_one_connection() -> Result<()> {
    for (extra_args, expected_lookup) in [
        (vec!["resume"], "thread/list"),
        (
            vec!["resume", "00000000-0000-0000-0000-000000000001"],
            "thread/read",
        ),
    ] {
        let repo_root = codex_utils_cargo_bin::repo_root()?;
        let codex_home = tempfile::tempdir_in("/tmp")?;
        write_test_config(codex_home.path(), &repo_root)?;
        let socket_path = codex_app_server_client::app_server_control_socket_path(codex_home.path())?;
        std::fs::create_dir_all(socket_path.as_path().parent().unwrap())?;
        let listener = UnixListener::bind(socket_path.as_path())?;
        let server_cwd = repo_root.clone();
        let thread = json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "sessionId": "00000000-0000-0000-0000-000000000001",
            "preview": "resume fixture",
            "ephemeral": false,
            "modelProvider": "openai",
            "createdAt": 1,
            "updatedAt": 2,
            "status": {"type": "idle", "activeFlags": []},
            "cwd": server_cwd.clone(),
            "cliVersion": "0.0.0",
            "source": "cli",
            "turns": []
        });
        let (lookup_tx, lookup_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut accepted = 0;
            let mut lookup_tx = Some(lookup_tx);
            let mut socket = loop {
                let (stream, _) = listener.accept().await?;
                accepted += 1;
                match tokio_tungstenite::accept_async(stream).await {
                    Ok(socket) => break socket,
                    Err(_) => continue,
                }
            };
            let mut methods = Vec::new();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                    continue;
                };
                let method = request.method.as_str();
                methods.push(method.to_string());
                let result = match method {
                    "initialize" => json!({"userAgent": "implicit-resume-test"}),
                    "account/read" => {
                        json!({"account": {"type": "apiKey"}, "requiresOpenaiAuth": false})
                    }
                    "account/switch" => json!({}),
                    "model/list" => json!({"data": [], "nextCursor": null}),
                    "configRequirements/read" => json!({"requirements": null}),
                    "skills/list" => json!({"data": []}),
                    "thread/goal/get" => json!({"goal": null}),
                    "thread/loaded/list" => json!({"data": [], "nextCursor": null}),
                    "thread/list" => json!({"data": [thread.clone()], "nextCursor": null}),
                    "thread/read" => json!({"thread": thread.clone()}),
                    "thread/resume" => json!({
                        "thread": thread.clone(),
                        "model": "gpt-5.6-terra",
                        "modelProvider": "openai",
                        "serviceTier": null,
                        "cwd": server_cwd.clone(),
                        "approvalPolicy": "never",
                        "approvalsReviewer": "user",
                        "sandbox": {"type": "readOnly"},
                        "reasoningEffort": null
                    }),
                    _ => json!({}),
                };
                socket
                    .send(Message::Text(
                        json!({"id": request.id, "result": result})
                            .to_string()
                            .into(),
                    ))
                    .await?;
                if method == "thread/resume"
                    && let Some(lookup_tx) = lookup_tx.take()
                {
                    lookup_tx.send(()).ok();
                }
            }
            Ok::<_, anyhow::Error>((accepted, methods))
        });

        let mut terminal = PtyCodex::start(&repo_root, codex_home, &extra_args)?;
        terminal.wait_for_startup()?;
        if expected_lookup == "thread/list" {
            terminal.wait_for_screen("resume fixture")?;
            terminal.write_input(b"\r")?;
        }
        tokio::time::timeout(Duration::from_secs(30), lookup_rx).await??;
        drop(terminal);

        let (accepted, methods) = tokio::time::timeout(Duration::from_secs(5), server).await???;
        assert_eq!(accepted, 1, "resume mode {extra_args:?} opened a second daemon connection");
        assert!(methods.iter().any(|method| method == expected_lookup));
        assert!(methods.iter().any(|method| method == "thread/resume"));
    }
    Ok(())
}
