use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;
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
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::McpHandler;
use crate::tools::handlers::mcp::McpToolRecovery;
use crate::tools::registry::ToolRegistry;

pub(crate) fn recovered_mcp_namespace_tools_enabled(turn_context: &TurnContext) -> bool {
    turn_context.provider.capabilities().namespace_tools
        && turn_context.tools_config.namespace_tools
}

#[derive(Default)]
pub(crate) struct McpHandlerCache {
    handlers: Mutex<HashMap<ToolName, CachedMcpHandler>>,
}

struct CachedMcpHandler {
    tool_info: McpToolInfo,
    agent_plugin: bool,
    schema_max_bytes: Option<NonZeroUsize>,
    recovery: McpToolRecovery,
    namespace_tools_enabled: bool,
    handler: Arc<McpHandler>,
}

#[derive(Clone, Copy)]
pub(crate) struct McpToolSelection<'a> {
    pub(crate) connectors: Option<&'a [connectors::AppInfo]>,
    pub(crate) explicitly_enabled_connectors: &'a [connectors::AppInfo],
    pub(crate) explicitly_referenced_mcp_servers: &'a HashSet<String>,
}

impl McpHandlerCache {
    pub(crate) fn append_mcp_tools(
        &self,
        binding: &Arc<McpBinding>,
        config: &Config,
        apps_enabled: bool,
        mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
        search_tool_enabled: bool,
        registry: &mut ToolRegistry,
    ) -> HashSet<ToolName> {
        self.append_mcp_tools_with_recovery(
            binding.tools(),
            config,
            apps_enabled,
            mcp_server_catalog,
            search_tool_enabled,
            &HashMap::new(),
            /*namespace_tools_enabled*/ true,
            registry,
        )
    }

    pub(crate) fn append_mcp_tools_with_recovery(
        &self,
        mcp_tools: &[McpToolInfo],
        config: &Config,
        apps_enabled: bool,
        mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
        search_tool_enabled: bool,
        recovered_tools: &HashMap<ToolName, McpToolRecovery>,
        namespace_tools_enabled: bool,
        registry: &mut ToolRegistry,
    ) -> HashSet<ToolName> {
        let mut handlers = self
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        append_mcp_tools_with_recovery(
            mcp_tools,
            config,
            apps_enabled,
            mcp_server_catalog,
            search_tool_enabled,
            recovered_tools,
            namespace_tools_enabled,
            &mut handlers,
            registry,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_mcp_tools_for_input_with_recovery(
        &self,
        mcp_tools: &[McpToolInfo],
        config: &Config,
        apps_enabled: bool,
        mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
        search_tool_enabled: bool,
        recovered_tools: &HashMap<ToolName, McpToolRecovery>,
        namespace_tools_enabled: bool,
        selection: McpToolSelection<'_>,
        registry: &mut ToolRegistry,
    ) -> HashSet<ToolName> {
        let mut handlers = self
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        append_mcp_tools_with_selection_and_recovery(
            mcp_tools,
            Some(selection),
            config,
            apps_enabled,
            mcp_server_catalog,
            search_tool_enabled,
            recovered_tools,
            namespace_tools_enabled,
            &mut handlers,
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
    handlers: &mut HashMap<ToolName, CachedMcpHandler>,
    registry: &mut ToolRegistry,
) -> HashSet<ToolName> {
    append_mcp_tools_with_recovery(
        all_mcp_tools,
        config,
        apps_enabled,
        mcp_server_catalog,
        search_tool_enabled,
        &HashMap::new(),
        /*namespace_tools_enabled*/ true,
        handlers,
        registry,
    )
}

#[instrument(level = "trace", skip_all)]
fn append_mcp_tools_with_recovery(
    all_mcp_tools: &[McpToolInfo],
    config: &Config,
    apps_enabled: bool,
    mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
    search_tool_enabled: bool,
    recovered_tools: &HashMap<ToolName, McpToolRecovery>,
    namespace_tools_enabled: bool,
    handlers: &mut HashMap<ToolName, CachedMcpHandler>,
    registry: &mut ToolRegistry,
) -> HashSet<ToolName> {
    append_mcp_tools_with_selection_and_recovery(
        all_mcp_tools,
        None,
        config,
        apps_enabled,
        mcp_server_catalog,
        search_tool_enabled,
        recovered_tools,
        namespace_tools_enabled,
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
    append_mcp_tools_with_selection_and_recovery(
        all_mcp_tools,
        Some(McpToolSelection {
            connectors,
            explicitly_enabled_connectors,
            explicitly_referenced_mcp_servers,
        }),
        config,
        apps_enabled,
        mcp_server_catalog,
        search_tool_enabled,
        &HashMap::new(),
        /*namespace_tools_enabled*/ true,
        &mut handlers,
        registry,
    )
}

#[instrument(level = "trace", skip_all)]
#[allow(clippy::too_many_arguments)]
fn append_mcp_tools_with_selection_and_recovery(
    all_mcp_tools: &[McpToolInfo],
    selection: Option<McpToolSelection<'_>>,
    config: &Config,
    apps_enabled: bool,
    mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
    search_tool_enabled: bool,
    recovered_tools: &HashMap<ToolName, McpToolRecovery>,
    namespace_tools_enabled: bool,
    handlers: &mut HashMap<ToolName, CachedMcpHandler>,
    registry: &mut ToolRegistry,
) -> HashSet<ToolName> {
    let current_tools: HashSet<_> = all_mcp_tools
        .iter()
        .map(McpToolInfo::canonical_tool_name)
        .collect();
    handlers.retain(|name, _| current_tools.contains(name));

    // Keep regular MCP tools first; Apps tools also require connector and policy checks.
    let exposed_tools = match selection.as_ref() {
        Some(selection) => {
            let direct_only_app_tools = apps_enabled
                .then(|| filter_codex_apps_mcp_tools(all_mcp_tools, None, config))
                .into_iter()
                .flatten()
                .filter(|tool| is_direct_only_namespace(tool, config))
                .collect::<Vec<_>>();
            let direct_only_tool_names = direct_only_app_tools
                .iter()
                .map(|tool| tool.canonical_tool_name())
                .collect::<HashSet<_>>();
            let selected_app_tools = apps_enabled
                .then(|| {
                    filter_codex_apps_mcp_tools(all_mcp_tools, selection.connectors, config)
                        .filter(|tool| {
                            !direct_only_tool_names.contains(&tool.canonical_tool_name())
                        })
                })
                .into_iter()
                .flatten();
            filter_non_codex_apps_mcp_tools_only(all_mcp_tools)
                .chain(direct_only_app_tools)
                .chain(selected_app_tools)
                .cloned()
                .collect::<Vec<_>>()
        }
        None => {
            let app_tools = apps_enabled
                .then(|| filter_codex_apps_mcp_tools(all_mcp_tools, None, config))
                .into_iter()
                .flatten();
            filter_non_codex_apps_mcp_tools_only(all_mcp_tools)
                .chain(app_tools)
                .cloned()
                .collect::<Vec<_>>()
        }
    };
    let direct_tool_names = selection.as_ref().map_or_else(HashSet::new, |selection| {
        if !search_tool_enabled {
            return HashSet::new();
        }
        let mut names = filter_codex_apps_mcp_tools(
            all_mcp_tools,
            Some(selection.explicitly_enabled_connectors),
            config,
        )
        .map(McpToolInfo::canonical_tool_name)
        .collect::<HashSet<_>>();
        names.extend(
            filter_explicitly_referenced_non_app_mcp_tools(
                all_mcp_tools,
                selection.explicitly_referenced_mcp_servers,
            )
            .into_iter()
            .map(|tool| tool.canonical_tool_name()),
        );
        names
    });
    let mut registered_tools = HashSet::new();
    let mut agent_plugin_bytes = 0usize;
    for tool in &exposed_tools {
        let tool_name = tool.canonical_tool_name();
        let recovery = recovered_tools
            .get(&tool_name)
            .copied()
            .unwrap_or_default();
        let server = mcp_server_catalog.server(&tool.server_name);
        let agent_plugin = server.is_some_and(|server| server.source().is_agent_plugin());
        let tool_input_schema_max_bytes =
            server.and_then(|server| server.config().tool_input_schema_max_bytes);
        // Handlers contain immutable tool metadata, not a connection or authorization snapshot.
        // Preserve their identity across equivalent bindings so the search index can also be reused.
        let handler = if let Some(cached) = handlers.get(&tool_name).filter(|cached| {
            cached.tool_info == *tool
                && cached.agent_plugin == agent_plugin
                && cached.schema_max_bytes == tool_input_schema_max_bytes
                && cached.recovery == recovery
                && cached.namespace_tools_enabled == namespace_tools_enabled
        }) {
            Arc::clone(&cached.handler)
        } else {
            handlers.remove(&tool_name);
            let handler = if recovery != McpToolRecovery::None {
                McpHandler::new_recovered_placeholder(
                    tool.clone(),
                    namespace_tools_enabled,
                    recovery,
                    agent_plugin,
                    tool_input_schema_max_bytes.map(NonZeroUsize::get),
                )
            } else if agent_plugin {
                McpHandler::new_agent_plugin_with_namespace_tools(
                    tool.clone(),
                    namespace_tools_enabled,
                )
            } else if let Some(budget) = tool_input_schema_max_bytes {
                McpHandler::new_with_schema_max_bytes_and_namespace_tools(
                    tool.clone(),
                    budget.get(),
                    namespace_tools_enabled,
                )
            } else {
                McpHandler::new_with_namespace_tools(tool.clone(), namespace_tools_enabled)
            };

            let handler = match handler {
                Ok(handler) => Arc::new(handler),
                Err(err) => {
                    warn!("Skipping MCP tool `{tool_name}`: failed to build tool spec: {err}");
                    continue;
                }
            };
            handlers.insert(
                tool_name.clone(),
                CachedMcpHandler {
                    tool_info: tool.clone(),
                    agent_plugin,
                    schema_max_bytes: tool_input_schema_max_bytes,
                    recovery,
                    namespace_tools_enabled,
                    handler: Arc::clone(&handler),
                },
            );
            handler
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
        let tool_exposure = if !fits_agent_budget {
            ToolExposure::Hidden
        } else if search_tool_enabled
            && selection.is_some()
            && !is_direct_only_namespace(tool, config)
            && !direct_tool_names.contains(&tool_name)
        {
            ToolExposure::Deferred
        } else if search_tool_enabled && selection.is_none() {
            ToolExposure::Deferred
        } else {
            ToolExposure::Direct
        };
        if registry.register_external_with_exposure(handler, tool_exposure) && fits_agent_budget {
            registered_tools.insert(tool_name);
        }
    }
    registered_tools
}

fn filter_non_codex_apps_mcp_tools_only(
    mcp_tools: &[McpToolInfo],
) -> impl Iterator<Item = &McpToolInfo> + '_ {
    mcp_tools.iter().filter(|tool| {
        tool.server_name != CODEX_APPS_MCP_SERVER_NAME && tool_is_model_visible(tool)
    })
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
        if allowed
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(connector_id))
        {
            return false;
        }
        let annotations = tool.tool.annotations.as_ref();
        app_tool_policy
            .policy(AppToolPolicyInput {
                connector_id: Some(connector_id),
                link_id: None,
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
