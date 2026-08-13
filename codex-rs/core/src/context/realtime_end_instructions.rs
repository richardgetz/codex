use super::ContextualUserFragment;
use codex_prompts::END_INSTRUCTIONS;
use codex_protocol::protocol::REALTIME_CONVERSATION_CLOSE_TAG;
use codex_protocol::protocol::REALTIME_CONVERSATION_OPEN_TAG;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RealtimeEndInstructions {
    instructions: Option<String>,
    reason: Option<String>,
}

impl RealtimeEndInstructions {
    pub(crate) fn new() -> Self {
        Self {
            instructions: None,
            reason: None,
        }
    }

    pub(crate) fn with_instructions(instructions: impl Into<String>) -> Self {
        Self {
            instructions: Some(instructions.into()),
            reason: None,
        }
    }

    pub(crate) fn with_reason(reason: impl Into<String>) -> Self {
        Self {
            instructions: None,
            reason: Some(reason.into()),
        }
    }
}

impl ContextualUserFragment for RealtimeEndInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            REALTIME_CONVERSATION_OPEN_TAG,
            REALTIME_CONVERSATION_CLOSE_TAG,
        )
    }

    fn body(&self) -> String {
        let instructions = self
            .instructions
            .as_deref()
            .unwrap_or_else(|| END_INSTRUCTIONS.trim());
        match &self.reason {
            Some(reason) => format!("\n{instructions}\n\nReason: {reason}\n"),
            None => format!("\n{instructions}\n"),
        }
    }
}
