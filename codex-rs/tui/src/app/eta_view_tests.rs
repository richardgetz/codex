use super::super::eta_time::EtaTimestampFormatter;
use super::EtaAccuracy;
use super::EtaOverall;
use super::EtaRevision;
use super::EtaSessionInfo;
use super::EtaSessionTask;
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
use jiff::tz::TimeZone;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

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
        // API task projections carry the range remaining at `generated_at`.
        current_lower_seconds: Some(30),
        current_upper_seconds: Some(90),
        original_lower_seconds: Some(90),
        original_upper_seconds: Some(180),
        started_at: Some(1_700_000_000),
        terminal_at: None,
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
    let mut root_task = task(
        "root-task",
        &root,
        None,
        "Prepare release",
        EtaTaskStatus::Active,
    );
    root_task.depends_on_task_ids = vec!["dependency-with-unknown-duration".to_string()];
    let mut child = task(
        "child-task",
        &worker,
        Some("root-task"),
        "Run checks",
        EtaTaskStatus::Blocked,
    );
    // Stale API rows intentionally retain their saved estimate rather than a
    // from-now range so the view can show the actionable value with a warning.
    child.current_lower_seconds = Some(60);
    child.current_upper_seconds = Some(120);
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
    completed.terminal_at = Some(1_700_000_075);
    completed.actual_elapsed_seconds = Some(75);
    completed.accuracy = EtaAccuracy::Within;
    let mut cancelled = task(
        "cancelled-task",
        &root,
        None,
        "Optional cleanup",
        EtaTaskStatus::Cancelled,
    );
    cancelled.terminal_at = Some(1_700_000_012);
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

fn session_task(
    task_id: &str,
    root_thread_id: &str,
    parent_task_id: Option<&str>,
    title: &str,
    nested_task_count: u32,
) -> EtaSessionTask {
    EtaSessionTask {
        task_id: task_id.to_string(),
        root_thread_id: root_thread_id.to_string(),
        parent_task_id: parent_task_id.map(str::to_string),
        title: title.to_string(),
        status: EtaTaskStatus::Active,
        current_lower_seconds: Some(30),
        current_upper_seconds: Some(90),
        session: EtaSessionInfo {
            thread_id: root_thread_id.to_string(),
            title: "Release session".to_string(),
            name: Some("release".to_string()),
            preview: Some("Prepare the release artifacts".to_string()),
            created_at: 1_699_999_900,
            updated_at: 1_700_000_030,
            archived_at: None,
            cwd: "/tmp/release".to_string(),
        },
        nested_task_count,
        active_nested_task_count: nested_task_count,
        nested_lower_seconds: Some(60),
        nested_upper_seconds: Some(120),
    }
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

fn all_sessions_view() -> EtaView {
    let root = "00000000-0000-0000-0000-000000000001";
    let second_root = "00000000-0000-0000-0000-000000000003";
    let rows = vec![
        session_task("root", root, None, "Prepare release", 1),
        session_task("child", root, Some("root"), "Run checks", 0),
        session_task("other", second_root, None, "Publish notes", 0),
    ];
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    EtaView::new_with_state_and_all_sessions(
        snapshot(),
        RuntimeKeymap::defaults().list,
        AppEventSender::new(tx),
        "all-sessions",
        None,
        EtaTimestampFormatter::utc(),
        rows,
        None,
        true,
        false,
        ThreadId::from_string(root).ok(),
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
fn expanded_history_renders_lifecycle_timestamps() {
    let mut view = view();
    view.handle_key_event(KeyCode::Right.into());
    view.handle_key_event(KeyCode::Enter.into());
    let lifecycle_lines = render(&view, 96, 24)
        .lines()
        .filter(|line| line.contains("Started:") || line.contains("Ended:"))
        .map(|line| line.trim_matches([' ', '"']))
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("eta_history_lifecycle_timestamps", lifecycle_lines);
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
fn history_renders_named_timezone_timestamps() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut view = EtaView::new_with_state_and_timestamp_formatter(
        snapshot(),
        RuntimeKeymap::defaults().list,
        AppEventSender::new(tx),
        "history",
        None,
        EtaTimestampFormatter::with_timezone(
            TimeZone::get("America/New_York").expect("known time zone"),
        ),
    );
    view.handle_key_event(KeyCode::Enter.into());
    insta::assert_snapshot!("eta_history_named_timezone", render(&view, 96, 24));
}

#[test]
fn active_remaining_uses_server_remaining_range() {
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

#[test]
fn all_sessions_are_grouped_and_nested_rows_toggle() {
    let mut view = all_sessions_view();
    insta::assert_debug_snapshot!(view.ordered_session_indices(), @r###"[0, 1, 2]"###);
    let rendered = render(&view, 112, 24);
    assert!(rendered.contains("Session: release"));
    assert!(rendered.contains("[1 nested]"));
    view.handle_key_event(KeyCode::Enter.into());
    assert!(!render(&view, 112, 24).contains("Run checks"));
    view.handle_key_event(KeyCode::Enter.into());
    assert!(render(&view, 112, 24).contains("Run checks"));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    view.app_event_tx = AppEventSender::new(tx);
    view.handle_key_event(KeyCode::Char('r').into());
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::ResumeEtaSessionTarget { target })
            if target.thread_id == ThreadId::from_string("00000000-0000-0000-0000-000000000001").unwrap()
    ));
}

#[test]
fn all_sessions_loading_error_and_empty_states_have_snapshots() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let make = |in_flight, error| {
        EtaView::new_with_state_and_all_sessions_and_status(
            snapshot(),
            RuntimeKeymap::defaults().list,
            AppEventSender::new(tx.clone()),
            "all-sessions",
            None,
            EtaTimestampFormatter::utc(),
            Vec::new(),
            None,
            false,
            in_flight,
            None,
            false,
            None,
            error,
            None,
        )
    };
    for (label, view) in [
        ("loading", make(true, None)),
        ("error", make(false, Some("server unavailable".to_string()))),
        ("empty", make(false, None)),
    ] {
        let rendered = render(&view, 80, 16);
        let line = rendered
            .lines()
            .map(str::trim)
            .find(|line| {
                line.contains("Loading retained")
                    || line.contains("Unable to load retained")
                    || line.contains("No retained ETA")
            })
            .expect("inline all-sessions status");
        match label {
            "loading" => insta::assert_snapshot!(line, @"Loading retained sessions…"),
            "error" => {
                insta::assert_snapshot!(line, @"Unable to load retained sessions: server unavailable · press r to retry")
            }
            "empty" => insta::assert_snapshot!(line, @"No retained ETA sessions found."),
            _ => unreachable!("unknown ETA status snapshot"),
        }
    }
}

#[test]
fn all_sessions_nested_and_narrow_layout_have_snapshots() {
    let view = all_sessions_view();
    let rows = render(&view, 112, 24)
        .lines()
        .map(str::trim)
        .filter(|line| line.contains("Prepare release") || line.contains("Run checks"))
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("eta_all_sessions_nested_rows", rows);
    insta::assert_debug_snapshot!(
        "eta_all_sessions_narrow_columns",
        super::eta_view_render::session_column_widths(48),
        @r###"(8, 8, 8, 10)"###
    );
}
