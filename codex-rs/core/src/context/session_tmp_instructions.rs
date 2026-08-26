use super::ContextualUserFragment;

const SESSION_TMP_INSTRUCTIONS_OPEN_TAG: &str = "<session_tmp_instructions>";
const SESSION_TMP_INSTRUCTIONS_CLOSE_TAG: &str = "</session_tmp_instructions>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionTmpInstructions;

impl ContextualUserFragment for SessionTmpInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        (
            SESSION_TMP_INSTRUCTIONS_OPEN_TAG,
            SESSION_TMP_INSTRUCTIONS_CLOSE_TAG,
        )
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            SESSION_TMP_INSTRUCTIONS_OPEN_TAG,
            SESSION_TMP_INSTRUCTIONS_CLOSE_TAG,
        )
    }

    fn body(&self) -> String {
        "\n## Session Temporary Storage\n\
The built-in `session_tmp` namespace provides persistent lineage for opt-in temporary files.\n\
- Treat this agent directory as disposable scratch space. Every file or directory under it, including untracked paths created directly by shell commands, is eligible for removal. Never store source files, user deliverables, checkpoints, credentials, or anything the user expects to keep there.\n\
- Put durable artifacts in the workspace or another explicitly persistent user-selected location. Only use `manual` retention when the user explicitly wants a temporary artifact to survive normal session cleanup; `/tmp clear` and stale-session reap can still remove it.\n\
- Use `session_tmp.create` for new files or directories, and provide a concise `purpose`.\n\
- Use `session_tmp.register` after a shell command creates a path directly; do not register paths outside the current agent temporary directory.\n\
- Session-retained entries are cleaned when the root session ends. Use manual retention only when the user needs the artifact to survive normal cleanup, and set a purpose that explains why.\n\
- For local shell and exec commands, the shell `TMPDIR`, `TMP`, and `TEMP` variables point to this agent's isolated directory. Remote executors keep their own temporary environment. The configured parent is not itself a writable cleanup target.\n\
- Use `session_tmp.list` to inspect lineage and `session_tmp.retain` to change retention for an entry owned by this agent.\n".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_describes_lineage_and_agent_scope() {
        let body = SessionTmpInstructions.body();

        assert!(body.contains("session_tmp.create"));
        assert!(body.contains("session_tmp.register"));
        assert!(body.contains("TMPDIR`"));
        assert!(body.contains("configured parent is not itself a writable cleanup target"));
    }
}
