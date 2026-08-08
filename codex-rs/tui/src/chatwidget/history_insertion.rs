//! History insertion helpers shared by queued and synchronous UI updates.

use super::*;

impl ChatWidget {
    pub(super) fn collect_history_cells_for_insertion(
        &mut self,
        cell: Box<dyn HistoryCell>,
    ) -> Vec<Box<dyn HistoryCell>> {
        // Keep the placeholder session header as the active cell until real session info arrives,
        // so we can merge headers instead of committing a duplicate box to history.
        let keep_placeholder_header_active = !self.is_session_configured()
            && self.transcript.active_cell.as_ref().is_some_and(|active| {
                active
                    .as_any()
                    .is::<history_cell::SessionHeaderHistoryCell>()
            });

        let mut cells = Vec::new();
        if !keep_placeholder_header_active && !cell.display_lines(u16::MAX).is_empty() {
            // Only break exec grouping if the cell renders visible lines.
            if !self.has_active_stream_tail()
                && let Some(active) = self.transcript.active_cell.take()
            {
                self.transcript.needs_final_message_separator = true;
                cells.push(active);
                self.request_pending_usage_output_insertion();
            }
            self.transcript.needs_final_message_separator = true;
        }
        cells.push(cell);
        cells
    }

    pub(crate) fn prepare_immediate_info_message(
        &mut self,
        message: String,
    ) -> Vec<Box<dyn HistoryCell>> {
        let cells = self.collect_history_cells_for_insertion(Box::new(
            crate::history_cell::new_info_event(message, None),
        ));
        self.request_pending_usage_output_insertion();
        self.request_redraw();
        cells
    }
}
