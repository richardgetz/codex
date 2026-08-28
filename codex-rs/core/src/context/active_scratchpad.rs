use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

const ACTIVE_SCRATCHPAD_CONTEXT_MAX_TOKENS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveScratchpadContext {
    scratchpad_id: String,
    summary_text: String,
}

impl ActiveScratchpadContext {
    pub(crate) fn new(scratchpad_id: String, summary_text: &str) -> Self {
        Self {
            scratchpad_id,
            summary_text: truncate_text(
                summary_text,
                TruncationPolicy::Tokens(ACTIVE_SCRATCHPAD_CONTEXT_MAX_TOKENS),
            ),
        }
    }
}

impl ContextualUserFragment for ActiveScratchpadContext {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("scratchpad.active_context".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<active_scratchpad>", "</active_scratchpad>")
    }

    fn body(&self) -> String {
        let scratchpad_id = &self.scratchpad_id;
        let summary_text = &self.summary_text;
        format!(
            "\nThe built-in scratchpad for this thread/session is `{scratchpad_id}`. Continue using this scratchpad id for recovery notes, next steps, waits, and durable working state.\n\n```json\n{summary_text}\n```\n"
        )
    }
}
