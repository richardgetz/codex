use std::time::Duration;

use futures::SinkExt;
use futures::StreamExt;
use serde_json::Value;
use serde_json::json;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::function_tool::FunctionCallError;

const CDP_CALL_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct CdpClient {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    session_id: Option<String>,
}

impl CdpClient {
    pub(crate) async fn connect(ws_url: &str) -> Result<Self, FunctionCallError> {
        let (socket, _) = connect_async(ws_url).await.map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to connect to browser websocket `{ws_url}`: {err}"
            ))
        })?;
        Ok(Self {
            socket,
            next_id: 1,
            session_id: None,
        })
    }

    pub(crate) fn has_session(&self) -> bool {
        self.session_id.is_some()
    }

    pub(crate) fn set_session_id(&mut self, session_id: String) {
        self.session_id = Some(session_id);
    }

    pub(crate) async fn call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, FunctionCallError> {
        timeout(
            CDP_CALL_TIMEOUT,
            self.call_inner(method, params, self.session_id.clone()),
        )
        .await
        .map_err(|_| {
            FunctionCallError::RespondToModel(format!(
                "browser command `{method}` timed out after {}s",
                CDP_CALL_TIMEOUT.as_secs()
            ))
        })?
    }

    pub(crate) async fn call_browser(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, FunctionCallError> {
        timeout(CDP_CALL_TIMEOUT, self.call_inner(method, params, None))
            .await
            .map_err(|_| {
                FunctionCallError::RespondToModel(format!(
                    "browser command `{method}` timed out after {}s",
                    CDP_CALL_TIMEOUT.as_secs()
                ))
            })?
    }

    async fn call_inner(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<String>,
    ) -> Result<Value, FunctionCallError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut request = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session_id) = session_id {
            request["sessionId"] = Value::String(session_id);
        }

        self.socket
            .send(Message::Text(request.to_string().into()))
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to send browser command `{method}`: {err}"
                ))
            })?;

        while let Some(message) = self.socket.next().await {
            let message = message.map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "browser websocket failed while waiting for `{method}`: {err}"
                ))
            })?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "browser returned invalid JSON for `{method}`: {err}"
                ))
            })?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(FunctionCallError::RespondToModel(format!(
                    "browser command `{method}` failed: {error}"
                )));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }

        Err(FunctionCallError::RespondToModel(format!(
            "browser websocket closed while waiting for `{method}`"
        )))
    }
}
