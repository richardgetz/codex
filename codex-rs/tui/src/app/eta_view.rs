//! Read-only task estimate view for the current root session.
//!
//! The view deliberately owns only presentation data. The app layer converts the app-server
//! snapshot into these private rows, and refreshes the view when a read or update notification
//! arrives. No elapsed time is inferred here; every value shown comes from the stored snapshot.

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPaneView;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::ScrollState;
use crate::bottom_pane::ViewCompletion;
use crate::bottom_pane::popup_consts::MAX_POPUP_ROWS;
use crate::keymap::KeymapContext;
use crate::keymap::KeymapContextSet;
use crate::keymap::ListAction;
use crate::keymap::ListKeymap;
use codex_protocol::ThreadId;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use std::collections::HashMap;
use std::collections::HashSet;

pub(super) const ETA_VIEW_ID: &str = "thread-eta";
pub(super) const ETA_ACTIVE_TAB_ID: &str = "active";
pub(super) const ETA_HISTORY_TAB_ID: &str = "history";

#[path = "eta_view_render.rs"]
mod eta_view_render;

/// Private task status used by the renderer. Values are copied from the app-server enum without
/// adding a second lifecycle source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EtaTaskStatus {
    Pending,
    Active,
    Blocked,
    Completed,
    Cancelled,
    Unknown,
}

impl EtaTaskStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

/// Timing accuracy supplied by the persisted task record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EtaAccuracy {
    Early,
    Within,
    Late,
    Unknown,
}

impl EtaAccuracy {
    fn label(self) -> &'static str {
        match self {
            Self::Early => "Early",
            Self::Within => "Within",
            Self::Late => "Late",
            Self::Unknown => "Unestimated",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EtaRevision {
    pub(super) lower_seconds: Option<i64>,
    pub(super) upper_seconds: Option<i64>,
    pub(super) reason: Option<String>,
    pub(super) updated_at: i64,
    pub(super) actor_thread_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EtaTask {
    pub(super) task_id: String,
    pub(super) owner_thread_id: String,
    pub(super) parent_task_id: Option<String>,
    pub(super) depends_on_task_ids: Vec<String>,
    pub(super) title: String,
    pub(super) status: EtaTaskStatus,
    pub(super) current_lower_seconds: Option<i64>,
    pub(super) current_upper_seconds: Option<i64>,
    pub(super) original_lower_seconds: Option<i64>,
    pub(super) original_upper_seconds: Option<i64>,
    pub(super) started_at: Option<i64>,
    pub(super) actual_elapsed_seconds: Option<i64>,
    pub(super) updated_at: i64,
    pub(super) is_stale: bool,
    pub(super) accuracy: EtaAccuracy,
    pub(super) revisions: Vec<EtaRevision>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EtaOverall {
    pub(super) finish_at: Option<i64>,
    pub(super) remaining_lower_seconds: Option<i64>,
    pub(super) remaining_upper_seconds: Option<i64>,
    pub(super) unknown_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EtaSnapshot {
    pub(super) root_thread_id: String,
    pub(super) generated_at: i64,
    pub(super) sequence: i64,
    pub(super) active: Vec<EtaTask>,
    pub(super) history: Vec<EtaTask>,
    pub(super) next_cursor: Option<String>,
    pub(super) overall: EtaOverall,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum EtaTab {
    #[default]
    Active,
    History,
}

impl EtaTab {
    fn id(self) -> &'static str {
        match self {
            Self::Active => ETA_ACTIVE_TAB_ID,
            Self::History => ETA_HISTORY_TAB_ID,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::History => "History",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Active => Self::History,
            Self::History => Self::Active,
        }
    }

    fn previous(self) -> Self {
        self.next()
    }
}

/// Read-only `/eta` popup state.
pub(super) struct EtaView {
    snapshot: EtaSnapshot,
    tab: EtaTab,
    state: ScrollState,
    expanded: bool,
    complete: Option<ViewCompletion>,
    keymap: ListKeymap,
    app_event_tx: AppEventSender,
    requested_history_cursor: Option<String>,
    collapsed_task_ids: HashSet<String>,
}

impl EtaView {
    pub(super) fn new(
        snapshot: EtaSnapshot,
        keymap: ListKeymap,
        app_event_tx: AppEventSender,
    ) -> Self {
        Self::new_with_state(snapshot, keymap, app_event_tx, ETA_ACTIVE_TAB_ID, None)
    }

    pub(super) fn new_with_state(
        snapshot: EtaSnapshot,
        keymap: ListKeymap,
        app_event_tx: AppEventSender,
        tab_id: &str,
        selected_idx: Option<usize>,
    ) -> Self {
        let mut view = Self {
            snapshot,
            tab: match tab_id {
                ETA_HISTORY_TAB_ID => EtaTab::History,
                _ => EtaTab::Active,
            },
            state: ScrollState {
                selected_idx,
                ..ScrollState::new()
            },
            expanded: false,
            complete: None,
            keymap,
            app_event_tx,
            requested_history_cursor: None,
            collapsed_task_ids: HashSet::new(),
        };
        view.clamp_selection();
        view
    }

    fn tasks(&self) -> &[EtaTask] {
        match self.tab {
            EtaTab::Active => &self.snapshot.active,
            EtaTab::History => &self.snapshot.history,
        }
    }

    /// Return row indices in a parent-before-child order while retaining the stored order among
    /// siblings. Missing parents remain visible at the root instead of disappearing.
    fn ordered_indices(&self) -> Vec<usize> {
        let tasks = self.tasks();
        let known_ids: HashSet<&str> = tasks.iter().map(|task| task.task_id.as_str()).collect();
        let mut children: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
        for (idx, task) in tasks.iter().enumerate() {
            let parent = task
                .parent_task_id
                .as_deref()
                .filter(|parent| known_ids.contains(parent));
            children.entry(parent).or_default().push(idx);
        }

        fn mark_hidden(
            parent: Option<&str>,
            children: &HashMap<Option<&str>, Vec<usize>>,
            tasks: &[EtaTask],
            seen: &mut HashSet<usize>,
        ) {
            let Some(indices) = children.get(&parent) else {
                return;
            };
            for &idx in indices {
                if seen.insert(idx) {
                    mark_hidden(Some(tasks[idx].task_id.as_str()), children, tasks, seen);
                }
            }
        }

        fn visit(
            parent: Option<&str>,
            children: &HashMap<Option<&str>, Vec<usize>>,
            tasks: &[EtaTask],
            collapsed_task_ids: &HashSet<String>,
            output: &mut Vec<usize>,
            seen: &mut HashSet<usize>,
        ) {
            let Some(indices) = children.get(&parent) else {
                return;
            };
            for &idx in indices {
                if seen.insert(idx) {
                    output.push(idx);
                    if !collapsed_task_ids.contains(tasks[idx].task_id.as_str()) {
                        visit(
                            Some(tasks[idx].task_id.as_str()),
                            children,
                            tasks,
                            collapsed_task_ids,
                            output,
                            seen,
                        );
                    } else {
                        mark_hidden(Some(tasks[idx].task_id.as_str()), children, tasks, seen);
                    }
                }
            }
        }

        let mut ordered = Vec::with_capacity(tasks.len());
        let mut seen = HashSet::with_capacity(tasks.len());
        visit(
            None,
            &children,
            tasks,
            &self.collapsed_task_ids,
            &mut ordered,
            &mut seen,
        );
        // A malformed/cyclic parent graph must not hide a row from the read-only view.
        for idx in 0..tasks.len() {
            if seen.insert(idx) {
                ordered.push(idx);
            }
        }
        ordered
    }

    fn task_depth(&self, task_idx: usize) -> usize {
        let tasks = self.tasks();
        let by_id: HashMap<&str, usize> = tasks
            .iter()
            .enumerate()
            .map(|(idx, task)| (task.task_id.as_str(), idx))
            .collect();
        let mut current = task_idx;
        let mut depth = 0;
        let mut seen = HashSet::new();
        while let Some(parent_id) = tasks[current].parent_task_id.as_deref() {
            let Some(&parent_idx) = by_id.get(parent_id) else {
                break;
            };
            if !seen.insert(parent_idx) {
                break;
            }
            depth += 1;
            current = parent_idx;
        }
        depth
    }

    fn selected_task(&self) -> Option<&EtaTask> {
        let idx = self.state.selected_idx?;
        self.ordered_indices()
            .get(idx)
            .and_then(|task_idx| self.tasks().get(*task_idx))
    }

    fn clamp_selection(&mut self) {
        self.state.clamp_selection(self.ordered_indices().len());
        self.state
            .ensure_visible(self.ordered_indices().len(), MAX_POPUP_ROWS);
    }

    fn visible_rows(&self) -> usize {
        MAX_POPUP_ROWS.min(self.ordered_indices().len().max(1))
    }

    fn move_selection(&mut self, action: ListAction) {
        let len = self.ordered_indices().len();
        match action {
            ListAction::MoveUp => self.state.move_up_wrap(len),
            ListAction::MoveDown => self.state.move_down_wrap(len),
            ListAction::PageUp => self.state.page_up_clamped(len, self.visible_rows()),
            ListAction::PageDown => self.state.page_down_clamped(len, self.visible_rows()),
            ListAction::JumpTop => self.state.jump_top(len, self.visible_rows()),
            ListAction::JumpBottom => self.state.jump_bottom(len, self.visible_rows()),
            ListAction::MoveLeft
            | ListAction::MoveRight
            | ListAction::Accept
            | ListAction::Cancel => {}
        }
        self.state.ensure_visible(len, self.visible_rows());
    }

    fn request_next_history_page(&mut self) {
        if self.tab != EtaTab::History {
            return;
        }
        let Some(cursor) = self.snapshot.next_cursor.clone() else {
            return;
        };
        if self.requested_history_cursor.as_deref() == Some(cursor.as_str()) {
            return;
        }
        let Some(root_thread_id) = ThreadId::from_string(&self.snapshot.root_thread_id).ok() else {
            return;
        };
        self.requested_history_cursor = Some(cursor.clone());
        self.app_event_tx.send(AppEvent::LoadEtaHistory {
            root_thread_id,
            cursor,
        });
    }

    fn switch_tab(&mut self, tab: EtaTab) {
        self.tab = tab;
        self.state.reset();
        self.clamp_selection();
        self.expanded = false;
    }

    fn toggle_selected(&mut self) {
        let Some(task_id) = self.selected_task().map(|task| task.task_id.clone()) else {
            self.expanded = !self.expanded;
            return;
        };
        let has_children = self
            .tasks()
            .iter()
            .any(|task| task.parent_task_id.as_deref() == Some(task_id.as_str()));
        if has_children {
            if !self.collapsed_task_ids.insert(task_id.clone()) {
                self.collapsed_task_ids.remove(&task_id);
            }
            self.clamp_selection();
        }
        self.expanded = !self.expanded;
    }

    fn close(&mut self) {
        self.complete = Some(ViewCompletion::Cancelled);
    }
}

impl BottomPaneView for EtaView {
    fn keymap_contexts(&self) -> KeymapContextSet {
        KeymapContextSet::new(KeymapContext::List)
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        if let Some(action) = self.keymap.action_for(key_event) {
            match action {
                ListAction::MoveUp
                | ListAction::MoveDown
                | ListAction::PageUp
                | ListAction::PageDown
                | ListAction::JumpTop
                | ListAction::JumpBottom => self.move_selection(action),
                ListAction::MoveLeft => self.switch_tab(EtaTab::Active),
                ListAction::MoveRight => self.switch_tab(EtaTab::History),
                ListAction::Accept => self.toggle_selected(),
                ListAction::Cancel => self.close(),
            }
            if action == ListAction::PageDown
                && self
                    .state
                    .selected_idx
                    .is_some_and(|idx| idx.saturating_add(1) >= self.ordered_indices().len())
            {
                self.request_next_history_page();
            }
            return;
        }
        match key_event {
            KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.switch_tab(self.tab.next()),
            KeyEvent {
                code: KeyCode::BackTab,
                modifiers: KeyModifiers::SHIFT,
                ..
            } => self.switch_tab(self.tab.previous()),
            _ => {}
        }
    }

    fn is_complete(&self) -> bool {
        self.complete.is_some()
    }

    fn completion(&self) -> Option<ViewCompletion> {
        self.complete
    }

    fn view_id(&self) -> Option<&'static str> {
        Some(ETA_VIEW_ID)
    }

    fn selected_index(&self) -> Option<usize> {
        self.state.selected_idx
    }

    fn active_tab_id(&self) -> Option<&str> {
        Some(self.tab.id())
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.close();
        CancellationEvent::Handled
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "eta_view_tests.rs"]
mod tests;
