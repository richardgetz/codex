//! Root-scoped ETA snapshot loading and notification projection for the TUI.
//!
//! Reads are issued when `/eta` opens and when the user explicitly requests another history page.
//! Notifications update the cached snapshot; neither path infers completion from turns, idle
//! state, or wall-clock time.

use super::App;
use super::AppServerSession;
use super::eta_time::EtaTimestampFormatter;
use super::eta_view::ETA_ACTIVE_TAB_ID;
use super::eta_view::ETA_VIEW_ID;
use super::eta_view::EtaAccuracy;
use super::eta_view::EtaOverall;
use super::eta_view::EtaRevision;
use super::eta_view::EtaSessionInfo;
use super::eta_view::EtaSessionTask;
use super::eta_view::EtaSnapshot;
use super::eta_view::EtaTask;
use super::eta_view::EtaTaskStatus;
use super::eta_view::EtaView;
use super::eta_view::EtaViewState;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadEtaAccuracy;
use codex_app_server_protocol::ThreadEtaListParams;
use codex_app_server_protocol::ThreadEtaListResponse;
use codex_app_server_protocol::ThreadEtaReadParams;
use codex_app_server_protocol::ThreadEtaReadResponse;
use codex_app_server_protocol::ThreadEtaStatus;
use codex_app_server_protocol::ThreadEtaTask;
use codex_app_server_protocol::ThreadEtaUpdatedNotification;
use codex_protocol::ThreadId;
use std::collections::HashMap;
use uuid::Uuid;

pub(super) const ETA_HISTORY_PAGE_SIZE: u32 = 50;

#[derive(Default)]
pub(super) struct EtaState {
    pub(super) snapshot: Option<EtaSnapshot>,
    pub(super) request_id: Option<Uuid>,
    pub(super) root_thread_id: Option<ThreadId>,
    pub(super) all_sessions: Vec<EtaSessionTask>,
    pub(super) all_sessions_next_cursor: Option<String>,
    pub(super) all_sessions_request_id: Option<Uuid>,
    pub(super) all_sessions_include_nested: bool,
    pub(super) eta_request_in_flight: bool,
    pub(super) eta_error: Option<String>,
    pub(super) all_sessions_error: Option<String>,
    pub(super) view_state: Option<EtaViewState>,
    pub(super) root_view_states: HashMap<ThreadId, EtaViewState>,
    pub(super) all_sessions_view_state: Option<EtaViewState>,
}

impl EtaState {
    pub(super) fn view_state_for_open(
        &self,
        root_thread_id: ThreadId,
        active_tab_id: Option<&str>,
    ) -> (String, Option<EtaViewState>) {
        let tab_id = active_tab_id
            .map(str::to_string)
            .or_else(|| {
                self.view_state
                    .as_ref()
                    .filter(|state| {
                        state.tab_id == super::eta_view::ETA_ALL_SESSIONS_TAB_ID
                    })
                    .map(|state| state.tab_id.clone())
            })
            .or_else(|| {
                self.root_view_states
                    .get(&root_thread_id)
                    .map(|state| state.tab_id.clone())
            })
            .unwrap_or_else(|| super::eta_view::ETA_ACTIVE_TAB_ID.to_string());
        let state = self.view_state_for_root(root_thread_id, &tab_id);
        (tab_id, state)
    }

    pub(super) fn view_state_for_root(
        &self,
        root_thread_id: ThreadId,
        tab_id: &str,
    ) -> Option<EtaViewState> {
        if tab_id == super::eta_view::ETA_ALL_SESSIONS_TAB_ID {
            self.all_sessions_view_state
                .as_ref()
                .filter(|state| state.tab_id == tab_id)
                .cloned()
        } else {
            self.root_view_states
                .get(&root_thread_id)
                .filter(|state| state.tab_id == tab_id)
                .cloned()
        }
    }
}

impl App {
    pub(super) fn show_eta_resume_confirmation(
        &mut self,
        target: crate::resume_picker::SessionTarget,
    ) {
        let target = target.clone();
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Resume another active session?".to_string()),
            items: vec![
                SelectionItem {
                    name: "Resume selected session".to_string(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::ResumeEtaSessionConfirmed {
                            target: target.clone(),
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Cancel".to_string(),
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
    }

    pub(super) async fn resume_eta_session_target(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        target: crate::resume_picker::SessionTarget,
        confirmed: bool,
    ) -> color_eyre::Result<AppRunControl> {
        let target_is_known = self.eta.all_sessions.iter().any(|task| {
            task.root_thread_id == target.thread_id.to_string()
                && task.session.thread_id == target.thread_id.to_string()
        });
        if !target_is_known {
            self.chat_widget.add_error_message(
                "That ETA session is no longer available. Refresh All Sessions and try again."
                    .to_string(),
            );
            return Ok(AppRunControl::Continue);
        }
        if self.reconnect.offline {
            self.chat_widget.add_error_message(
                "Session resume is unavailable while disconnected; retry after reconnect."
                    .to_string(),
            );
            return Ok(AppRunControl::Continue);
        }
        if !confirmed {
            let leaving_active_session = match self
                .active_thread_id
                .filter(|active| *active != target.thread_id)
            {
                Some(active) => {
                    self.active_turn_id_for_thread(active).await.is_some()
                        || self
                            .team_activity
                            .status_for_root(active, self.config.team.worker_max_concurrent)
                            .is_some_and(|status| {
                                status.workers_working > 0 || status.in_flight_operations > 0
                            })
                }
                None => false,
            };
            if (self.active_thread_id == Some(target.thread_id)
                && !self.thread_unavailable(target.thread_id))
                || leaving_active_session
            {
                self.show_eta_resume_confirmation(target);
                return Ok(AppRunControl::Continue);
            }
        }
        self.resume_target_session(tui, app_server, target).await
    }

    pub(super) fn open_eta(&mut self, app_server: &AppServerSession) {
        let Some(root_thread_id) = self
            .primary_thread_id
            .or_else(|| self.chat_widget.thread_id())
        else {
            self.chat_widget.add_error_message(
                "The ETA view is unavailable before the session starts.".to_string(),
            );
            return;
        };

        let previous_root_thread_id = self.eta.root_thread_id;
        self.eta.root_thread_id = Some(root_thread_id);
        self.eta.eta_request_in_flight = true;
        self.eta.eta_error = None;
        self.eta.all_sessions_error = None;
        let snapshot = self
            .eta
            .snapshot
            .clone()
            .filter(|snapshot| snapshot.root_thread_id == root_thread_id.to_string())
            .unwrap_or_else(|| empty_snapshot(root_thread_id));
        let active_tab_id = self
            .chat_widget
            .active_tab_id_for_active_view(ETA_VIEW_ID)
            .map(str::to_string);
        let (tab_id, view_state) = self
            .eta
            .view_state_for_open(root_thread_id, active_tab_id.as_deref());
        let selected_idx = if previous_root_thread_id == Some(root_thread_id) {
            self.chat_widget
                .selected_index_for_present_view(ETA_VIEW_ID)
        } else {
            None
        };
        let all_sessions_request_in_flight = self.eta.all_sessions_request_id.is_some()
            || tab_id == super::eta_view::ETA_ALL_SESSIONS_TAB_ID;
        self.chat_widget.show_bottom_pane_view(Box::new(
            EtaView::new_with_state_and_all_sessions_and_status(
                snapshot,
                self.keymap.list.clone(),
                self.app_event_tx.clone(),
                &tab_id,
                selected_idx,
                EtaTimestampFormatter::from_config(&self.config.eta),
                self.eta.all_sessions.clone(),
                self.eta.all_sessions_next_cursor.clone(),
                self.eta.all_sessions_include_nested,
                all_sessions_request_in_flight,
                self.primary_thread_id
                    .or(self.current_displayed_thread_id()),
                self.eta.eta_request_in_flight,
                self.eta.eta_error.clone(),
                self.eta.all_sessions_error.clone(),
                view_state,
            ),
        ));
        self.refresh_eta(app_server, root_thread_id, None);
        self.refresh_eta_sessions(app_server, None, self.eta.all_sessions_include_nested);
    }

    pub(super) fn refresh_eta(
        &mut self,
        app_server: &AppServerSession,
        root_thread_id: ThreadId,
        cursor: Option<String>,
    ) {
        let request_id = Uuid::new_v4();
        self.eta.request_id = Some(request_id);
        self.eta.root_thread_id = Some(root_thread_id);
        self.eta.eta_request_in_flight = true;
        self.eta.eta_error = None;
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<ThreadEtaReadResponse>(ClientRequest::ThreadEtaRead {
                    request_id: RequestId::String(format!("thread-eta-{request_id}")),
                    params: ThreadEtaReadParams {
                        thread_id: root_thread_id.to_string(),
                        cursor: cursor.clone(),
                        limit: Some(ETA_HISTORY_PAGE_SIZE),
                    },
                })
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::ThreadEtaSnapshotLoaded {
                root_thread_id,
                request_id,
                cursor,
                result,
            });
        });
    }

    pub(super) fn refresh_eta_sessions(
        &mut self,
        app_server: &AppServerSession,
        cursor: Option<String>,
        include_nested: bool,
    ) {
        let request_id = Uuid::new_v4();
        self.eta.all_sessions_request_id = Some(request_id);
        self.eta.all_sessions_error = None;
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<ThreadEtaListResponse>(ClientRequest::ThreadEtaList {
                    request_id: RequestId::String(format!("thread-eta-list-{request_id}")),
                    params: ThreadEtaListParams {
                        cursor: cursor.clone(),
                        limit: Some(50),
                        include_nested,
                    },
                })
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::ThreadEtaSessionsLoaded {
                request_id,
                cursor,
                include_nested,
                result,
            });
        });
    }

    pub(super) fn apply_eta_snapshot(
        &mut self,
        root_thread_id: ThreadId,
        request_id: Uuid,
        cursor: Option<String>,
        result: Result<ThreadEtaReadResponse, String>,
    ) {
        if self.eta.request_id != Some(request_id)
            || self.eta.root_thread_id != Some(root_thread_id)
        {
            return;
        }
        self.eta.request_id = None;
        self.eta.eta_request_in_flight = false;
        match result {
            Ok(response) => {
                let incoming = snapshot_from_api(response.snapshot);
                if incoming.root_thread_id != root_thread_id.to_string() {
                    return;
                }
                if cursor.is_some() {
                    self.merge_eta_history(incoming);
                } else if self
                    .eta
                    .snapshot
                    .as_ref()
                    .is_none_or(|current| incoming.sequence >= current.sequence)
                {
                    self.eta.snapshot = Some(incoming);
                }
            }
            Err(error) => {
                self.eta.eta_error = Some(error);
            }
        }
        self.repaint_eta();
    }

    pub(super) fn apply_eta_sessions(
        &mut self,
        request_id: Uuid,
        cursor: Option<String>,
        include_nested: bool,
        result: Result<ThreadEtaListResponse, String>,
    ) {
        if self.eta.all_sessions_request_id != Some(request_id) {
            return;
        }
        self.eta.all_sessions_request_id = None;
        self.eta.all_sessions_error = None;
        match result {
            Ok(response) => {
                let incoming = response
                    .data
                    .into_iter()
                    .map(session_task_from_api)
                    .collect::<Vec<_>>();
                if cursor.is_some() && self.eta.all_sessions_include_nested == include_nested {
                    self.merge_eta_sessions(incoming);
                } else {
                    self.eta.all_sessions = incoming;
                }
                self.eta.all_sessions_include_nested = include_nested;
                self.eta.all_sessions_next_cursor = response.next_cursor;
            }
            Err(error) => {
                self.eta.all_sessions_error = Some(error);
            }
        }
        self.repaint_eta();
    }

    fn merge_eta_sessions(&mut self, incoming: Vec<EtaSessionTask>) {
        for task in incoming {
            if let Some(existing) = self.eta.all_sessions.iter_mut().find(|existing| {
                existing.task_id == task.task_id && existing.root_thread_id == task.root_thread_id
            }) {
                *existing = task;
            } else {
                self.eta.all_sessions.push(task);
            }
        }
    }

    pub(super) fn apply_eta_notification(&mut self, notification: &ThreadEtaUpdatedNotification) {
        let Ok(root_thread_id) = ThreadId::from_string(&notification.root_thread_id) else {
            return;
        };
        if self.eta.root_thread_id != Some(root_thread_id) {
            return;
        }
        let snapshot = self
            .eta
            .snapshot
            .get_or_insert_with(|| empty_snapshot(root_thread_id));
        if notification.sequence <= snapshot.sequence {
            return;
        }
        snapshot.generated_at = notification.generated_at;
        snapshot.sequence = notification.sequence;
        snapshot.overall = overall_from_api(&notification.overall);
        for changed in &notification.changed_tasks {
            let task = task_from_api(changed.clone());
            snapshot
                .active
                .retain(|existing| existing.task_id != task.task_id);
            snapshot
                .history
                .retain(|existing| existing.task_id != task.task_id);
            if matches!(
                task.status,
                EtaTaskStatus::Completed | EtaTaskStatus::Cancelled
            ) {
                snapshot.history.push(task);
            } else {
                snapshot.active.push(task);
            }
        }
        self.repaint_eta();
    }

    pub(super) fn apply_eta_view_state(&mut self, root_thread_id: String, state: EtaViewState) {
        if state.tab_id == super::eta_view::ETA_ALL_SESSIONS_TAB_ID {
            self.eta.all_sessions_view_state = Some(state.clone());
        } else if let Ok(root_thread_id) = ThreadId::from_string(&root_thread_id) {
            self.eta.root_view_states.insert(root_thread_id, state.clone());
        }
        self.eta.view_state = Some(state);
    }

    fn merge_eta_history(&mut self, incoming: EtaSnapshot) {
        let Some(snapshot) = self.eta.snapshot.as_mut() else {
            self.eta.snapshot = Some(incoming);
            return;
        };
        if incoming.sequence < snapshot.sequence {
            return;
        }
        snapshot.generated_at = incoming.generated_at;
        snapshot.sequence = incoming.sequence;
        snapshot.active = incoming.active;
        snapshot.overall = incoming.overall;
        snapshot.next_cursor = incoming.next_cursor;
        for task in incoming.history {
            if let Some(existing) = snapshot
                .history
                .iter_mut()
                .find(|existing| existing.task_id == task.task_id)
            {
                *existing = task;
            } else {
                snapshot.history.push(task);
            }
        }
    }

    pub(super) fn repaint_eta(&mut self) {
        let Some(root_thread_id) = self.eta.root_thread_id else {
            return;
        };
        let snapshot = self
            .eta
            .snapshot
            .clone()
            .unwrap_or_else(|| empty_snapshot(root_thread_id));
        if self
            .chat_widget
            .active_tab_id_for_active_view(ETA_VIEW_ID)
            .is_none()
        {
            return;
        }
        let tab_id = self
            .chat_widget
            .active_tab_id_for_active_view(ETA_VIEW_ID)
            .map(str::to_string)
            .unwrap_or_else(|| ETA_ACTIVE_TAB_ID.to_string());
        let selected_idx = self
            .chat_widget
            .selected_index_for_present_view(ETA_VIEW_ID);
        let view_state = self.eta.view_state_for_root(root_thread_id, &tab_id);
        let view = EtaView::new_with_state_and_all_sessions_and_status(
            snapshot,
            self.keymap.list.clone(),
            self.app_event_tx.clone(),
            &tab_id,
            selected_idx,
            EtaTimestampFormatter::from_config(&self.config.eta),
            self.eta.all_sessions.clone(),
            self.eta.all_sessions_next_cursor.clone(),
            self.eta.all_sessions_include_nested,
            self.eta.all_sessions_request_id.is_some(),
            self.primary_thread_id
                .or(self.current_displayed_thread_id()),
            self.eta.eta_request_in_flight,
            self.eta.eta_error.clone(),
            self.eta.all_sessions_error.clone(),
            view_state,
        );
        self.chat_widget
            .replace_bottom_pane_view_if_present(ETA_VIEW_ID, Box::new(view));
    }
}

fn empty_snapshot(root_thread_id: ThreadId) -> EtaSnapshot {
    EtaSnapshot {
        root_thread_id: root_thread_id.to_string(),
        generated_at: 0,
        sequence: 0,
        active: Vec::new(),
        history: Vec::new(),
        next_cursor: None,
        overall: EtaOverall {
            finish_at: None,
            remaining_lower_seconds: None,
            remaining_upper_seconds: None,
            unknown_reason: Some("No stored task estimates yet".to_string()),
        },
    }
}

fn snapshot_from_api(snapshot: codex_app_server_protocol::ThreadEtaSnapshot) -> EtaSnapshot {
    let overall = overall_from_api(&snapshot.overall);
    let mut active = Vec::new();
    let mut history = snapshot
        .history
        .into_iter()
        .map(task_from_api)
        .collect::<Vec<_>>();
    for task in snapshot.active.into_iter().map(task_from_api) {
        if matches!(
            task.status,
            EtaTaskStatus::Completed | EtaTaskStatus::Cancelled
        ) {
            history.push(task);
        } else {
            active.push(task);
        }
    }
    EtaSnapshot {
        root_thread_id: snapshot.root_thread_id,
        generated_at: snapshot.generated_at,
        sequence: snapshot.sequence,
        active,
        history,
        next_cursor: snapshot.next_cursor,
        overall,
    }
}

fn task_from_api(task: ThreadEtaTask) -> EtaTask {
    EtaTask {
        task_id: task.task_id,
        owner_thread_id: task.owner_thread_id,
        parent_task_id: task.parent_task_id,
        depends_on_task_ids: task.depends_on_task_ids,
        title: task.title,
        status: match task.status {
            ThreadEtaStatus::Pending => EtaTaskStatus::Pending,
            ThreadEtaStatus::Active => EtaTaskStatus::Active,
            ThreadEtaStatus::Blocked => EtaTaskStatus::Blocked,
            ThreadEtaStatus::Completed => EtaTaskStatus::Completed,
            ThreadEtaStatus::Cancelled => EtaTaskStatus::Cancelled,
            ThreadEtaStatus::Unknown => EtaTaskStatus::Unknown,
        },
        current_lower_seconds: task.current_lower_seconds,
        current_upper_seconds: task.current_upper_seconds,
        original_lower_seconds: task.original_lower_seconds,
        original_upper_seconds: task.original_upper_seconds,
        started_at: task.started_at,
        terminal_at: task.terminal_at,
        actual_elapsed_seconds: task.actual_elapsed_seconds,
        updated_at: task.updated_at,
        is_stale: task.is_stale,
        accuracy: match task.accuracy {
            ThreadEtaAccuracy::Early => EtaAccuracy::Early,
            ThreadEtaAccuracy::Within => EtaAccuracy::Within,
            ThreadEtaAccuracy::Late => EtaAccuracy::Late,
            ThreadEtaAccuracy::Unknown => EtaAccuracy::Unknown,
        },
        revisions: task
            .revisions
            .into_iter()
            .map(|revision| EtaRevision {
                lower_seconds: revision.lower_seconds,
                upper_seconds: revision.upper_seconds,
                reason: revision.reason,
                updated_at: revision.updated_at,
                actor_thread_id: revision.actor_thread_id,
            })
            .collect(),
    }
}

fn session_task_from_api(task: codex_app_server_protocol::ThreadEtaSessionTask) -> EtaSessionTask {
    EtaSessionTask {
        task_id: task.task_id,
        root_thread_id: task.root_thread_id,
        parent_task_id: task.parent_task_id,
        title: task.title,
        status: match task.status {
            ThreadEtaStatus::Pending => EtaTaskStatus::Pending,
            ThreadEtaStatus::Active => EtaTaskStatus::Active,
            ThreadEtaStatus::Blocked => EtaTaskStatus::Blocked,
            ThreadEtaStatus::Completed => EtaTaskStatus::Completed,
            ThreadEtaStatus::Cancelled => EtaTaskStatus::Cancelled,
            ThreadEtaStatus::Unknown => EtaTaskStatus::Unknown,
        },
        current_lower_seconds: task.current_lower_seconds,
        current_upper_seconds: task.current_upper_seconds,
        is_stale: task.is_stale,
        session: EtaSessionInfo {
            thread_id: task.session.thread_id,
            title: task.session.title,
            name: task.session.name,
            preview: task.session.preview,
            created_at: task.session.created_at,
            updated_at: task.session.updated_at,
            archived_at: task.session.archived_at,
            cwd: task.session.cwd,
        },
        nested_task_count: task.nested_task_count,
        active_nested_task_count: task.active_nested_task_count,
        nested_lower_seconds: task.nested_lower_seconds,
        nested_upper_seconds: task.nested_upper_seconds,
    }
}

fn overall_from_api(overall: &codex_app_server_protocol::ThreadEtaOverall) -> EtaOverall {
    EtaOverall {
        finish_at: overall.finish_at,
        remaining_lower_seconds: overall.remaining_lower_seconds,
        remaining_upper_seconds: overall.remaining_upper_seconds,
        unknown_reason: overall.unknown_reason.clone(),
    }
}
