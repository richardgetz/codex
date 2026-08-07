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
        let Some(cell) = self.realtime_transcript_cell(role) else {
            return;
        };
        cell.append(delta);
        self.bump_active_cell_revision();
        self.request_redraw();
    }

    pub(super) fn handle_realtime_transcript_done(&mut self, role: &str, text: &str) {
        let Some(role) = RealtimeTranscriptRole::from_wire(role) else {
            return;
        };
        if text.is_empty() && self.realtime_transcript_cell(role).is_none() {
            return;
        }

        self.ensure_realtime_transcript_cell(role);
        if !text.is_empty() {
            if let Some(cell) = self.realtime_transcript_cell(role) {
                cell.set_text(text);
            }
        }
        self.bump_active_cell_revision();
        self.flush_realtime_transcript_cell(role);
        self.request_redraw();
    }

    pub(super) fn finish_realtime_transcript_stream(&mut self) {
        if self.transcript.realtime_user_transcript_cell.is_some()
            || self.transcript.realtime_assistant_transcript_cell.is_some()
        {
            self.flush_realtime_transcript_cell(RealtimeTranscriptRole::User);
            self.flush_realtime_transcript_cell(RealtimeTranscriptRole::Assistant);
            self.request_redraw();
        }
    }

    fn ensure_realtime_transcript_cell(&mut self, role: RealtimeTranscriptRole) {
        if self.realtime_transcript_cell(role).is_some() {
            return;
        }

        if self.transcript.realtime_user_transcript_cell.is_none()
            && self.transcript.realtime_assistant_transcript_cell.is_none()
            && self.stream_controller.is_none()
            && self.plan_stream_controller.is_none()
        {
            // The session header and other non-streaming active cells should remain before the
            // realtime transcript, but never interrupt an in-flight normal assistant stream.
            self.flush_active_cell();
        }
        *self.realtime_transcript_cell_mut(role) =
            Some(crate::history_cell::RealtimeTranscriptCell::new(role));
        self.bump_active_cell_revision();
    }

    fn realtime_transcript_cell(
        &self,
        role: RealtimeTranscriptRole,
    ) -> Option<&crate::history_cell::RealtimeTranscriptCell> {
        match role {
            RealtimeTranscriptRole::User => self.transcript.realtime_user_transcript_cell.as_ref(),
            RealtimeTranscriptRole::Assistant => {
                self.transcript.realtime_assistant_transcript_cell.as_ref()
            }
        }
    }

    fn realtime_transcript_cell_mut(
        &mut self,
        role: RealtimeTranscriptRole,
    ) -> &mut Option<crate::history_cell::RealtimeTranscriptCell> {
        match role {
            RealtimeTranscriptRole::User => &mut self.transcript.realtime_user_transcript_cell,
            RealtimeTranscriptRole::Assistant => {
                &mut self.transcript.realtime_assistant_transcript_cell
            }
        }
    }

    fn flush_realtime_transcript_cell(&mut self, role: RealtimeTranscriptRole) {
        let Some(cell) = self.realtime_transcript_cell_mut(role).take() else {
            return;
        };
        self.transcript.bump_active_cell_revision();
        self.transcript.needs_final_message_separator = true;
        self.app_event_tx
            .send(AppEvent::InsertHistoryCell(Box::new(cell)));
        self.request_pending_usage_output_insertion();
    }
}
