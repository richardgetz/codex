use super::*;
use crate::app::event_dispatch::EMBEDDED_RELOAD_PREPARED_HINT;
use crate::app::event_dispatch::EMBEDDED_RELOAD_PREPARED_MESSAGE;
use crate::history_cell;
use crate::history_cell::HistoryCell;

#[test]
fn desktop_thread_opened_history_snapshot() {
    let cell = history_cell::new_info_event(
        DESKTOP_THREAD_OPENED_MESSAGE.to_string(),
        /*hint*/ None,
    );

    insta::assert_snapshot!("desktop_thread_opened_history", render_cell(&cell));
}

#[test]
fn desktop_thread_open_error_history_snapshot() {
    let cell = history_cell::new_error_event(desktop_thread_open_error_message("launch failed"));

    insta::assert_snapshot!("desktop_thread_open_error_history", render_cell(&cell));
}

#[test]
fn embedded_reload_prepared_history_snapshot() {
    let cell = history_cell::new_info_event(
        EMBEDDED_RELOAD_PREPARED_MESSAGE.to_string(),
        Some(EMBEDDED_RELOAD_PREPARED_HINT.to_string()),
    );

    insta::assert_snapshot!("embedded_reload_prepared_history", render_cell(&cell));
}

fn render_cell(cell: &impl HistoryCell) -> String {
    let lines = cell.display_lines(/*width*/ 80);
    lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
