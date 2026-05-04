use super::ContextualUserFragment;

const CONVENTIONAL_COMMITS_INSTRUCTIONS_OPEN_TAG: &str = "<conventional_commits>";
const CONVENTIONAL_COMMITS_INSTRUCTIONS_CLOSE_TAG: &str = "</conventional_commits>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConventionalCommitsInstructions;

impl ConventionalCommitsInstructions {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl ContextualUserFragment for ConventionalCommitsInstructions {
    const ROLE: &'static str = "developer";
    const START_MARKER: &'static str = CONVENTIONAL_COMMITS_INSTRUCTIONS_OPEN_TAG;
    const END_MARKER: &'static str = CONVENTIONAL_COMMITS_INSTRUCTIONS_CLOSE_TAG;

    fn body(&self) -> String {
        "\n## Conventional Commits\n\
When creating or reviewing git commit messages, use Conventional Commits.\n\
- Use `type(scope): short imperative summary`; omit the scope only when no narrow useful scope exists.\n\
- Prefer these types: `feat`, `fix`, `docs`, `refactor`, `test`, `build`, `ci`, `perf`, `chore`, `revert`.\n\
- Choose the type by user-visible behavior, not implementation effort.\n\
- For breaking changes, use both `type(scope)!: summary` and a `BREAKING CHANGE:` footer that explains impact and migration.\n"
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_mentions_required_commit_shape() {
        let body = ConventionalCommitsInstructions::new().body();

        assert!(body.contains("type(scope): short imperative summary"));
        assert!(body.contains("BREAKING CHANGE:"));
    }
}
