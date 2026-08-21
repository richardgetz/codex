use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;

use codex_connectors::AppToolPolicyEvaluator;
use codex_connectors::AppToolPolicyInput;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpBinding;
use codex_mcp::ToolInfo as McpToolInfo;
use codex_mcp::tool_is_model_visible;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use tracing::instrument;
use tracing::warn;

const MAX_AGENT_PLUGIN_MCP_SPEC_BYTES: usize = 8_000;
const MAX_AGENT_PLUGIN_MCP_TOTAL_BYTES: usize = 64_000;

use crate::config::Config;
use crate::connectors;
use crate::tools::handlers::McpHandler;
use crate::tools::registry::ToolRegistry;

#[derive(Default)]
pub(crate) struct McpHandlerCache {
    cached: Mutex<Option<CachedMcpHandlers>>,
}

struct CachedMcpHandlers {
    binding: usize,
    handlers: HashMap<ToolName, Arc<McpHandler>>,
}

impl McpHandlerCache {
    pub(crate) fn append_mcp_tools(
        &self,
        binding: &McpBinding,
        config: &Config,
        apps_enabled: bool,
        mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
        search_tool_enabled: bool,
        registry: &mut ToolRegistry,
    ) -> HashSet<ToolName> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let binding_ptr = std::ptr::from_ref(binding) as usize;
        if cached
            .as_ref()
            .is_none_or(|cached| cached.binding != binding_ptr)
        {
            *cached = None;
        }

        let cached = cached.get_or_insert_with(|| CachedMcpHandlers {
            binding: binding_ptr,
            handlers: HashMap::new(),
        });
        append_mcp_tools(
            binding.tools(),
            config,
            apps_enabled,
            mcp_server_catalog,
            search_tool_enabled,
            &mut cached.handlers,
            registry,
        )
    }
}

#[instrument(level = "trace", skip_all)]
fn append_mcp_tools(
    all_mcp_tools: &[McpToolInfo],
    config: &Config,
    apps_enabled: bool,
    mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
    search_tool_enabled: bool,
    handlers: &mut HashMap<ToolName, Arc<McpHandler>>,
    registry: &mut ToolRegistry,
) -> HashSet<ToolName> {
    append_mcp_tools_with_selection(
        all_mcp_tools,
        None,
        &[],
        &HashSet::new(),
        config,
        apps_enabled,
        mcp_server_catalog,
        search_tool_enabled,
        handlers,
        registry,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn append_mcp_tools_for_input(
    all_mcp_tools: &[McpToolInfo],
    connectors: Option<&[connectors::AppInfo]>,
    explicitly_enabled_connectors: &[connectors::AppInfo],
    explicitly_referenced_mcp_servers: &HashSet<String>,
    config: &Config,
    apps_enabled: bool,
    mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
    search_tool_enabled: bool,
    registry: &mut ToolRegistry,
) -> HashSet<ToolName> {
    let mut handlers = HashMap::new();
    append_mcp_tools_with_selection(
        all_mcp_tools,
        connectors,
        explicitly_enabled_connectors,
        explicitly_referenced_mcp_servers,
        config,
        apps_enabled,
        mcp_server_catalog,
        search_tool_enabled,
        &mut handlers,
        registry,
    )
}

#[allow(clippy::too_many_arguments)]
fn append_mcp_tools_with_selection(
    all_mcp_tools: &[McpToolInfo],
    connectors: Option<&[connectors::AppInfo]>,
    explicitly_enabled_connectors: &[connectors::AppInfo],
    explicitly_referenced_mcp_servers: &HashSet<String>,
    config: &Config,
    apps_enabled: bool,
    mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
    search_tool_enabled: bool,
    handlers: &mut HashMap<ToolName, Arc<McpHandler>>,
    registry: &mut ToolRegistry,
) -> HashSet<ToolName> {
    // Keep regular MCP tools first; Apps tools also require connector and policy checks.
    let non_app_tools = filter_non_codex_apps_mcp_tools_only(all_mcp_tools);
    let direct_only_app_tools = apps_enabled
        .then(|| {
            filter_codex_apps_mcp_tools(all_mcp_tools, None, config)
                .filter(|tool| is_direct_only_namespace(tool, config))
        })
        .into_iter()
        .flatten();
    let selected_app_tools = apps_enabled
        .then(|| filter_codex_apps_mcp_tools(all_mcp_tools, connectors, config))
        .into_iter()
        .flatten();
    let mut exposed_tools = non_app_tools
        .chain(direct_only_app_tools)
        .chain(selected_app_tools)
        .cloned()
        .collect::<Vec<_>>();
    let direct_tool_names = if search_tool_enabled {
        let mut names =
            filter_codex_apps_mcp_tools(all_mcp_tools, Some(explicitly_enabled_connectors), config)
                .map(codex_mcp::ToolInfo::canonical_tool_name)
                .collect::<HashSet<_>>();
        names.extend(
            filter_explicitly_referenced_non_app_mcp_tools(
                all_mcp_tools,
                explicitly_referenced_mcp_servers,
            )
            .into_iter()
            .map(|tool| tool.canonical_tool_name()),
        );
        names
    } else {
        HashSet::new()
    };
    if !apps_enabled {
        exposed_tools.retain(|tool| tool.server_name != CODEX_APPS_MCP_SERVER_NAME);
    }
    let exposure = |tool: &McpToolInfo| {
        if search_tool_enabled
            && !is_direct_only_namespace(tool, config)
            && !direct_tool_names.contains(&tool.canonical_tool_name())
        {
            ToolExposure::Deferred
        } else {
            ToolExposure::Direct
        }
    };
    let mut registered_tools = HashSet::new();
    let mut agent_plugin_bytes = 0usize;
    for tool in exposed_tools {
        let tool_name = tool.canonical_tool_name();
        let agent_plugin = mcp_server_catalog
            .server(&tool.server_name)
            .is_some_and(|server| server.source().is_agent_plugin());
        let handler = match handlers.entry(tool_name.clone()) {
            Entry::Occupied(entry) => Arc::clone(entry.get()),
            Entry::Vacant(entry) => {
                let handler = if agent_plugin {
                    McpHandler::new_agent_plugin(tool.clone())
                } else {
                    McpHandler::new(tool.clone(), /*namespace_tools_enabled*/ true)
                };
                match handler {
                    Ok(handler) => Arc::clone(entry.insert(Arc::new(handler))),
                    Err(err) => {
                        warn!("Skipping MCP tool `{tool_name}`: failed to build tool spec: {err}");
                        continue;
                    }
                }
            }
        };

        let fits_agent_budget = if agent_plugin {
            handler.model_spec_bytes().is_ok_and(|bytes| {
                if bytes > MAX_AGENT_PLUGIN_MCP_SPEC_BYTES {
                    return false;
                }
                let next = agent_plugin_bytes.saturating_add(bytes);
                if next <= MAX_AGENT_PLUGIN_MCP_TOTAL_BYTES {
                    agent_plugin_bytes = next;
                    true
                } else {
                    false
                }
            })
        } else {
            true
        };
        let tool_exposure = if fits_agent_budget {
            exposure(&tool)
        } else {
            ToolExposure::Hidden
        };
        if registry.register_external_with_exposure(handler, tool_exposure) && fits_agent_budget {
            registered_tools.insert(tool_name);
        }
    }
    registered_tools
}

fn is_direct_only_namespace(tool: &McpToolInfo, config: &Config) -> bool {
    tool.canonical_tool_name()
        .namespace
        .as_deref()
        .is_some_and(|namespace| {
            config
                .code_mode
                .direct_only_tool_namespaces
                .iter()
                .any(|configured_namespace| configured_namespace == namespace)
        })
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

fn filter_non_codex_apps_mcp_tools_only(
    mcp_tools: &[McpToolInfo],
) -> impl Iterator<Item = &McpToolInfo> + '_ {
    mcp_tools.iter().filter(|tool| {
        tool.server_name != CODEX_APPS_MCP_SERVER_NAME && tool_is_model_visible(tool)
    })
}

fn filter_codex_apps_mcp_tools<'a>(
    mcp_tools: &'a [McpToolInfo],
    connectors: Option<&'a [connectors::AppInfo]>,
    config: &'a Config,
) -> impl Iterator<Item = &'a McpToolInfo> + 'a {
    let app_tool_policy = AppToolPolicyEvaluator::new(&config.config_layer_stack);
    let allowed = connectors.map(|connectors| {
        connectors
            .iter()
            .map(|connector| connector.id.as_str())
            .collect::<HashSet<_>>()
    });

    mcp_tools.iter().filter(move |tool| {
        if tool.server_name != CODEX_APPS_MCP_SERVER_NAME {
            return false;
        }
        if !tool_is_model_visible(tool) {
            return false;
        }
        let Some(connector_id) = tool.connector_id.as_deref() else {
            return false;
        };
        if let Some(allowed) = &allowed
            && !allowed.contains(connector_id)
        {
            return false;
        }
        let annotations = tool.tool.annotations.as_ref();
        app_tool_policy
            .policy(AppToolPolicyInput {
                connector_id: Some(connector_id),
                tool_name: &tool.tool.name,
                tool_title: tool.tool.title.as_deref(),
                destructive_hint: annotations.and_then(|annotations| annotations.destructive_hint),
                open_world_hint: annotations.and_then(|annotations| annotations.open_world_hint),
            })
            .enabled
    })
}

#[cfg(test)]
#[path = "mcp_tool_exposure_test.rs"]
mod tests;
