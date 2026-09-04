use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

use super::ContextualUserFragment;

const MCP_INSTRUCTIONS_OPEN_TAG: &str = "<mcp_instructions>";
const MCP_INSTRUCTIONS_CLOSE_TAG: &str = "</mcp_instructions>";
const MCP_SERVER_NAMES_MAX_BYTES: usize = 8 * 1024;
const MCP_SERVER_NAME_MAX_BYTES: usize = 512;
const AVAILABLE_MCP_INSTRUCTIONS_MAX_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AvailableMcpInstructions {
    direct_servers: Vec<String>,
    lazy_servers: Vec<String>,
    omitted_direct_servers: usize,
    omitted_lazy_servers: usize,
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

        let mut remaining_name_bytes = MCP_SERVER_NAMES_MAX_BYTES;
        let (direct_servers, omitted_direct_servers) =
            retain_bounded_server_names(direct_servers, &mut remaining_name_bytes);
        let (lazy_servers, omitted_lazy_servers) =
            retain_bounded_server_names(lazy_servers, &mut remaining_name_bytes);

        if direct_servers.is_empty()
            && lazy_servers.is_empty()
            && omitted_direct_servers == 0
            && omitted_lazy_servers == 0
        {
            return None;
        }

        Some(Self {
            direct_servers,
            lazy_servers,
            omitted_direct_servers,
            omitted_lazy_servers,
        })
    }
}

fn retain_bounded_server_names(
    servers: Vec<String>,
    remaining_bytes: &mut usize,
) -> (Vec<String>, usize) {
    let mut retained = Vec::new();
    let mut omitted = 0usize;
    for server in servers {
        let rendered_bytes = server.len().saturating_add(8);
        if server.len() > MCP_SERVER_NAME_MAX_BYTES || rendered_bytes > *remaining_bytes {
            omitted = omitted.saturating_add(1);
        } else {
            *remaining_bytes -= rendered_bytes;
            retained.push(server);
        }
    }
    (retained, omitted)
}

impl ContextualUserFragment for AvailableMcpInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("mcp.catalog".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        (MCP_INSTRUCTIONS_OPEN_TAG, MCP_INSTRUCTIONS_CLOSE_TAG)
    }

    fn type_markers() -> (&'static str, &'static str) {
        (MCP_INSTRUCTIONS_OPEN_TAG, MCP_INSTRUCTIONS_CLOSE_TAG)
    }

    fn body(&self) -> String {
        let mut lines = vec![
            "## MCP Servers".to_string(),
            "Below is the non-app MCP inventory available in this session. Treat it as a first-class source when the user asks what tools or MCPs you can access.".to_string(),
        ];

        if !self.direct_servers.is_empty() || self.omitted_direct_servers > 0 {
            lines.push("### Direct MCP servers".to_string());
            lines.extend(
                self.direct_servers
                    .iter()
                    .map(|server| format!("- `{server}`")),
            );
            if self.omitted_direct_servers > 0 {
                lines.push(format!(
                    "- {} direct MCP server name(s) omitted to keep this context bounded.",
                    self.omitted_direct_servers
                ));
            }
        }

        if !self.lazy_servers.is_empty() || self.omitted_lazy_servers > 0 {
            lines.push("### Lazy MCP servers".to_string());
            lines.extend(
                self.lazy_servers
                    .iter()
                    .map(|server| format!("- `{server}`")),
            );
            if self.omitted_lazy_servers > 0 {
                lines.push(format!(
                    "- {} lazy MCP server name(s) omitted to keep this context bounded.",
                    self.omitted_lazy_servers
                ));
            }
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

        truncate_text(
            format!("\n{}\n", lines.join("\n")).as_str(),
            TruncationPolicy::Bytes(AVAILABLE_MCP_INSTRUCTIONS_MAX_BYTES),
        )
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
        assert_eq!(instructions.omitted_direct_servers, 0);
        assert_eq!(instructions.omitted_lazy_servers, 0);
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
        assert_eq!(instructions.omitted_direct_servers, 0);
        assert_eq!(instructions.omitted_lazy_servers, 0);
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

    #[test]
    fn body_bounds_large_inventory_and_reports_omitted_servers() {
        let instructions = AvailableMcpInstructions::new(
            (0..2_000)
                .map(|index| format!("direct-server-{index:04}"))
                .collect(),
            Vec::new(),
        )
        .expect("large inventory should still be rendered");

        let body = instructions.body();
        assert!(body.len() <= AVAILABLE_MCP_INSTRUCTIONS_MAX_BYTES);
        assert!(body.contains("omitted to keep this context bounded"));
        assert!(instructions.omitted_direct_servers > 0);
    }
}
