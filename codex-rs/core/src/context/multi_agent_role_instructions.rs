use super::ContextualUserFragment;
use codex_protocol::protocol::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

const MULTI_AGENT_ROLE_INSTRUCTIONS_MAX_TOKENS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MultiAgentRoleInstructions {
    text: String,
    marked: bool,
}

impl MultiAgentRoleInstructions {
    pub(crate) fn unmarked(text: impl Into<String>) -> Self {
        Self {
            text: bounded_text(text.into()),
            marked: false,
        }
    }

    pub(crate) fn catalog(text: impl Into<String>) -> Self {
        Self {
            text: bounded_text(text.into()),
            marked: true,
        }
    }
}

fn bounded_text(text: String) -> String {
    truncate_text(
        &text,
        TruncationPolicy::Tokens(MULTI_AGENT_ROLE_INSTRUCTIONS_MAX_TOKENS),
    )
}

impl ContextualUserFragment for MultiAgentRoleInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        if self.marked {
            Self::type_markers()
        } else {
            ("", "")
        }
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<multi_agent_role>", "</multi_agent_role>")
    }

    fn body(&self) -> String {
        self.text.clone()
    }
}

#[cfg(test)]
#[path = "multi_agent_role_instructions_tests.rs"]
mod tests;
