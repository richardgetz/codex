//! TUI handling for GPT-Live transcript notifications.

use super::*;
use crate::history_cell::RealtimeTranscriptRole;

impl ChatWidget {
    pub(super) fn handle_realtime_transcript_delta(&mut self, role: &str, delta: &str) {
        let Some(role) = RealtimeTranscriptRole::from_wire(role) else {
            return;
        };
        if delta.is_empty() {
            return;
        }

        self.ensure_realtime_transcript_cell(role);
        if let Some(cell) = self.transcript.realtime_transcript_cell.as_ref() {
            cell.append(delta);
            self.bump_active_cell_revision();
            self.request_redraw();
        }
    }

    pub(super) fn handle_realtime_transcript_done(&mut self, role: &str, text: &str) {
        let Some(role) = RealtimeTranscriptRole::from_wire(role) else {
            return;
        };
        if text.is_empty()
            && !self
                .transcript
                .realtime_transcript_cell
                .as_ref()
                .is_some_and(|cell| cell.role() == role)
        {
            return;
        }

        self.ensure_realtime_transcript_cell(role);
        if let Some(cell) = self.transcript.realtime_transcript_cell.as_ref() {
            if !text.is_empty() {
                cell.set_text(text);
            }
            self.bump_active_cell_revision();
            self.flush_realtime_transcript_cell();
            self.request_redraw();
        }
    }

    pub(super) fn finish_realtime_transcript_stream(&mut self) {
        if self.transcript.realtime_transcript_cell.is_some() {
            self.flush_realtime_transcript_cell();
            self.request_redraw();
        }
    }

    fn ensure_realtime_transcript_cell(&mut self, role: RealtimeTranscriptRole) {
        let has_matching_cell = self
            .transcript
            .realtime_transcript_cell
            .as_ref()
            .is_some_and(|cell| cell.role() == role);
        if has_matching_cell {
            return;
        }

        if self.transcript.realtime_transcript_cell.is_some() {
            self.flush_realtime_transcript_cell();
        } else if self.stream_controller.is_none() && self.plan_stream_controller.is_none() {
            // The session header and other non-streaming active cells should remain before the
            // realtime transcript, but never interrupt an in-flight normal assistant stream.
            self.flush_active_cell();
        }
        self.transcript.realtime_transcript_cell =
            Some(crate::history_cell::RealtimeTranscriptCell::new(role));
        self.bump_active_cell_revision();
    }

    fn flush_realtime_transcript_cell(&mut self) {
        let Some(cell) = self.transcript.realtime_transcript_cell.take() else {
            return;
        };
        self.transcript.bump_active_cell_revision();
        self.transcript.needs_final_message_separator = true;
        self.app_event_tx
            .send(AppEvent::InsertHistoryCell(Box::new(cell)));
        self.request_pending_usage_output_insertion();
    }
}
