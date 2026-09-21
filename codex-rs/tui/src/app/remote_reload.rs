//! Remote managed `/reload` forwarding and durable status reconciliation.

use super::App;
use super::AppServerSession;
use super::reconnect::PendingRemoteReload;
use crate::app_event::AppEvent;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SlashCommandExecuteResponse;
use codex_app_server_protocol::SlashCommandReloadResult;
use codex_app_server_protocol::SlashCommandResultNotification;
use codex_protocol::ThreadId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteReloadState {
    Accepted,
    InProgress,
    Completed,
    Failed,
    Unavailable,
    Unknown,
}

impl RemoteReloadState {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Unavailable | Self::Unknown
        )
    }
}

fn remote_reload_state(
    reload: Option<&SlashCommandReloadResult>,
    ok: bool,
) -> RemoteReloadState {
    match reload.map(|result| result.state.as_str()) {
        Some("accepted") => RemoteReloadState::Accepted,
        Some("in_progress") => RemoteReloadState::InProgress,
        Some("completed") => RemoteReloadState::Completed,
        Some("failed") => RemoteReloadState::Failed,
        Some("unavailable") => RemoteReloadState::Unavailable,
        _ if !ok => RemoteReloadState::Unknown,
        _ => RemoteReloadState::InProgress,
    }
}

impl App {
    pub(super) async fn execute_remote_reload(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        args: String,
    ) -> color_eyre::Result<()> {
        if !matches!(self.app_server_target, crate::AppServerTarget::Remote { .. }) {
            self.chat_widget.add_error_message(
                "`/reload` forwarding requires a remote app-server connection.".to_string(),
            );
            return Ok(());
        }

        let is_status = args.trim().eq_ignore_ascii_case("status");
        if !is_status && self.reconnect.pending_remote_reload.is_some() {
            self.chat_widget.add_info_message(
                "A managed remote reload is already in progress; no second handoff was started."
                    .to_string(),
                Some("Use `/reload status` to inspect the durable handoff state.".to_string()),
            );
            return Ok(());
        }

        let request_id = app_server.next_request_id();
        let tracked_thread_id = self
            .reconnect
            .pending_remote_reload
            .as_ref()
            .map_or(thread_id, |pending| pending.thread_id);
        match (is_status, self.reconnect.pending_remote_reload.is_some()) {
            (true, true) => {
                self.reconnect.pending_remote_reload = Some(PendingRemoteReload {
                    thread_id: tracked_thread_id,
                    request_id: request_id.clone(),
                });
            }
            (false, _) => {
                self.reconnect.pending_remote_reload = Some(PendingRemoteReload {
                    thread_id,
                    request_id: request_id.clone(),
                });
            }
            (true, false) => {}
        }

        let result = app_server
            .slash_command_execute(
                request_id.clone(),
                tracked_thread_id,
                "reload".to_string(),
                args,
            )
            .await;
        match result {
            Ok(response) => {
                self.apply_remote_reload_response(
                    tracked_thread_id,
                    request_id,
                    response,
                    /*allow_new_pending*/ true,
                    /*display_progress*/ true,
                );
                Ok(())
            }
            Err(error) if self.recover_transport_error(&error) => Ok(()),
            Err(error) => {
                self.clear_remote_reload(request_id, tracked_thread_id);
                self.chat_widget
                    .add_error_message(format!("Remote `/reload` failed: {error:#}"));
                Ok(())
            }
        }
    }

    pub(super) async fn execute_remote_reload_status(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> color_eyre::Result<()> {
        self.execute_remote_reload(app_server, thread_id, "status".to_string())
            .await
    }

    pub(super) fn handle_remote_reload_notification(
        &mut self,
        notification: &SlashCommandResultNotification,
    ) {
        let Ok(thread_id) = ThreadId::from_string(&notification.thread_id) else {
            return;
        };
        let Some(pending) = self.reconnect.pending_remote_reload.as_ref() else {
            return;
        };
        if pending.thread_id != thread_id || pending.request_id != notification.request_id {
            return;
        }
        self.apply_remote_reload_payload(
            thread_id,
            notification.request_id.clone(),
            notification.result.ok,
            &notification.result.output.text,
            notification.result.reload.as_ref(),
            /*allow_new_pending*/ false,
            /*display_progress*/ false,
        );
    }

    pub(super) fn queue_remote_reload_status_after_reconnect(&self) {
        let Some(pending) = self.reconnect.pending_remote_reload.as_ref() else {
            return;
        };
        self.app_event_tx.send(AppEvent::RemoteReloadStatusRequested {
            thread_id: pending.thread_id,
        });
    }

    fn apply_remote_reload_response(
        &mut self,
        thread_id: ThreadId,
        request_id: RequestId,
        response: SlashCommandExecuteResponse,
        allow_new_pending: bool,
        display_progress: bool,
    ) {
        self.apply_remote_reload_payload(
            thread_id,
            request_id,
            response.ok,
            &response.output.text,
            response.reload.as_ref(),
            allow_new_pending,
            display_progress,
        );
    }

    fn apply_remote_reload_payload(
        &mut self,
        thread_id: ThreadId,
        request_id: RequestId,
        ok: bool,
        output: &str,
        reload: Option<&SlashCommandReloadResult>,
        allow_new_pending: bool,
        display_progress: bool,
    ) {
        let state = remote_reload_state(reload, ok);
        let terminal = state.is_terminal();
        let matches_pending = self
            .reconnect
            .pending_remote_reload
            .as_ref()
            .is_some_and(|pending| {
                pending.thread_id == thread_id && pending.request_id == request_id
            });

        if matches!(state, RemoteReloadState::Accepted | RemoteReloadState::InProgress)
            && allow_new_pending
            && self.reconnect.pending_remote_reload.is_none()
        {
            self.reconnect.pending_remote_reload = Some(PendingRemoteReload {
                thread_id,
                request_id: request_id.clone(),
            });
        }

        if terminal && matches_pending {
            self.reconnect.pending_remote_reload = None;
        }

        let detail = reload
            .and_then(|result| result.reason.as_deref())
            .filter(|reason| !reason.is_empty());
        let message = if output.trim().is_empty() {
            detail.unwrap_or("The remote app-server returned no reload details.")
        } else {
            output.trim()
        };
        match state {
            RemoteReloadState::Accepted | RemoteReloadState::InProgress if display_progress => {
                self.chat_widget.add_info_message(
                    message.to_string(),
                    Some(
                        "The remote app-server owns pause, replacement, and exact-turn recovery; use `/reload status` after reconnecting."
                            .to_string(),
                    ),
                );
            }
            RemoteReloadState::Completed => {
                self.chat_widget.add_info_message(message.to_string(), None);
            }
            RemoteReloadState::Failed | RemoteReloadState::Unavailable | RemoteReloadState::Unknown => {
                self.chat_widget.add_error_message(message.to_string());
            }
        }
    }

    fn clear_remote_reload(&mut self, request_id: RequestId, thread_id: ThreadId) {
        if self
            .reconnect
            .pending_remote_reload
            .as_ref()
            .is_some_and(|pending| {
                pending.request_id == request_id && pending.thread_id == thread_id
            })
        {
            self.reconnect.pending_remote_reload = None;
        }
    }
}

#[cfg(test)]
#[path = "remote_reload_tests.rs"]
mod tests;
