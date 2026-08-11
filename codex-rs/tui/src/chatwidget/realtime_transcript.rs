//! TUI handling for GPT-Live transcript notifications.

use super::*;
use crate::history_cell::RealtimeTranscriptHistoryCell;
use crate::history_cell::RealtimeTranscriptHistoryEntry;
use crate::history_cell::RealtimeTranscriptRole;

const DEFAULT_REALTIME_HISTORY_LIMIT: usize = 5;
const MAX_REALTIME_HISTORY_LIMIT: usize = 20;
const MAX_REALTIME_HISTORY_ENTRIES: usize = 40;

impl ChatWidget {
    pub(super) fn handle_realtime_handoff_item(&mut self, item: &serde_json::Value) {
        if self.config.realtime.enable_preambles
            || item.get("type").and_then(serde_json::Value::as_str) != Some("handoff_request")
        {
            return;
        }

        let handoff_id = item
            .get("handoff_id")
            .or_else(|| item.get("item_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let duplicate = match (
            self.transcript
                .realtime_preamble_suppression_handoff_id
                .as_deref(),
            handoff_id.as_deref(),
        ) {
            (Some(previous), Some(current)) => previous == current,
            (None, None) => self.transcript.realtime_preamble_suppression_active,
            _ => false,
        };
        if duplicate {
            return;
        }

        self.transcript.realtime_preamble_suppression_active = true;
        self.transcript
            .realtime_preamble_commentary_item_ids
            .clear();
        self.transcript.realtime_preamble_suppression_handoff_id = handoff_id;
        self.bump_active_cell_revision();
    }

    pub(crate) fn realtime_preamble_suppression_active(&self) -> bool {
        self.transcript.realtime_preamble_suppression_active
    }

    pub(super) fn reset_realtime_preamble_suppression(&mut self) {
        self.transcript.realtime_preamble_suppression_active = false;
        self.transcript
            .realtime_preamble_commentary_item_ids
            .clear();
    }

    pub(super) fn clear_realtime_preamble_suppression_history(&mut self) {
        self.reset_realtime_preamble_suppression();
        self.transcript.realtime_preamble_suppression_handoff_id = None;
    }

    pub(super) fn handle_realtime_agent_item_started(
        &mut self,
        item: &codex_app_server_protocol::ThreadItem,
    ) {
        if !self.transcript.realtime_preamble_suppression_active {
            return;
        }
        let codex_app_server_protocol::ThreadItem::AgentMessage { id, phase, .. } = item else {
            return;
        };
        match phase {
            Some(codex_protocol::models::MessagePhase::Commentary) => {
                self.transcript
                    .realtime_preamble_commentary_item_ids
                    .insert(id.clone());
            }
            Some(codex_protocol::models::MessagePhase::FinalAnswer) | None => {
                self.reset_realtime_preamble_suppression();
            }
        }
    }

    pub(super) fn handle_realtime_agent_item_completed(
        &mut self,
        item_id: &str,
        phase: Option<&codex_protocol::models::MessagePhase>,
    ) -> bool {
        match phase {
            Some(codex_protocol::models::MessagePhase::Commentary)
                if self.transcript.realtime_preamble_suppression_active =>
            {
                self.transcript
                    .realtime_preamble_commentary_item_ids
                    .remove(item_id);
                true
            }
            Some(codex_protocol::models::MessagePhase::FinalAnswer) | None => {
                self.reset_realtime_preamble_suppression();
                false
            }
            _ => false,
        }
    }

    pub(super) fn handle_realtime_transcript_delta(&mut self, role: &str, delta: &str) {
        if role == "assistant" && self.realtime_preamble_suppression_active() {
            return;
        }
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
        if role == "assistant" && self.realtime_preamble_suppression_active() {
            return;
        }
        let Some(role) = RealtimeTranscriptRole::from_wire(role) else {
            return;
        };
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

    pub(super) fn finish_realtime_transcript_stream(&mut self) {
        if self.transcript.realtime_user_transcript_cell.is_some()
            || self.transcript.realtime_assistant_transcript_cell.is_some()
        {
            self.flush_realtime_transcript_cell(RealtimeTranscriptRole::User);
            self.flush_realtime_transcript_cell(RealtimeTranscriptRole::Assistant);
            self.request_redraw();
        }
        self.reset_realtime_preamble_suppression();
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
