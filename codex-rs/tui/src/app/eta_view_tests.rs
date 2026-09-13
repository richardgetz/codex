use super::EtaAccuracy;
use super::EtaOverall;
use super::EtaRevision;
use super::EtaSnapshot;
use super::EtaTask;
use super::EtaTaskStatus;
use super::EtaView;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPaneView;
use crate::keymap::RuntimeKeymap;
use crate::render::renderable::Renderable;
use codex_protocol::ThreadId;
use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

fn task(
    task_id: &str,
    owner_thread_id: &str,
    parent_task_id: Option<&str>,
    title: &str,
    status: EtaTaskStatus,
) -> EtaTask {
    EtaTask {
        task_id: task_id.to_string(),
        owner_thread_id: owner_thread_id.to_string(),
        parent_task_id: parent_task_id.map(str::to_string),
        depends_on_task_ids: Vec::new(),
        title: title.to_string(),
        status,
        current_lower_seconds: Some(60),
        current_upper_seconds: Some(120),
        original_lower_seconds: Some(90),
        original_upper_seconds: Some(180),
        started_at: Some(1_700_000_000),
        actual_elapsed_seconds: None,
        updated_at: 1_700_000_020,
        is_stale: false,
        accuracy: EtaAccuracy::Unknown,
        revisions: vec![EtaRevision {
            lower_seconds: Some(60),
            upper_seconds: Some(120),
            reason: Some("worker update".to_string()),
            updated_at: 1_700_000_020,
            actor_thread_id: owner_thread_id.to_string(),
        }],
    }
}

fn snapshot() -> EtaSnapshot {
    let root = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
        .expect("valid root thread")
        .to_string();
    let worker = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
        .expect("valid worker thread")
        .to_string();
    let mut root_task = task("root-task", &root, None, "Prepare release", EtaTaskStatus::Active);
    root_task.depends_on_task_ids = vec!["dependency-with-unknown-duration".to_string()];
    let mut child = task(
        "child-task",
        &worker,
        Some("root-task"),
        "Run checks",
        EtaTaskStatus::Blocked,
    );
    child.is_stale = true;
    let mut completed = task(
        "done-task",
        &worker,
        None,
        "Publish notes",
        EtaTaskStatus::Completed,
    );
    completed.current_lower_seconds = None;
    completed.current_upper_seconds = None;
    completed.actual_elapsed_seconds = Some(75);
    completed.accuracy = EtaAccuracy::Within;
    let mut cancelled = task(
        "cancelled-task",
        &root,
        None,
        "Optional cleanup",
        EtaTaskStatus::Cancelled,
    );
    cancelled.actual_elapsed_seconds = Some(12);
    cancelled.accuracy = EtaAccuracy::Late;
    EtaSnapshot {
        root_thread_id: root,
        generated_at: 1_700_000_030,
        sequence: 4,
        active: vec![child, root_task],
        history: vec![completed, cancelled],
        next_cursor: Some("history-next".to_string()),
        overall: EtaOverall {
            finish_at: None,
            remaining_lower_seconds: None,
            remaining_upper_seconds: None,
            unknown_reason: Some("blocked dependency".to_string()),
        },
    }
}

fn known_finish_snapshot() -> EtaSnapshot {
    let mut snapshot = snapshot();
    for task in &mut snapshot.active {
        task.status = EtaTaskStatus::Active;
        task.is_stale = false;
        task.depends_on_task_ids.clear();
    }
    snapshot.overall = EtaOverall {
        finish_at: Some(snapshot.generated_at + 120),
        remaining_lower_seconds: Some(60),
        remaining_upper_seconds: Some(120),
        unknown_reason: None,
    };
    snapshot
}

fn render(view: &EtaView, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| view.render(frame.area(), frame.buffer_mut()))
        .expect("render ETA view");
    terminal.backend().to_string()
}

fn view() -> EtaView {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    EtaView::new(
        snapshot(),
        RuntimeKeymap::defaults().list,
        AppEventSender::new(tx),
    )
}

#[test]
fn active_tree_renders_unknown_and_stale_rows() {
    insta::assert_snapshot!("eta_active_tree", render(&view(), 96, 24));
}

#[test]
fn history_renders_accuracy_and_cancelled_without_accuracy() {
    let mut view = view();
    view.handle_key_event(KeyCode::Right.into());
    insta::assert_snapshot!("eta_history_accuracy", render(&view, 96, 24));
}

#[test]
fn overall_finish_range_uses_fixed_snapshot_time_in_utc() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let view = EtaView::new(
        known_finish_snapshot(),
        RuntimeKeymap::defaults().list,
        AppEventSender::new(tx),
    );
    insta::assert_snapshot!("eta_finish_range_utc", render(&view, 96, 24));
}

#[test]
fn active_remaining_uses_snapshot_anchor_and_started_at() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let view = EtaView::new(
        known_finish_snapshot(),
        RuntimeKeymap::defaults().list,
        AppEventSender::new(tx),
    );
    let rendered = render(&view, 96, 24);
    let root_row = rendered
        .lines()
        .find(|line| line.contains("Prepare release"))
        .expect("root task row");
    assert!(root_row.contains("30s–1m"));
    assert!(!root_row.contains("1m–2m"));
}

#[test]
fn normal_width_keeps_timing_column_visible() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let view = EtaView::new(
        known_finish_snapshot(),
        RuntimeKeymap::defaults().list,
        AppEventSender::new(tx),
    );
    insta::assert_snapshot!("eta_columns_at_80", render(&view, 80, 24));
}

#[test]
fn enter_expands_details_and_escape_completes() {
    let mut view = view();
    view.handle_key_event(KeyCode::Enter.into());
    let expanded = render(&view, 96, 24);
    assert!(expanded.contains("Task details"));
    assert!(!expanded.contains("Run checks"));
    view.handle_key_event(KeyCode::Enter.into());
    assert!(render(&view, 96, 24).contains("Run checks"));
    view.handle_key_event(KeyCode::Esc.into());
    assert!(view.is_complete());
}

#[test]
fn history_page_down_requests_next_cursor_once() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut view = EtaView::new(
        snapshot(),
        RuntimeKeymap::defaults().list,
        AppEventSender::new(tx),
    );
    view.handle_key_event(KeyCode::Right.into());
    view.handle_key_event(KeyCode::PageDown.into());
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::LoadEtaHistory { cursor, .. }) if cursor == "history-next"
    ));
    view.handle_key_event(KeyCode::PageDown.into());
    assert!(rx.try_recv().is_err());
}
