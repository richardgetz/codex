use super::*;
use codex_app_server_protocol::JSONRPCMessage;
use futures::SinkExt;
use futures::StreamExt;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test(start_paused = true)]
async fn thread_settings_update_times_out_when_server_does_not_acknowledge() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let (request_received_tx, request_received_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut request_received_tx = Some(request_received_tx);
        let mut settings_update_requests = 0;
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            if request.method == THREAD_SETTINGS_UPDATE_METHOD {
                settings_update_requests += 1;
                request_received_tx
                    .take()
                    .expect("settings update should be sent once")
                    .send(())
                    .expect("test should still be waiting for settings update");
                continue;
            }
            let mut reply = match request.method.as_str() {
                "initialize" => {
                    json!({"result": {"userAgent": "settings-timeout-test/1.0"}})
                }
                method => panic!("unexpected request: {method}"),
            };
            reply["id"] = json!(request.id);
            socket.send(Message::Text(reply.to_string().into())).await?;
        }
        Ok::<_, color_eyre::Report>(settings_update_requests)
    });

    let mut session = AppServerSession::new(
        crate::connect_remote_app_server(endpoint).await?,
        ThreadParamsMode::Remote,
    );
    let params = ThreadSettingsUpdateParams {
        thread_id: ThreadId::new().to_string(),
        ..ThreadSettingsUpdateParams::default()
    };
    let error = {
        let request = session.thread_settings_update(params);
        tokio::pin!(request);
        tokio::select! {
            result = &mut request => panic!("settings update returned before server ack: {result:?}"),
            result = request_received_rx => result.expect("mock server should receive settings update"),
        }

        tokio::time::advance(THREAD_SETTINGS_UPDATE_ACK_TIMEOUT).await;
        request
            .await
            .expect_err("missing acknowledgment should time out")
    };
    assert!(
        format!("{error:#}")
            .contains("timed out waiting for thread/settings/update acknowledgment")
    );

    session.shutdown().await?;
    assert_eq!(server.await??, 1);
    Ok(())
}
