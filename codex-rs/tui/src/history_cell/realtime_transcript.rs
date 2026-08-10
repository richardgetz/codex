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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RealtimeTranscriptHistoryEntry {
    role: RealtimeTranscriptRole,
    text: String,
}

impl RealtimeTranscriptHistoryEntry {
    pub(crate) fn new(role: RealtimeTranscriptRole, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
        }
    }

    pub(crate) fn is_same_transcript(&self, role: RealtimeTranscriptRole, text: &str) -> bool {
        self.role == role && self.text == text
    }
}

/// A dynamically wrapped view of recent GPT-Live transcript entries.
#[derive(Debug)]
pub(crate) struct RealtimeTranscriptHistoryCell {
    entries: Vec<RealtimeTranscriptHistoryEntry>,
}

impl RealtimeTranscriptHistoryCell {
    pub(crate) fn new(entries: Vec<RealtimeTranscriptHistoryEntry>) -> Self {
        Self { entries }
    }
}

impl HistoryCell for RealtimeTranscriptHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for entry in &self.entries {
            let text = sanitize_user_text(&entry.text);
            if text.is_empty() {
                continue;
            }
            lines.extend(adaptive_wrap_lines(
                text.trim_end_matches(['\r', '\n'])
                    .split('\n')
                    .map(|line| Line::from(line.to_string()).style(entry.role.style())),
                RtOptions::new(width.saturating_sub(3).max(1) as usize)
                    .initial_indent(entry.role.prefix())
                    .subsequent_indent("  ".into()),
            ));
        }
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        self.entries
            .iter()
            .flat_map(|entry| raw_lines_from_source(sanitize_user_text(&entry.text).as_str()))
            .collect()
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

    pub(crate) fn append(&self, delta: &str) {
        match self.text.lock() {
            Ok(mut text) => append_transcript_delta(&mut text, delta),
            Err(poisoned) => append_transcript_delta(&mut poisoned.into_inner(), delta),
        }
    }

    pub(crate) fn set_text(&self, text: &str) {
        match self.text.lock() {
            Ok(mut current) => *current = text.to_string(),
            Err(poisoned) => *poisoned.into_inner() = text.to_string(),
        }
    }

    pub(crate) fn text(&self) -> String {
        match self.text.lock() {
            Ok(text) => text.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

fn append_transcript_delta(text: &mut String, delta: &str) {
    if delta.is_empty() || text == delta {
        return;
    }
    if delta.starts_with(text.as_str()) {
        *text = delta.to_string();
        return;
    }
    if text.ends_with(delta) {
        return;
    }
    text.push_str(delta);
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
