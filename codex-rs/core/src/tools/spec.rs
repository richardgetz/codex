use crate::shell::Shell;
use crate::shell::ShellType;
use crate::tools::handlers::multi_agents_common::DEFAULT_WAIT_TIMEOUT_MS;
use crate::tools::handlers::multi_agents_common::MAX_WAIT_TIMEOUT_MS;
use crate::tools::handlers::multi_agents_common::MIN_WAIT_TIMEOUT_MS;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::registry::ToolRegistryBuilder;
use crate::tools::spec_plan::build_tool_registry_builder;
use crate::tools::spec_plan_types::ToolNamespace;
use crate::tools::spec_plan_types::ToolRegistryBuildDeferredTool;
use crate::tools::spec_plan_types::ToolRegistryBuildLazyMcpServer;
use crate::tools::spec_plan_types::ToolRegistryBuildMcpTool;
use crate::tools::spec_plan_types::ToolRegistryBuildParams;
use codex_mcp::LazyMcpServerInfo;
use codex_mcp::ToolInfo;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_tools::AdditionalProperties;
use codex_tools::DiscoverableTool;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolUserShellType;
use codex_tools::ToolsConfig;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) fn tool_user_shell_type(user_shell: &Shell) -> ToolUserShellType {
    match user_shell.shell_type {
        ShellType::Zsh => ToolUserShellType::Zsh,
        ShellType::Bash => ToolUserShellType::Bash,
        ShellType::PowerShell => ToolUserShellType::PowerShell,
        ShellType::Sh => ToolUserShellType::Sh,
        ShellType::Cmd => ToolUserShellType::Cmd,
    }
}

struct McpToolPlanInputs<'a> {
    mcp_tools: Vec<ToolRegistryBuildMcpTool<'a>>,
    tool_namespaces: HashMap<String, ToolNamespace>,
}

fn map_mcp_tools_for_plan(mcp_tools: &HashMap<String, ToolInfo>) -> McpToolPlanInputs<'_> {
    McpToolPlanInputs {
        mcp_tools: mcp_tools
            .values()
            .map(|tool| ToolRegistryBuildMcpTool {
                name: tool.canonical_tool_name(),
                tool: &tool.tool,
            })
            .collect(),
        tool_namespaces: mcp_tools
            .values()
            .map(|tool| {
                (
                    tool.callable_namespace.clone(),
                    ToolNamespace {
                        name: tool.callable_namespace.clone(),
                        description: tool.namespace_description.clone(),
                    },
                )
            })
            .collect(),
    }
}

pub(crate) fn build_specs_with_discoverable_tools(
    config: &ToolsConfig,
    mcp_tools: Option<HashMap<String, ToolInfo>>,
    deferred_mcp_tools: Option<HashMap<String, ToolInfo>>,
    lazy_mcp_servers: Vec<LazyMcpServerInfo>,
    unavailable_called_tools: Vec<ToolName>,
    discoverable_tools: Option<Vec<DiscoverableTool>>,
    dynamic_tools: &[DynamicToolSpec],
) -> ToolRegistryBuilder {
    use crate::tools::handlers::UnavailableToolHandler;
    use crate::tools::handlers::builtin_schedule::BUILTIN_SCHEDULE_TOOL_NAMES;
    use crate::tools::handlers::builtin_schedule::BuiltinScheduleHandler;
    use crate::tools::handlers::builtin_schedule::schedule_namespace_spec;
    use crate::tools::handlers::builtin_scratchpad::BUILTIN_SCRATCHPAD_TOOL_NAMES;
    use crate::tools::handlers::builtin_scratchpad::BuiltinScratchpadHandler;
    use crate::tools::handlers::builtin_scratchpad::scratchpad_namespace_spec;
    use crate::tools::handlers::mcp::McpHandler;
    use crate::tools::handlers::session_overwatch::SESSION_OVERWATCH_TOOL_NAMES;
    use crate::tools::handlers::session_overwatch::SessionOverwatchHandler;
    use crate::tools::handlers::session_overwatch::session_overwatch_namespace_spec;
    use crate::tools::handlers::unavailable_tool_message;
    use crate::tools::tool_search_entry::build_tool_search_entries_for_config;

    let mcp_tool_plan_inputs = mcp_tools.as_ref().map(map_mcp_tools_for_plan);
    let deferred_mcp_tool_sources = deferred_mcp_tools.as_ref().map(|tools| {
        tools
            .values()
            .map(|tool| ToolRegistryBuildDeferredTool {
                name: tool.canonical_tool_name(),
                server_name: tool.server_name.as_str(),
                connector_name: tool.connector_name.as_deref(),
                description: tool.namespace_description.as_deref(),
            })
            .collect::<Vec<_>>()
    });
    let lazy_mcp_server_sources = lazy_mcp_servers
        .iter()
        .map(|server| ToolRegistryBuildLazyMcpServer {
            server_name: server.server_name.as_str(),
            description: server.description.as_deref(),
        })
        .collect::<Vec<_>>();
    let default_agent_type_description =
        crate::agent::role::spawn_tool_spec::build(&std::collections::BTreeMap::new());
    let min_wait_timeout_ms = if config.multi_agent_v2 {
        config
            .wait_agent_min_timeout_ms
            .unwrap_or(MIN_WAIT_TIMEOUT_MS)
            .clamp(1, MAX_WAIT_TIMEOUT_MS)
    } else {
        MIN_WAIT_TIMEOUT_MS
    };
    let default_wait_timeout_ms =
        DEFAULT_WAIT_TIMEOUT_MS.clamp(min_wait_timeout_ms, MAX_WAIT_TIMEOUT_MS);
    let deferred_dynamic_tools = dynamic_tools
        .iter()
        .filter(|tool| tool.defer_loading && (config.namespace_tools || tool.namespace.is_none()))
        .cloned()
        .collect::<Vec<_>>();
    let tool_search_entries = build_tool_search_entries_for_config(
        config,
        deferred_mcp_tools.as_ref(),
        lazy_mcp_servers.as_slice(),
        &deferred_dynamic_tools,
    );
    let mut builder = build_tool_registry_builder(
        config,
        ToolRegistryBuildParams {
            mcp_tools: mcp_tool_plan_inputs
                .as_ref()
                .map(|inputs| inputs.mcp_tools.as_slice()),
            deferred_mcp_tools: deferred_mcp_tool_sources.as_deref(),
            lazy_mcp_servers: lazy_mcp_server_sources.as_slice(),
            tool_namespaces: mcp_tool_plan_inputs
                .as_ref()
                .map(|inputs| &inputs.tool_namespaces),
            discoverable_tools: discoverable_tools.as_deref(),
            dynamic_tools,
            default_agent_type_description: &default_agent_type_description,
            wait_agent_timeouts: WaitAgentTimeoutOptions {
                default_timeout_ms: default_wait_timeout_ms,
                min_timeout_ms: min_wait_timeout_ms,
                max_timeout_ms: MAX_WAIT_TIMEOUT_MS,
            },
            tool_search_entries: &tool_search_entries,
        },
    );
    let mcp_handler = Arc::new(McpHandler::new(ToolName::plain("mcp")));
    let has_unavailable_mcp_tools = unavailable_called_tools
        .iter()
        .any(|tool_name| tool_name.display().starts_with("mcp__"));
    if has_unavailable_mcp_tools || !lazy_mcp_servers.is_empty() {
        builder.register_fallback_mcp_handler(mcp_handler);
    }

    let mut existing_spec_names = builder
        .specs()
        .iter()
        .filter(|configured_tool| {
            (!config.builtin_scratchpad_enabled || configured_tool.name() != "scratchpad")
                && (!config.builtin_schedule_enabled || configured_tool.name() != "schedule")
                && (!config.builtin_session_overwatch_enabled
                    || configured_tool.name() != "session_overwatch")
        })
        .map(|configured_tool| configured_tool.name().to_string())
        .collect::<HashSet<_>>();

    if config.builtin_scratchpad_enabled {
        let scratchpad_handler = Arc::new(BuiltinScratchpadHandler);
        existing_spec_names.insert("scratchpad".to_string());
        builder.push_spec(
            scratchpad_namespace_spec(),
            /*supports_parallel_tool_calls*/ false,
            config.code_mode_enabled,
        );
        for tool_name in BUILTIN_SCRATCHPAD_TOOL_NAMES {
            builder.register_handler_with_name(
                ToolName::namespaced("scratchpad", *tool_name),
                scratchpad_handler.clone(),
            );
        }
    }

    if config.builtin_schedule_enabled {
        let schedule_handler = Arc::new(BuiltinScheduleHandler);
        existing_spec_names.insert("schedule".to_string());
        builder.push_spec(
            schedule_namespace_spec(),
            /*supports_parallel_tool_calls*/ false,
            config.code_mode_enabled,
        );
        for tool_name in BUILTIN_SCHEDULE_TOOL_NAMES {
            builder.register_handler_with_name(
                ToolName::namespaced("schedule", *tool_name),
                schedule_handler.clone(),
            );
        }
    }

    if config.builtin_session_overwatch_enabled {
        let session_overwatch_handler = Arc::new(SessionOverwatchHandler);
        existing_spec_names.insert("session_overwatch".to_string());
        builder.push_spec(
            session_overwatch_namespace_spec(),
            /*supports_parallel_tool_calls*/ false,
            config.code_mode_enabled,
        );
        for tool_name in SESSION_OVERWATCH_TOOL_NAMES {
            builder.register_handler_with_name(
                ToolName::namespaced("session_overwatch", *tool_name),
                session_overwatch_handler.clone(),
            );
        }
    }

    for unavailable_tool in unavailable_called_tools {
        let tool_name = unavailable_tool.display();
        if existing_spec_names.insert(tool_name.clone()) {
            let spec = codex_tools::ToolSpec::Function(ResponsesApiTool {
                name: tool_name.clone(),
                description: unavailable_tool_message(
                    &tool_name,
                    "Calling this placeholder returns an error explaining that the tool is unavailable.",
                ),
                strict: false,
                parameters: JsonSchema::object(
                    Default::default(),
                    /*required*/ None,
                    Some(AdditionalProperties::Boolean(false)),
                ),
                output_schema: None,
                defer_loading: None,
            });
            builder.push_spec(
                spec,
                /*supports_parallel_tool_calls*/ false,
                config.code_mode_enabled,
            );
        }
        builder.register_handler(Arc::new(UnavailableToolHandler::new(unavailable_tool)));
    }
    builder
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;
