use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

const GIT_INTENT_NOTES_INSTRUCTIONS_OPEN_TAG: &str = "<git_intent_notes>";
const GIT_INTENT_NOTES_INSTRUCTIONS_CLOSE_TAG: &str = "</git_intent_notes>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitIntentNotesInstructions;

impl GitIntentNotesInstructions {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl ContextualUserFragment for GitIntentNotesInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("git.intent_notes".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        (
            GIT_INTENT_NOTES_INSTRUCTIONS_OPEN_TAG,
            GIT_INTENT_NOTES_INSTRUCTIONS_CLOSE_TAG,
        )
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            GIT_INTENT_NOTES_INSTRUCTIONS_OPEN_TAG,
            GIT_INTENT_NOTES_INSTRUCTIONS_CLOSE_TAG,
        )
    }

    fn body(&self) -> String {
        "\n## Git Intent Notes\n\
For code changes, preserve the why in git notes under `refs/notes/intention` instead of overloading commit messages.\n\
- Before behavior, API, invariant, or public-contract changes, look up related intent notes. Prefer the `git-intent-notes` MCP (`find_related_intent_notes` with an explicit `workdir`) when available; otherwise use `git log --show-notes=refs/notes/intention` scoped to relevant paths.\n\
- If a must-level note conflicts with the planned change, ask the user before proceeding.\n\
- When committing, attach one intent note to the final feature/fix commit unless the user asks for per-commit notes. Prefer MCP `validate_intent_note` and `add_intent_note`; fall back to `git notes --ref=refs/notes/intention` only when the MCP is unavailable.\n\
- Use this minimal YAML shape: `intent.id`, `change_type`, `scope`, `summary`, `decision`, `constraints`, `intent_priority`, `code_locations`, `requested_by`, and `recorded_by`.\n\
- Do not record secrets or sensitive/private details in intent notes; ask first if sensitivity is unclear.\n"
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_mentions_intention_ref_and_mcp() {
        let body = GitIntentNotesInstructions::new().body();

        assert!(body.contains("refs/notes/intention"));
        assert!(body.contains("find_related_intent_notes"));
        assert!(body.contains("validate_intent_note"));
    }
}
