use super::ContextualUserFragment;

const FORK_DELTA_INVENTORY: &str = include_str!("../../templates/fork/delta_inventory.md");
const FORK_HELP_OPEN_TAG: &str = "<fork_help>";
const FORK_HELP_CLOSE_TAG: &str = "</fork_help>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForkHelpInstructions;

impl ForkHelpInstructions {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl ContextualUserFragment for ForkHelpInstructions {
    const ROLE: &'static str = "developer";
    const START_MARKER: &'static str = FORK_HELP_OPEN_TAG;
    const END_MARKER: &'static str = FORK_HELP_CLOSE_TAG;

    fn body(&self) -> String {
        format!(
            "\n## Rick Fork Guidance\n\
This build includes fork-only behavior beyond upstream/mainline Codex.\n\
Use the inventory below as the source of truth when the user asks:\n\
- what is new in Rick's version or in this fork,\n\
- which fork-only slash commands, defaults, or configs exist,\n\
- how this fork differs from upstream/mainline,\n\
- what should be regression-checked after rebasing or merging from upstream.\n\n\
When answering \"what's new\", prioritize the newest release section first and treat older sections as background history unless the user asks for the full timeline.\n\
Do not invent fork features that are not listed here.\n\n\
{FORK_DELTA_INVENTORY}\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_mentions_recent_fork_only_commands() {
        let body = ForkHelpInstructions::new().body();

        assert!(body.contains("what is new in Rick's version"));
        assert!(body.contains("/account <alias>"));
        assert!(body.contains("/orchestrator-memory-forget <needle>"));
    }
}
