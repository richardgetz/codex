use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;

use super::ContextualUserFragment;

const MCP_INSTRUCTIONS_OPEN_TAG: &str = "<mcp_instructions>";
const MCP_INSTRUCTIONS_CLOSE_TAG: &str = "</mcp_instructions>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AvailableMcpInstructions {
    direct_servers: Vec<String>,
    lazy_servers: Vec<String>,
}

impl AvailableMcpInstructions {
    pub(crate) fn new(
        mut direct_servers: Vec<String>,
        mut lazy_servers: Vec<String>,
    ) -> Option<Self> {
        direct_servers.retain(|name| name != CODEX_APPS_MCP_SERVER_NAME);
        lazy_servers.retain(|name| name != CODEX_APPS_MCP_SERVER_NAME);
        direct_servers.sort();
        direct_servers.dedup();
        lazy_servers.retain(|name| direct_servers.binary_search(name).is_err());
        lazy_servers.sort();
        lazy_servers.dedup();

        if direct_servers.is_empty() && lazy_servers.is_empty() {
            return None;
        }

        Some(Self {
            direct_servers,
            lazy_servers,
        })
    }
}

impl ContextualUserFragment for AvailableMcpInstructions {
    const ROLE: &'static str = "developer";
    const START_MARKER: &'static str = MCP_INSTRUCTIONS_OPEN_TAG;
    const END_MARKER: &'static str = MCP_INSTRUCTIONS_CLOSE_TAG;

    fn body(&self) -> String {
        let mut lines = vec![
            "## MCP Servers".to_string(),
            "Below is the non-app MCP inventory available in this session. Treat it as a first-class source when the user asks what tools or MCPs you can access.".to_string(),
        ];

        if !self.direct_servers.is_empty() {
            lines.push("### Direct MCP servers".to_string());
            lines.extend(
                self.direct_servers
                    .iter()
                    .map(|server| format!("- `{server}`")),
            );
        }

        if !self.lazy_servers.is_empty() {
            lines.push("### Lazy MCP servers".to_string());
            lines.extend(
                self.lazy_servers
                    .iter()
                    .map(|server| format!("- `{server}`")),
            );
        }

        lines.push("### How to use MCP inventory".to_string());
        lines.push(
            "- When the user asks which MCPs you have, answer from this inventory instead of guessing from the skills list.".to_string(),
        );
        lines.push(
            "- `Direct MCP servers` are already surfaced in the session context.".to_string(),
        );
        lines.push(
            "- `Lazy MCP servers` are available in this session but may only surface their tools after lazy load or tool search.".to_string(),
        );

        format!("\n{}\n", lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_filters_codex_apps_sorts_and_deduplicates() {
        let instructions = AvailableMcpInstructions::new(
            vec![
                "imessage".to_string(),
                CODEX_APPS_MCP_SERVER_NAME.to_string(),
                "agent-state".to_string(),
                "imessage".to_string(),
            ],
            vec![
                "scratchpad".to_string(),
                CODEX_APPS_MCP_SERVER_NAME.to_string(),
                "scratchpad".to_string(),
            ],
        )
        .expect("inventory should be rendered");

        assert_eq!(
            instructions.direct_servers,
            vec!["agent-state".to_string(), "imessage".to_string()]
        );
        assert_eq!(instructions.lazy_servers, vec!["scratchpad".to_string()]);
    }

    #[test]
    fn new_prefers_direct_inventory_when_server_is_also_lazy() {
        let instructions = AvailableMcpInstructions::new(
            vec!["playwright".to_string()],
            vec!["playwright".to_string(), "semgrep".to_string()],
        )
        .expect("inventory should be rendered");

        assert_eq!(instructions.direct_servers, vec!["playwright".to_string()]);
        assert_eq!(instructions.lazy_servers, vec!["semgrep".to_string()]);
    }

    #[test]
    fn body_mentions_direct_and_lazy_servers() {
        let instructions = AvailableMcpInstructions::new(
            vec!["imessage".to_string()],
            vec!["scratchpad".to_string()],
        )
        .expect("inventory should be rendered");

        let body = instructions.body();
        assert!(body.contains("### Direct MCP servers"));
        assert!(body.contains("- `imessage`"));
        assert!(body.contains("### Lazy MCP servers"));
        assert!(body.contains("- `scratchpad`"));
    }
}
