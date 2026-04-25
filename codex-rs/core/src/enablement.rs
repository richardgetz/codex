use std::collections::HashMap;

use codex_app_server_protocol::AppInfo;
use codex_config::EnablementFilterConfig;
use codex_config::EnablementFilterMode;
use codex_mcp::LazyMcpServerInfo;
use codex_mcp::ToolInfo;
use codex_plugin::PluginCapabilitySummary;
use codex_protocol::config_types::ModeKind;
use codex_tools::DiscoverableTool;

use crate::config::Config;

fn selector_matches(selector: &str, candidates: &[&str]) -> bool {
    let selector = selector.trim();
    if selector.is_empty() {
        return false;
    }
    if selector == "*" {
        return candidates
            .iter()
            .any(|candidate| !candidate.trim().is_empty());
    }

    candidates.iter().any(|candidate| {
        let candidate = candidate.trim();
        !candidate.is_empty() && (candidate == selector || candidate.ends_with(selector))
    })
}

fn filter_allows(filter: &EnablementFilterConfig, candidates: &[&str]) -> bool {
    if filter.items.is_empty() {
        return true;
    }

    let matches = filter
        .items
        .iter()
        .any(|selector| selector_matches(selector, candidates));
    match filter.mode {
        EnablementFilterMode::Include => matches,
        EnablementFilterMode::Exclude => !matches,
    }
}

fn skill_filter(config: &Config, mode: ModeKind) -> Option<&EnablementFilterConfig> {
    config
        .enablement
        .modes
        .get(&mode)
        .and_then(|mode_enablement| mode_enablement.skills.as_ref())
}

fn mcp_filter(config: &Config, mode: ModeKind) -> Option<&EnablementFilterConfig> {
    config
        .enablement
        .modes
        .get(&mode)
        .and_then(|mode_enablement| mode_enablement.mcps.as_ref())
}

fn plugin_filter(config: &Config, mode: ModeKind) -> Option<&EnablementFilterConfig> {
    config
        .enablement
        .modes
        .get(&mode)
        .and_then(|mode_enablement| mode_enablement.plugins.as_ref())
}

pub(crate) fn skill_enablement_filter(
    config: &Config,
    mode: ModeKind,
) -> Option<&EnablementFilterConfig> {
    skill_filter(config, mode)
}

pub(crate) fn mcp_tool_allowed_in_mode(config: &Config, mode: ModeKind, tool: &ToolInfo) -> bool {
    let Some(filter) = mcp_filter(config, mode) else {
        return true;
    };
    let mut candidates = vec![
        tool.server_name.as_str(),
        tool.callable_namespace.as_str(),
        tool.callable_name.as_str(),
    ];
    if let Some(connector_id) = tool.connector_id.as_deref() {
        candidates.push(connector_id);
    }
    if let Some(connector_name) = tool.connector_name.as_deref() {
        candidates.push(connector_name);
    }
    filter_allows(filter, &candidates)
}

pub(crate) fn filter_mcp_tools_for_mode(
    config: &Config,
    mode: ModeKind,
    tools: &HashMap<String, ToolInfo>,
) -> HashMap<String, ToolInfo> {
    tools
        .iter()
        .filter(|(_, tool)| mcp_tool_allowed_in_mode(config, mode, tool))
        .map(|(name, tool)| (name.clone(), tool.clone()))
        .collect()
}

pub(crate) fn lazy_mcp_server_allowed_in_mode(
    config: &Config,
    mode: ModeKind,
    server: &LazyMcpServerInfo,
) -> bool {
    let Some(filter) = mcp_filter(config, mode) else {
        return true;
    };
    filter_allows(filter, &[server.server_name.as_str()])
}

pub(crate) fn filter_lazy_mcp_servers_for_mode(
    config: &Config,
    mode: ModeKind,
    servers: &[LazyMcpServerInfo],
) -> Vec<LazyMcpServerInfo> {
    servers
        .iter()
        .filter(|server| lazy_mcp_server_allowed_in_mode(config, mode, server))
        .cloned()
        .collect()
}

pub(crate) fn connector_allowed_in_mode(
    config: &Config,
    mode: ModeKind,
    connector: &AppInfo,
) -> bool {
    let Some(filter) = mcp_filter(config, mode) else {
        return true;
    };
    filter_allows(filter, &[connector.id.as_str(), connector.name.as_str()])
}

pub(crate) fn filter_connectors_for_mode(
    config: &Config,
    mode: ModeKind,
    connectors: &[AppInfo],
) -> Vec<AppInfo> {
    connectors
        .iter()
        .filter(|connector| connector_allowed_in_mode(config, mode, connector))
        .cloned()
        .collect()
}

pub(crate) fn plugin_allowed_in_mode(
    config: &Config,
    mode: ModeKind,
    plugin: &PluginCapabilitySummary,
) -> bool {
    let Some(filter) = plugin_filter(config, mode) else {
        return true;
    };
    filter_allows(
        filter,
        &[plugin.config_name.as_str(), plugin.display_name.as_str()],
    )
}

pub(crate) fn filter_plugins_for_mode<'a>(
    config: &Config,
    mode: ModeKind,
    plugins: &'a [PluginCapabilitySummary],
) -> Vec<&'a PluginCapabilitySummary> {
    plugins
        .iter()
        .filter(|plugin| plugin_allowed_in_mode(config, mode, plugin))
        .collect()
}

pub(crate) fn filter_discoverable_tools_for_mode(
    config: &Config,
    mode: ModeKind,
    tools: Vec<DiscoverableTool>,
) -> Vec<DiscoverableTool> {
    tools
        .into_iter()
        .filter(|tool| match tool {
            DiscoverableTool::Connector(connector) => {
                connector_allowed_in_mode(config, mode, connector)
            }
            DiscoverableTool::Plugin(plugin) => {
                let summary = PluginCapabilitySummary {
                    config_name: plugin.id.clone(),
                    display_name: plugin.name.clone(),
                    description: plugin.description.clone(),
                    has_skills: plugin.has_skills,
                    mcp_server_names: plugin.mcp_server_names.clone(),
                    app_connector_ids: plugin
                        .app_connector_ids
                        .iter()
                        .cloned()
                        .map(codex_plugin::AppConnectorId)
                        .collect(),
                };
                plugin_allowed_in_mode(config, mode, &summary)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use codex_app_server_protocol::AppInfo;
    use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
    use codex_mcp::ToolInfo;
    use codex_plugin::AppConnectorId;
    use codex_plugin::PluginCapabilitySummary;
    use codex_protocol::config_types::ModeKind;
    use pretty_assertions::assert_eq;
    use rmcp::model::JsonObject;
    use rmcp::model::Tool;

    use super::*;
    use crate::config::test_config;

    fn make_connector(id: &str, name: &str) -> AppInfo {
        AppInfo {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            logo_url: None,
            logo_url_dark: None,
            distribution_channel: None,
            branding: None,
            app_metadata: None,
            labels: None,
            install_url: None,
            is_accessible: true,
            is_enabled: true,
            plugin_display_names: Vec::new(),
        }
    }

    fn make_mcp_tool(server_name: &str, tool_name: &str, connector_id: Option<&str>) -> ToolInfo {
        ToolInfo {
            server_name: server_name.to_string(),
            callable_name: tool_name.to_string(),
            callable_namespace: format!("mcp__{server_name}__"),
            server_instructions: None,
            tool: Tool {
                name: tool_name.to_string().into(),
                title: None,
                description: Some("Test".into()),
                input_schema: Arc::new(JsonObject::default()),
                output_schema: None,
                annotations: None,
                execution: None,
                icons: None,
                meta: None,
            },
            connector_id: connector_id.map(str::to_string),
            connector_name: connector_id.map(str::to_string),
            plugin_display_names: Vec::new(),
            connector_description: None,
        }
    }

    #[tokio::test]
    async fn filters_mcp_tools_by_mode_enablement() {
        let mut config = test_config().await;
        config.enablement.modes.insert(
            ModeKind::Orchestrator,
            codex_config::ModeEnablementConfig {
                mcps: Some(EnablementFilterConfig {
                    mode: EnablementFilterMode::Include,
                    items: vec!["scratchpad".to_string(), "calendar".to_string()],
                }),
                ..Default::default()
            },
        );

        let tools = HashMap::from([
            (
                "mcp__scratchpad__get".to_string(),
                make_mcp_tool("scratchpad", "get", None),
            ),
            (
                "mcp__other__run".to_string(),
                make_mcp_tool("other", "run", None),
            ),
            (
                "mcp__codex_apps__calendar_list".to_string(),
                make_mcp_tool(
                    CODEX_APPS_MCP_SERVER_NAME,
                    "calendar_list",
                    Some("calendar"),
                ),
            ),
        ]);

        let filtered = filter_mcp_tools_for_mode(&config, ModeKind::Orchestrator, &tools);
        let mut names = filtered.keys().cloned().collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            vec![
                "mcp__codex_apps__calendar_list".to_string(),
                "mcp__scratchpad__get".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn include_wildcard_keeps_all_mcp_tools() {
        let mut config = test_config().await;
        config.enablement.modes.insert(
            ModeKind::Orchestrator,
            codex_config::ModeEnablementConfig {
                mcps: Some(EnablementFilterConfig {
                    mode: EnablementFilterMode::Include,
                    items: vec!["*".to_string()],
                }),
                ..Default::default()
            },
        );

        let tools = HashMap::from([
            (
                "mcp__scratchpad__get".to_string(),
                make_mcp_tool("scratchpad", "get", None),
            ),
            (
                "mcp__other__run".to_string(),
                make_mcp_tool("other", "run", None),
            ),
        ]);

        let filtered = filter_mcp_tools_for_mode(&config, ModeKind::Orchestrator, &tools);
        assert_eq!(filtered.len(), 2);
    }

    #[tokio::test]
    async fn exclude_wildcard_hides_all_plugins() {
        let mut config = test_config().await;
        config.enablement.modes.insert(
            ModeKind::Orchestrator,
            codex_config::ModeEnablementConfig {
                plugins: Some(EnablementFilterConfig {
                    mode: EnablementFilterMode::Exclude,
                    items: vec!["*".to_string()],
                }),
                ..Default::default()
            },
        );

        let plugins = vec![
            PluginCapabilitySummary {
                config_name: "gmail@openai-curated".to_string(),
                display_name: "Gmail".to_string(),
                description: None,
                has_skills: true,
                mcp_server_names: Vec::new(),
                app_connector_ids: vec![AppConnectorId("gmail".to_string())],
            },
            PluginCapabilitySummary {
                config_name: "slack@openai-curated".to_string(),
                display_name: "Slack".to_string(),
                description: None,
                has_skills: true,
                mcp_server_names: Vec::new(),
                app_connector_ids: vec![AppConnectorId("slack".to_string())],
            },
        ];

        let filtered = filter_plugins_for_mode(&config, ModeKind::Orchestrator, &plugins);
        assert!(filtered.is_empty());
    }

    #[tokio::test]
    async fn filters_plugins_by_mode_enablement() {
        let mut config = test_config().await;
        config.enablement.modes.insert(
            ModeKind::Orchestrator,
            codex_config::ModeEnablementConfig {
                plugins: Some(EnablementFilterConfig {
                    mode: EnablementFilterMode::Include,
                    items: vec!["gmail@openai-curated".to_string()],
                }),
                ..Default::default()
            },
        );

        let plugins = vec![
            PluginCapabilitySummary {
                config_name: "gmail@openai-curated".to_string(),
                display_name: "Gmail".to_string(),
                description: None,
                has_skills: true,
                mcp_server_names: Vec::new(),
                app_connector_ids: vec![AppConnectorId("gmail".to_string())],
            },
            PluginCapabilitySummary {
                config_name: "slack@openai-curated".to_string(),
                display_name: "Slack".to_string(),
                description: None,
                has_skills: true,
                mcp_server_names: Vec::new(),
                app_connector_ids: vec![AppConnectorId("slack".to_string())],
            },
        ];

        let filtered = filter_plugins_for_mode(&config, ModeKind::Orchestrator, &plugins);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].display_name, "Gmail");
    }

    #[tokio::test]
    async fn filters_connectors_by_mode_enablement() {
        let mut config = test_config().await;
        config.enablement.modes.insert(
            ModeKind::Orchestrator,
            codex_config::ModeEnablementConfig {
                mcps: Some(EnablementFilterConfig {
                    mode: EnablementFilterMode::Exclude,
                    items: vec!["slack".to_string()],
                }),
                ..Default::default()
            },
        );

        let connectors = vec![
            make_connector("gmail", "Gmail"),
            make_connector("slack", "Slack"),
        ];
        let filtered = filter_connectors_for_mode(&config, ModeKind::Orchestrator, &connectors);
        assert_eq!(filtered, vec![make_connector("gmail", "Gmail")]);
    }
}
