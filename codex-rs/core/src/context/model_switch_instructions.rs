use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelSwitchInstructions {
    model_instructions: String,
}

impl ModelSwitchInstructions {
    pub(crate) fn new(model_instructions: impl Into<String>) -> Self {
        let model_instructions = model_instructions.into();
        Self {
            model_instructions: crate::context::truncate_model_catalog_context(&model_instructions),
        }
    }
}

impl ContextualUserFragment for ModelSwitchInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("model_switch.instructions".to_string())
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
        ("<model_switch>", "</model_switch>")
    }

    fn body(&self) -> String {
        format!(
            "\nThe user was previously using a different model. Please continue the conversation according to the following instructions:\n\n{}\n",
            self.model_instructions
        )
    }
}
