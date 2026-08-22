//! TUI handling for GPT-Live transcript notifications.

use super::*;
use crate::history_cell::RealtimeTranscriptHistoryCell;
use crate::history_cell::RealtimeTranscriptHistoryEntry;
use crate::history_cell::RealtimeTranscriptRole;

const DEFAULT_REALTIME_HISTORY_LIMIT: usize = 5;
const MAX_REALTIME_HISTORY_LIMIT: usize = 20;
const MAX_REALTIME_HISTORY_ENTRIES: usize = 40;

impl ChatWidget {
    pub(super) fn handle_realtime_item_added(&mut self, item: &serde_json::Value) {
        if self.config.realtime.enable_preambles
            || item.get("type").and_then(serde_json::Value::as_str) != Some("handoff_request")
        {
            return;
        }

        self.transcript.realtime_handoff_output_suppressed = true;
        self.transcript.realtime_assistant_transcript_cell = None;
        self.transcript.bump_active_cell_revision();
    }

    pub(super) fn release_realtime_handoff_output(&mut self) {
        self.transcript.realtime_handoff_output_suppressed = false;
    }

    pub(super) fn handle_realtime_transcript_delta(&mut self, role: &str, delta: &str) {
        let Some(role) = RealtimeTranscriptRole::from_wire(role) else {
            return;
        };
        if matches!(role, RealtimeTranscriptRole::Assistant)
            && self.transcript.realtime_handoff_output_suppressed
        {
            return;
        }
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
        if matches!(role, RealtimeTranscriptRole::Assistant)
            && self.transcript.realtime_handoff_output_suppressed
        {
            return;
        }
        let active_text = self
            .realtime_transcript_cell(role)
            .map(crate::history_cell::RealtimeTranscriptCell::text);
        if text.is_empty() && active_text.is_none() {
            return;
        }
        if active_text.is_none()
            && self
                .transcript
                .realtime_history
                .back()
                .is_some_and(|entry| entry.is_same_transcript(role, text.trim()))
        {
            return;
        }

        self.ensure_realtime_transcript_cell(role);
        if !text.is_empty()
            && let Some(cell) = self.realtime_transcript_cell(role)
        {
            cell.set_text(text);
        }
        let completed_text = if text.is_empty() {
            active_text.as_deref().unwrap_or_default()
        } else {
            text
        };
        self.record_realtime_transcript(role, completed_text);
        self.bump_active_cell_revision();
        self.flush_realtime_transcript_cell(role);
        self.request_redraw();
    }

    pub(super) fn add_realtime_history_output(&mut self, requested_limit: Option<usize>) {
        let limit = requested_limit
            .unwrap_or(DEFAULT_REALTIME_HISTORY_LIMIT)
            .clamp(1, MAX_REALTIME_HISTORY_LIMIT);
        let entries = self
            .transcript
            .realtime_history
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();

        if entries.is_empty() {
            self.add_info_message(
                "No completed GPT-Live transcript entries yet.".to_string(),
                Some("Use /voice history [count] after speaking in realtime.".to_string()),
            );
            return;
        }

        let count = entries.len();
        let header = crate::history_cell::PlainHistoryCell::new(vec![
            vec![
                "• ".dim(),
                format!("Recent GPT-Live transcript ({count} entries)").bold(),
            ]
            .into(),
        ]);
        self.add_to_history(crate::history_cell::CompositeHistoryCell::new(vec![
            Box::new(header),
            Box::new(RealtimeTranscriptHistoryCell::new(
                entries.into_iter().rev().collect(),
            )),
        ]));
        self.request_redraw();
    }

    fn record_realtime_transcript(&mut self, role: RealtimeTranscriptRole, text: &str) {
        let text = text.trim();
        if text.is_empty()
            || self
                .transcript
                .realtime_history
                .back()
                .is_some_and(|entry| entry.is_same_transcript(role, text))
        {
            return;
        }
        self.transcript
            .realtime_history
            .push_back(RealtimeTranscriptHistoryEntry::new(role, text));
        while self.transcript.realtime_history.len() > MAX_REALTIME_HISTORY_ENTRIES {
            self.transcript.realtime_history.pop_front();
        }
    }

    pub(crate) fn clear_realtime_transcript(&mut self) {
        let had_user_transcript = self
            .transcript
            .realtime_user_transcript_cell
            .take()
            .is_some();
        let had_assistant_transcript = self
            .transcript
            .realtime_assistant_transcript_cell
            .take()
            .is_some();
        if had_user_transcript || had_assistant_transcript {
            self.transcript.bump_active_cell_revision();
            self.request_redraw();
        }
    }

    pub(super) fn finish_realtime_transcript_stream(&mut self) {
        self.clear_realtime_transcript();
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
