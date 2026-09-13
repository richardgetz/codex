//! Root-scoped ETA snapshot loading and notification projection for the TUI.
//!
//! Reads are issued when `/eta` opens and when the user explicitly requests another history page.
//! Notifications update the cached snapshot; neither path infers completion from turns, idle
//! state, or wall-clock time.

use super::App;
use super::AppServerSession;
use super::eta_view::ETA_ACTIVE_TAB_ID;
use super::eta_view::ETA_VIEW_ID;
use super::eta_view::EtaAccuracy;
use super::eta_view::EtaOverall;
use super::eta_view::EtaRevision;
use super::eta_view::EtaSnapshot;
use super::eta_view::EtaTask;
use super::eta_view::EtaTaskStatus;
use super::eta_view::EtaView;
use crate::app_event::AppEvent;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadEtaAccuracy;
use codex_app_server_protocol::ThreadEtaReadParams;
use codex_app_server_protocol::ThreadEtaReadResponse;
use codex_app_server_protocol::ThreadEtaStatus;
use codex_app_server_protocol::ThreadEtaTask;
use codex_app_server_protocol::ThreadEtaUpdatedNotification;
use codex_protocol::ThreadId;
use uuid::Uuid;

pub(super) const ETA_HISTORY_PAGE_SIZE: u32 = 50;

#[derive(Default)]
pub(super) struct EtaState {
    pub(super) snapshot: Option<EtaSnapshot>,
    pub(super) request_id: Option<Uuid>,
    pub(super) root_thread_id: Option<ThreadId>,
}

impl App {
    pub(super) fn open_eta(&mut self, app_server: &AppServerSession) {
        let Some(root_thread_id) = self.primary_thread_id.or_else(|| self.chat_widget.thread_id())
        else {
            self.chat_widget
                .add_error_message("The ETA view is unavailable before the session starts.".to_string());
            return;
        };

        self.eta.root_thread_id = Some(root_thread_id);
        let snapshot = self
            .eta
            .snapshot
            .clone()
            .filter(|snapshot| snapshot.root_thread_id == root_thread_id.to_string())
            .unwrap_or_else(|| empty_snapshot(root_thread_id));
        let tab_id = self
            .chat_widget
            .active_tab_id_for_active_view(ETA_VIEW_ID)
            .unwrap_or(ETA_ACTIVE_TAB_ID);
        let selected_idx = self
            .chat_widget
            .selected_index_for_present_view(ETA_VIEW_ID);
        self.chat_widget.show_bottom_pane_view(Box::new(
            EtaView::new_with_state(
                snapshot,
                self.keymap.list.clone(),
                self.app_event_tx.clone(),
                tab_id,
                selected_idx,
            ),
        ));
        self.refresh_eta(app_server, root_thread_id, None);
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
                if self.chat_widget.active_tab_id_for_active_view(ETA_VIEW_ID).is_some() {
                    self.chat_widget
                        .add_error_message(format!("Failed to load ETA: {error}"));
                }
            }
        }
        self.repaint_eta();
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
            snapshot.active.retain(|existing| existing.task_id != task.task_id);
            snapshot.history.retain(|existing| existing.task_id != task.task_id);
            if matches!(task.status, EtaTaskStatus::Completed | EtaTaskStatus::Cancelled) {
                snapshot.history.push(task);
            } else {
                snapshot.active.push(task);
            }
        }
        self.repaint_eta();
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
        let Some(snapshot) = self.eta.snapshot.clone() else {
            return;
        };
        if self.chat_widget.active_tab_id_for_active_view(ETA_VIEW_ID).is_none() {
            return;
        }
        let selected_idx = self
            .chat_widget
            .selected_index_for_present_view(ETA_VIEW_ID);
        let tab_id = self
            .chat_widget
            .active_tab_id_for_active_view(ETA_VIEW_ID)
            .unwrap_or(ETA_ACTIVE_TAB_ID);
        let view = EtaView::new_with_state(
            snapshot,
            self.keymap.list.clone(),
            self.app_event_tx.clone(),
            tab_id,
            selected_idx,
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
        if matches!(task.status, EtaTaskStatus::Completed | EtaTaskStatus::Cancelled) {
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

fn overall_from_api(overall: &codex_app_server_protocol::ThreadEtaOverall) -> EtaOverall {
    EtaOverall {
        finish_at: overall.finish_at,
        remaining_lower_seconds: overall.remaining_lower_seconds,
        remaining_upper_seconds: overall.remaining_upper_seconds,
        unknown_reason: overall.unknown_reason.clone(),
    }
}
