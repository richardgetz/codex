//! Live user and assistant transcript cells for realtime voice.

use super::*;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RealtimeTranscriptRole {
    User,
    Assistant,
}

impl RealtimeTranscriptRole {
    pub(crate) fn from_wire(role: &str) -> Option<Self> {
        match role {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }

    fn prefix(self) -> Line<'static> {
        match self {
            Self::User => "› ".bold().dim().into(),
            Self::Assistant => "• ".dim().into(),
        }
    }

    fn style(self) -> Style {
        match self {
            Self::User => user_message_style(),
            Self::Assistant => Style::default(),
        }
    }
}

/// A mutable history cell used while GPT-Live is sending a transcript part.
#[derive(Debug)]
pub(crate) struct RealtimeTranscriptCell {
    role: RealtimeTranscriptRole,
    text: Mutex<String>,
}

impl RealtimeTranscriptCell {
    pub(crate) fn new(role: RealtimeTranscriptRole) -> Self {
        Self {
            role,
            text: Mutex::new(String::new()),
        }
    }

    pub(crate) fn role(&self) -> RealtimeTranscriptRole {
        self.role
    }

    pub(crate) fn append(&self, delta: &str) {
        match self.text.lock() {
            Ok(mut text) => text.push_str(delta),
            Err(poisoned) => poisoned.into_inner().push_str(delta),
        }
    }

    pub(crate) fn set_text(&self, text: &str) {
        match self.text.lock() {
            Ok(mut current) => *current = text.to_string(),
            Err(poisoned) => *poisoned.into_inner() = text.to_string(),
        }
    }

    fn text(&self) -> String {
        match self.text.lock() {
            Ok(text) => text.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl HistoryCell for RealtimeTranscriptCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let text = sanitize_user_text(&self.text());
        if text.is_empty() {
            return Vec::new();
        }

        let style = self.role.style();
        adaptive_wrap_lines(
            text.trim_end_matches(['\r', '\n'])
                .split('\n')
                .map(|line| Line::from(line.to_string()).style(style)),
            RtOptions::new(width.saturating_sub(3).max(1) as usize)
                .initial_indent(self.role.prefix())
                .subsequent_indent("  ".into()),
        )
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        raw_lines_from_source(sanitize_user_text(&self.text()).trim_end_matches(['\r', '\n']))
    }

    fn has_stable_transcript_height(&self) -> bool {
        false
    }
}
