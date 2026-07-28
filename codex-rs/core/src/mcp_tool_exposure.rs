use std::collections::HashSet;
use std::sync::Arc;

use codex_connectors::AppToolPolicyEvaluator;
use codex_connectors::AppToolPolicyInput;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::ToolInfo as McpToolInfo;
use codex_mcp::tool_is_model_visible;
use codex_tools::ToolExposure;
use tracing::instrument;
use tracing::warn;

use crate::config::Config;
use crate::connectors;
use crate::tools::handlers::McpHandler;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::override_tool_exposure;

#[instrument(level = "trace", skip_all)]
pub(crate) fn build_mcp_tool_runtimes(
    all_mcp_tools: &[McpToolInfo],
    connectors: Option<&[connectors::AppInfo]>,
    explicitly_enabled_connectors: &[connectors::AppInfo],
    explicitly_referenced_mcp_servers: &HashSet<String>,
    config: &Config,
    search_tool_enabled: bool,
    namespace_tools_enabled: bool,
) -> Vec<Arc<dyn CoreToolRuntime>> {
    let mut exposed_tools = filter_non_codex_apps_mcp_tools_only(all_mcp_tools);
    if let Some(connectors) = connectors {
        exposed_tools.extend(filter_codex_apps_mcp_tools(
            all_mcp_tools,
            connectors,
            config,
        ));
    }

    let mut direct_tools =
        filter_codex_apps_mcp_tools(all_mcp_tools, explicitly_enabled_connectors, config);
    direct_tools.extend(filter_explicitly_referenced_non_app_mcp_tools(
        all_mcp_tools,
        explicitly_referenced_mcp_servers,
    ));
    let direct_tool_names = direct_tools
        .iter()
        .map(McpToolInfo::canonical_tool_name)
        .collect::<HashSet<_>>();
    exposed_tools
        .into_iter()
        .filter_map(|tool| {
            let tool_name = tool.canonical_tool_name();
            let exposure = if search_tool_enabled && !direct_tool_names.contains(&tool_name) {
                ToolExposure::Deferred
            } else {
                ToolExposure::Direct
            };
            match McpHandler::new(tool, namespace_tools_enabled) {
                Ok(handler) => {
                    let handler: Arc<dyn CoreToolRuntime> = Arc::new(handler);
                    Some(override_tool_exposure(handler, exposure))
                }
                Err(err) => {
                    warn!("Skipping MCP tool `{tool_name}`: failed to build tool spec: {err}");
                    None
                }
            }
        })
        .collect()
}

fn filter_explicitly_referenced_non_app_mcp_tools(
    mcp_tools: &[McpToolInfo],
    explicitly_referenced_mcp_servers: &HashSet<String>,
) -> Vec<McpToolInfo> {
    if explicitly_referenced_mcp_servers.is_empty() {
        return Vec::new();
    }

    mcp_tools
        .iter()
        .filter(|tool| {
            tool.server_name != CODEX_APPS_MCP_SERVER_NAME
                && tool_is_model_visible(tool)
                && explicitly_referenced_mcp_servers.contains(&tool.server_name)
        })
        .cloned()
        .collect()
}

fn filter_non_codex_apps_mcp_tools_only(mcp_tools: &[McpToolInfo]) -> Vec<McpToolInfo> {
    mcp_tools
        .iter()
        .filter(|tool| {
            tool.server_name != CODEX_APPS_MCP_SERVER_NAME && tool_is_model_visible(tool)
        })
        .cloned()
        .collect()
}

fn filter_codex_apps_mcp_tools(
    mcp_tools: &[McpToolInfo],
    connectors: &[connectors::AppInfo],
    config: &Config,
) -> Vec<McpToolInfo> {
    let allowed: HashSet<&str> = connectors
        .iter()
        .map(|connector| connector.id.as_str())
        .collect();
    let app_tool_policy = AppToolPolicyEvaluator::new(&config.config_layer_stack);

    mcp_tools
        .iter()
        .filter(|tool| {
            if tool.server_name != CODEX_APPS_MCP_SERVER_NAME {
                return false;
            }
            if !tool_is_model_visible(tool) {
                return false;
            }
            let Some(connector_id) = tool.connector_id.as_deref() else {
                return false;
            };
            let annotations = tool.tool.annotations.as_ref();
            allowed.contains(connector_id)
                && app_tool_policy
                    .policy(AppToolPolicyInput {
                        connector_id: Some(connector_id),
                        tool_name: &tool.tool.name,
                        tool_title: tool.tool.title.as_deref(),
                        destructive_hint: annotations
                            .and_then(|annotations| annotations.destructive_hint),
                        open_world_hint: annotations
                            .and_then(|annotations| annotations.open_world_hint),
                    })
                    .enabled
        })
        .cloned()
        .collect()
}

#[cfg(test)]
#[path = "mcp_tool_exposure_test.rs"]
mod tests;
