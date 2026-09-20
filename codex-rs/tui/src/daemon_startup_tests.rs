//! Implicit daemon discovery is opportunistic; explicit endpoints remain authoritative.

use super::*;
use crate::legacy_core::config::ConfigBuilder;
use codex_app_server_protocol::JSONRPCMessage;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

#[cfg(windows)]
#[tokio::test]
async fn daemon_connection_rejects_unprotected_socket_before_handshake() -> color_eyre::Result<()> {
    let home = TempDir::new()?;
    let parent = home.path().join("control");
    std::fs::create_dir(&parent)?;
    let socket_path = AbsolutePathBuf::from_absolute_path_checked(parent.join("server.sock"))?;
    let mut listener = codex_uds::UnixListener::bind(socket_path.as_path()).await?;
    let target = AppServerTarget::LocalDaemon {
        endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
    };
    tokio::select! {
        result = app_server_connection::connect(&target) => assert!(result.is_err()),
        _ = listener.accept() => panic!("unprotected listener must not receive a connection"),
    }
    Ok(())
}

#[tokio::test]
async fn daemon_startup_reports_connection_failures_without_embedded_fallback()
-> color_eyre::Result<()> {
    for scenario in ["missing socket", "failed handshake", "explicit endpoint"] {
        let home = TempDir::new()?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = if scenario == "missing socket" {
            RemoteAppServerEndpoint::UnixSocket {
                socket_path: AbsolutePathBuf::from_absolute_path_checked(
                    home.path().join("gone.sock"),
                )?,
            }
        } else {
            RemoteAppServerEndpoint::WebSocket {
                websocket_url: format!("ws://{}", listener.local_addr()?),
                auth_token: None,
            }
        };
        let reject_handshake = tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let mut target = if scenario == "explicit endpoint" {
            AppServerTarget::Remote { endpoint }
        } else {
            AppServerTarget::LocalDaemon { endpoint }
        };
        let original_target = target.clone();
        let mut state_db = None;
        let result = start_app_server(
            &mut target,
            Arg0DispatchPaths::default(),
            config,
            Vec::new(),
            LoaderOverrides::default(),
            /*strict_config*/ false,
            CloudConfigBundleLoader::default(),
            codex_feedback::CodexFeedback::new(),
            /*log_db*/ None,
            &mut state_db,
            Arc::new(EnvironmentManager::default_for_tests()),
        )
        .await;
        reject_handshake.abort();
        assert!(result.is_err());
        assert_eq!(target, original_target);
        assert!(state_db.is_none());
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn default_daemon_discovery_distinguishes_absent_and_stale_sockets() -> color_eyre::Result<()>
{
    let home = TempDir::new()?;
    assert!(connect_default_daemon(home.path()).await?.is_none());

    let socket_path = codex_app_server_client::app_server_control_socket_path(home.path())?;
    std::fs::create_dir_all(socket_path.as_path().parent().unwrap())?;
    let listener = codex_uds::UnixListener::bind(socket_path.as_path()).await?;
    drop(listener);

    let error = match connect_default_daemon(home.path()).await {
        Ok(_) => color_eyre::eyre::bail!("an existing but unavailable socket must fail closed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("refusing to start a competing embedded server")
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn default_daemon_startup_reuses_authoritative_handshake_for_resume() -> color_eyre::Result<()>
{
    let home = TempDir::new()?;
    let socket_path = codex_app_server_client::app_server_control_socket_path(home.path())?;
    std::fs::create_dir_all(socket_path.as_path().parent().unwrap())?;
    let mut listener = codex_uds::UnixListener::bind(socket_path.as_path()).await?;
    let server = tokio::spawn(async move {
        let stream = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut methods = Vec::new();
        while let Some(message) = socket.next().await {
            let Message::Text(text) = message? else {
                continue;
            };
            match serde_json::from_str::<JSONRPCMessage>(&text)? {
                JSONRPCMessage::Request(request) => {
                    methods.push(request.method.clone());
                    assert_eq!(request.method, "initialize");
                    socket
                        .send(Message::Text(
                            json!({
                                "id": request.id,
                                "result": {"userAgent": "implicit-resume-test"}
                            })
                            .to_string()
                            .into(),
                        ))
                        .await?;
                }
                JSONRPCMessage::Notification(notification)
                    if notification.method == "initialized" =>
                {
                    break;
                }
                JSONRPCMessage::Notification(_)
                | JSONRPCMessage::Response(_)
                | JSONRPCMessage::Error(_) => {}
            }
        }
        Ok::<_, color_eyre::Report>(methods)
    });

    let prepared = connect_default_daemon(home.path())
        .await?
        .expect("the bound daemon socket should be discovered");
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await?;
    let mut target = AppServerTarget::LocalDaemon {
        endpoint: RemoteAppServerEndpoint::UnixSocket {
            socket_path: prepared.socket_path.clone(),
        },
    };
    let mut state_db = None;
    let app_server = start_app_server_with_preconnected(
        &mut target,
        Arg0DispatchPaths::default(),
        config,
        Vec::new(),
        LoaderOverrides::default(),
        /*strict_config*/ false,
        CloudConfigBundleLoader::default(),
        codex_feedback::CodexFeedback::new(),
        /*log_db*/ None,
        &mut state_db,
        Arc::new(EnvironmentManager::default_for_tests()),
        Some(prepared.app_server),
    )
    .await?;
    drop(app_server);

    let methods = tokio::time::timeout(Duration::from_secs(5), server).await???;
    assert_eq!(methods, vec!["initialize".to_string()]);
    Ok(())
}
