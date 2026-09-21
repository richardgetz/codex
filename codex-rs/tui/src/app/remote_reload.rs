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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteReloadTerminalAction {
    Ignore,
    KeepPending,
    ClearPending,
    Completed,
    FrontendRefresh,
}

fn remote_reload_terminal_action(
    state: RemoteReloadState,
    operation_request_match: bool,
    status_request_match: bool,
    allow_frontend_refresh: bool,
) -> RemoteReloadTerminalAction {
    if !state.is_terminal() || !(operation_request_match || status_request_match) {
        return RemoteReloadTerminalAction::Ignore;
    }
    if status_request_match && matches!(state, RemoteReloadState::Unknown) {
        return RemoteReloadTerminalAction::KeepPending;
    }
    if matches!(state, RemoteReloadState::Completed) {
        return RemoteReloadTerminalAction::Completed;
    }
    if matches!(state, RemoteReloadState::Unavailable) && allow_frontend_refresh {
        return RemoteReloadTerminalAction::FrontendRefresh;
    }
    RemoteReloadTerminalAction::ClearPending
}

fn remote_reload_state(reload: Option<&SlashCommandReloadResult>, ok: bool) -> RemoteReloadState {
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

fn suppress_duplicate_reload(is_status: bool, pending: bool) -> bool {
    pending && !is_status
}

fn pending_reload_request_matches(pending: &PendingRemoteReload, request_id: &RequestId) -> bool {
    &pending.request_id == request_id || pending.status_request_id.as_ref() == Some(request_id)
}

impl App {
    pub(super) async fn execute_remote_reload(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        args: String,
    ) -> color_eyre::Result<()> {
        if matches!(self.app_server_target, crate::AppServerTarget::Embedded) {
            self.chat_widget.add_error_message(
                "`/reload` forwarding requires a persistent app-server connection.".to_string(),
            );
            return Ok(());
        }

        let is_status = args.trim().eq_ignore_ascii_case("status");
        let allows_frontend_refresh = args.trim().is_empty();
        if suppress_duplicate_reload(is_status, self.reconnect.pending_remote_reload.is_some()) {
            self.chat_widget.add_info_message(
                "A managed remote reload is already in progress; no second handoff was started."
                    .to_string(),
                Some("Use `/reload status` to inspect the durable handoff state.".to_string()),
            );
            return Ok(());
        }

        let request_id = app_server.next_request_id();
        self.reconnect.last_remote_reload_terminal = None;
        let tracked_thread_id = self
            .reconnect
            .pending_remote_reload
            .as_ref()
            .map_or(thread_id, |pending| pending.thread_id);
        match (is_status, self.reconnect.pending_remote_reload.is_some()) {
            (true, true) => {
                if let Some(pending) = self.reconnect.pending_remote_reload.as_mut() {
                    pending.status_request_id = Some(request_id.clone());
                }
            }
            (false, _) => {
                self.reconnect.pending_remote_reload = Some(PendingRemoteReload {
                    thread_id,
                    request_id: request_id.clone(),
                    status_request_id: None,
                    allow_frontend_refresh: allows_frontend_refresh,
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
                    is_status,
                );
                Ok(())
            }
            Err(error) if self.recover_transport_error(&error) => Ok(()),
            Err(error) => {
                let persistent_target = matches!(
                    self.app_server_target,
                    crate::AppServerTarget::LocalDaemon { .. }
                        | crate::AppServerTarget::Remote { .. }
                );
                let method_unsupported = matches!(
                    error.downcast_ref::<codex_app_server_client::TypedRequestError>(),
                    Some(codex_app_server_client::TypedRequestError::Server { source, .. })
                        if source.code == -32601
                );
                self.clear_remote_reload(request_id, tracked_thread_id);
                if persistent_target && method_unsupported && allows_frontend_refresh {
                    self.chat_widget.add_info_message(
                        "The connected app-server does not expose managed reload; refreshing this frontend while leaving the server running."
                            .to_string(),
                        Some("No prompt or tool call will be replayed.".to_string()),
                    );
                    self.app_event_tx.send(AppEvent::FrontendRefreshRequested {
                        thread_id: tracked_thread_id,
                    });
                    return Ok(());
                }
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
        if pending.thread_id != thread_id
            || !pending_reload_request_matches(pending, &notification.request_id)
        {
            return;
        }
        let is_status_request =
            pending.status_request_id.as_ref() == Some(&notification.request_id);
        self.apply_remote_reload_payload(
            thread_id,
            notification.request_id.clone(),
            notification.result.ok,
            &notification.result.output.text,
            notification.result.reload.as_ref(),
            /*allow_new_pending*/ false,
            /*display_progress*/ false,
            is_status_request,
        );
    }

    pub(super) fn queue_remote_reload_status_after_reconnect(&self) {
        let Some(pending) = self.reconnect.pending_remote_reload.as_ref() else {
            return;
        };
        self.app_event_tx
            .send(AppEvent::RemoteReloadStatusRequested {
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
        is_status_request: bool,
    ) {
        self.apply_remote_reload_payload(
            thread_id,
            request_id,
            response.ok,
            &response.output.text,
            response.reload.as_ref(),
            allow_new_pending,
            display_progress,
            is_status_request,
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
        is_status_request: bool,
    ) {
        let state = remote_reload_state(reload, ok);
        let operation_request_match =
            self.reconnect
                .pending_remote_reload
                .as_ref()
                .is_some_and(|pending| {
                    pending.thread_id == thread_id && pending.request_id == request_id
                });
        let status_request_match =
            self.reconnect
                .pending_remote_reload
                .as_ref()
                .is_some_and(|pending| {
                    pending.thread_id == thread_id
                        && pending.status_request_id.as_ref() == Some(&request_id)
                });
        let matches_pending = operation_request_match || status_request_match;
        let duplicate_terminal_result = state.is_terminal()
            && self
                .reconnect
                .last_remote_reload_terminal
                .as_ref()
                .is_some_and(|(last_thread_id, last_request_id)| {
                    *last_thread_id == thread_id && last_request_id == &request_id
                });
        let present_terminal_result =
            !duplicate_terminal_result && (matches_pending || is_status_request);

        if state.is_terminal() && present_terminal_result {
            self.reconnect.last_remote_reload_terminal = Some((thread_id, request_id.clone()));
        }

        if matches!(
            state,
            RemoteReloadState::Accepted | RemoteReloadState::InProgress
        ) && allow_new_pending
            && self.reconnect.pending_remote_reload.is_none()
        {
            self.reconnect.pending_remote_reload = Some(PendingRemoteReload {
                thread_id,
                request_id: request_id.clone(),
                status_request_id: None,
                allow_frontend_refresh: false,
            });
        }

        let allow_frontend_refresh = self
            .reconnect
            .pending_remote_reload
            .as_ref()
            .is_some_and(|pending| pending.allow_frontend_refresh);
        match remote_reload_terminal_action(
            state,
            operation_request_match,
            status_request_match,
            allow_frontend_refresh,
        ) {
            RemoteReloadTerminalAction::Ignore => {}
            RemoteReloadTerminalAction::KeepPending => {
                if let Some(pending) = self.reconnect.pending_remote_reload.as_mut() {
                    pending.status_request_id = None;
                }
            }
            RemoteReloadTerminalAction::ClearPending => {
                self.reconnect.pending_remote_reload = None;
            }
            RemoteReloadTerminalAction::Completed => {
                self.reconnect.pending_remote_reload = None;
                self.app_event_tx
                    .send(AppEvent::RemoteReloadCompleted { thread_id });
            }
            RemoteReloadTerminalAction::FrontendRefresh => {
                self.reconnect.pending_remote_reload = None;
                self.app_event_tx
                    .send(AppEvent::FrontendRefreshRequested { thread_id });
            }
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
            RemoteReloadState::Accepted | RemoteReloadState::InProgress => {}
            RemoteReloadState::Completed => {}
            RemoteReloadState::Unavailable => {
                if present_terminal_result && allow_frontend_refresh {
                    self.chat_widget.add_info_message(
                        message.to_string(),
                        Some("Refreshing this frontend while the connected app-server continues running.".to_string()),
                    );
                } else if present_terminal_result {
                    self.chat_widget.add_error_message(message.to_string());
                }
            }
            RemoteReloadState::Failed | RemoteReloadState::Unknown => {
                if present_terminal_result {
                    self.chat_widget.add_error_message(message.to_string());
                }
            }
        }
    }

    fn clear_remote_reload(&mut self, request_id: RequestId, thread_id: ThreadId) {
        let status_request_matches =
            self.reconnect
                .pending_remote_reload
                .as_ref()
                .is_some_and(|pending| {
                    pending.thread_id == thread_id
                        && pending.status_request_id.as_ref() == Some(&request_id)
                });
        if status_request_matches {
            if let Some(pending) = self.reconnect.pending_remote_reload.as_mut() {
                pending.status_request_id = None;
            }
        } else if self
            .reconnect
            .pending_remote_reload
            .as_ref()
            .is_some_and(|pending| {
                pending.thread_id == thread_id && pending.request_id == request_id
            })
        {
            self.reconnect.pending_remote_reload = None;
        }
    }
}

#[cfg(test)]
#[path = "remote_reload_tests.rs"]
mod tests;
