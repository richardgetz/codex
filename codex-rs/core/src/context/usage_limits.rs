use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsageLimitsContext {
    body: String,
}

impl UsageLimitsContext {
    pub(crate) fn new(body: String) -> Self {
        Self { body }
    }
}

impl ContextualUserFragment for UsageLimitsContext {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("usage_limits.status".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<thread_usage_limits>\n", "\n</thread_usage_limits>")
    }

    fn body(&self) -> String {
        self.body.clone()
    }
}
