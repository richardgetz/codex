use std::collections::HashSet;
use std::sync::Arc;

use codex_features::Feature;
use codex_features::Features;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::ToolInfo;
use codex_models_manager::test_support::construct_model_info_offline_for_tests;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::SessionSource;
use codex_tools::ToolName;
use codex_tools::ToolsConfig;
use codex_tools::ToolsConfigParams;
use pretty_assertions::assert_eq;
use rmcp::model::JsonObject;
use rmcp::model::Meta;
use rmcp::model::Tool;

use super::*;
use crate::config::test_config;
use crate::connectors::AppInfo;

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

fn make_mcp_tool(
    server_name: &str,
    tool_name: &str,
    callable_namespace: &str,
    callable_name: &str,
    connector_id: Option<&str>,
    connector_name: Option<&str>,
) -> ToolInfo {
    ToolInfo {
        server_name: server_name.to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: callable_name.to_string(),
        callable_namespace: callable_namespace.to_string(),
        namespace_description: None,
        tool: Tool::new(
            tool_name.to_string(),
            format!("Test tool: {tool_name}"),
            Arc::new(JsonObject::default()),
        ),
        connector_id: connector_id.map(str::to_string),
        connector_name: connector_name.map(str::to_string),
        plugin_display_names: Vec::new(),
    }
}

fn numbered_mcp_tools(count: usize) -> Vec<ToolInfo> {
    (0..count)
        .map(|index| {
            let tool_name = format!("tool_{index}");
            make_mcp_tool(
                "rmcp",
                &tool_name,
                "mcp__rmcp",
                &tool_name,
                /*connector_id*/ None,
                /*connector_name*/ None,
            )
        })
        .collect()
}

fn tool_names(tools: &[ToolInfo]) -> HashSet<ToolName> {
    tools
        .iter()
        .map(codex_mcp::ToolInfo::canonical_tool_name)
        .collect()
}

async fn tools_config_for_mcp_tool_exposure(search_tool: bool) -> ToolsConfig {
    let config = test_config().await;
    let model_info =
        construct_model_info_offline_for_tests("gpt-5.4", &config.to_models_manager_config());
    let features = Features::with_defaults();
    let available_models = Vec::new();
    let mut tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &model_info,
        available_models: &available_models,
        features: &features,
        image_generation_tool_auth_allowed: true,
        web_search_mode: Some(WebSearchMode::Cached),
        session_source: SessionSource::Cli,
        permission_profile: &PermissionProfile::Disabled,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
    });
    tools_config.search_tool = search_tool;
    tools_config
}

fn with_visibility(mut tool: ToolInfo, visibility: &[&str]) -> ToolInfo {
    tool.tool.meta = Some(Meta(
        serde_json::json!({ "ui": { "visibility": visibility } })
            .as_object()
            .expect("metadata object")
            .clone(),
    ));
    tool
}

#[tokio::test]
async fn directly_exposes_small_effective_tool_sets() {
    let config = test_config().await;
    let tools_config = tools_config_for_mcp_tool_exposure(/*search_tool*/ true).await;
    let mcp_tools = numbered_mcp_tools(DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD - 1);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &HashSet::new(),
        &config,
        &tools_config,
    );

    assert_eq!(tool_names(&exposure.direct_tools), tool_names(&mcp_tools));
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn excludes_tools_hidden_from_model_exposure() {
    let config = test_config().await;
    let visible_tool = make_mcp_tool(
        "rmcp",
        "visible_tool",
        "mcp__rmcp",
        "visible_tool",
        /*connector_id*/ None,
        /*connector_name*/ None,
    );
    let hidden_tool = with_visibility(
        make_mcp_tool(
            "rmcp",
            "hidden_tool",
            "mcp__rmcp",
            "hidden_tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
        &["app"],
    );
    let empty_visibility_tool = with_visibility(
        make_mcp_tool(
            "rmcp",
            "empty_visibility_tool",
            "mcp__rmcp",
            "empty_visibility_tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
        &[],
    );
    let visible_app_tool = with_visibility(
        make_mcp_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_read",
            "mcp__codex_apps__calendar",
            "read",
            Some("calendar"),
            Some("Calendar"),
        ),
        &["app", "model"],
    );
    let hidden_app_tool = with_visibility(
        make_mcp_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_open",
            "mcp__codex_apps__calendar",
            "open",
            Some("calendar"),
            Some("Calendar"),
        ),
        &["app"],
    );
    let mcp_tools = vec![
        visible_tool.clone(),
        hidden_tool,
        empty_visibility_tool,
        visible_app_tool.clone(),
        hidden_app_tool,
    ];
    let connectors = vec![make_connector("calendar", "Calendar")];
    let tools_config = tools_config_for_mcp_tool_exposure(/*search_tool*/ false).await;

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        Some(connectors.as_slice()),
        &[],
        &HashSet::new(),
        &config,
        &tools_config,
    );

    assert_eq!(
        tool_names(&exposure.direct_tools),
        tool_names(&[visible_tool, visible_app_tool])
    );
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn explicitly_referenced_servers_do_not_expose_hidden_tools() {
    let mut config = test_config().await;
    config
        .features
        .enable(Feature::ToolSearchAlwaysDeferMcpTools)
        .expect("enable tool search deferral");
    let visible_tool = make_mcp_tool(
        "rmcp",
        "visible_tool",
        "mcp__rmcp",
        "visible_tool",
        /*connector_id*/ None,
        /*connector_name*/ None,
    );
    let hidden_tool = with_visibility(
        make_mcp_tool(
            "rmcp",
            "hidden_tool",
            "mcp__rmcp",
            "hidden_tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
        &["app"],
    );
    let mcp_tools = vec![visible_tool.clone(), hidden_tool];
    let explicitly_referenced_mcp_servers = HashSet::from(["rmcp".to_string()]);
    let tools_config = tools_config_for_mcp_tool_exposure(/*search_tool*/ true).await;

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &explicitly_referenced_mcp_servers,
        &config,
        &tools_config,
    );

    assert_eq!(
        tool_names(&exposure.direct_tools),
        tool_names(&[visible_tool])
    );
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn searches_large_effective_tool_sets() {
    let config = test_config().await;
    let tools_config = tools_config_for_mcp_tool_exposure(/*search_tool*/ true).await;
    let mcp_tools = numbered_mcp_tools(DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &HashSet::new(),
        &config,
        &tools_config,
    );

    assert!(exposure.direct_tools.is_empty());
    let deferred_tools = exposure
        .deferred_tools
        .as_ref()
        .expect("large tool sets should be discoverable through tool_search");
    assert_eq!(tool_names(deferred_tools), tool_names(&mcp_tools));
}

#[tokio::test]
async fn directly_exposes_explicit_apps_without_deferred_overlap() {
    let config = test_config().await;
    let tools_config = tools_config_for_mcp_tool_exposure(/*search_tool*/ true).await;
    let mut mcp_tools = numbered_mcp_tools(DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD - 1);
    mcp_tools.push(make_mcp_tool(
        CODEX_APPS_MCP_SERVER_NAME,
        "calendar_create_event",
        "mcp__codex_apps__calendar",
        "_create_event",
        Some("calendar"),
        Some("Calendar"),
    ));
    let connectors = vec![make_connector("calendar", "Calendar")];

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        Some(connectors.as_slice()),
        connectors.as_slice(),
        &HashSet::new(),
        &config,
        &tools_config,
    );

    let direct_tool_names = tool_names(&exposure.direct_tools);
    assert_eq!(
        direct_tool_names,
        HashSet::from([ToolName::namespaced(
            "mcp__codex_apps__calendar",
            "_create_event"
        )])
    );
    assert_eq!(
        exposure.deferred_tools.as_ref().map(Vec::len),
        Some(DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD - 1)
    );
    let deferred_tools = exposure
        .deferred_tools
        .as_ref()
        .expect("large tool sets should be discoverable through tool_search");
    let deferred_tool_names = tool_names(deferred_tools);
    assert!(
        direct_tool_names.is_disjoint(&deferred_tool_names),
        "direct tools should not also be deferred: {direct_tool_names:?}"
    );
    assert!(!deferred_tool_names.contains(&ToolName::namespaced(
        "mcp__codex_apps__calendar",
        "_create_event"
    )));
    assert!(deferred_tool_names.contains(&ToolName::namespaced("mcp__rmcp", "tool_0")));
}

#[tokio::test]
async fn always_defer_feature_preserves_explicit_apps() {
    let mut config = test_config().await;
    config
        .features
        .enable(Feature::ToolSearchAlwaysDeferMcpTools)
        .expect("test config should allow feature update");
    let tools_config = tools_config_for_mcp_tool_exposure(/*search_tool*/ true).await;
    let mcp_tools = vec![
        make_mcp_tool(
            "rmcp",
            "tool",
            "mcp__rmcp",
            "tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
        make_mcp_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_create_event",
            "mcp__codex_apps__calendar",
            "_create_event",
            Some("calendar"),
            Some("Calendar"),
        ),
    ];
    let connectors = vec![make_connector("calendar", "Calendar")];

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        Some(connectors.as_slice()),
        connectors.as_slice(),
        &HashSet::new(),
        &config,
        &tools_config,
    );

    assert!(exposure.direct_tools.is_empty());
    let deferred_tools = exposure
        .deferred_tools
        .as_ref()
        .expect("MCP tools should be discoverable through tool_search");
    let deferred_tool_names = tool_names(deferred_tools);
    assert!(deferred_tool_names.contains(&ToolName::namespaced("mcp__rmcp", "tool")));
    assert!(deferred_tool_names.contains(&ToolName::namespaced(
        "mcp__codex_apps__calendar",
        "_create_event"
    )));
}

#[tokio::test]
async fn direct_exposure_preserves_explicit_non_app_mcp_servers() {
    let mut config = test_config().await;
    config
        .features
        .enable(Feature::ToolSearchAlwaysDeferMcpTools)
        .expect("test config should allow feature update");
    let tools_config = tools_config_for_mcp_tool_exposure(/*search_tool*/ true).await;
    let mcp_tools = vec![
        make_mcp_tool(
            "aws_docs",
            "search_documentation",
            "mcp__aws_docs__",
            "search_documentation",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
        make_mcp_tool(
            "rmcp",
            "tool",
            "mcp__rmcp__",
            "tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
    ];
    let explicitly_referenced_mcp_servers = HashSet::from(["aws_docs".to_string()]);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &explicitly_referenced_mcp_servers,
        &config,
        &tools_config,
    );

    let direct_tool_names = tool_names(&exposure.direct_tools);
    assert_eq!(
        direct_tool_names,
        HashSet::from([ToolName::namespaced(
            "mcp__aws_docs__",
            "search_documentation"
        )])
    );
    let deferred_tools = exposure
        .deferred_tools
        .as_ref()
        .expect("non-explicit MCP tools should remain deferred");
    let deferred_tool_names = tool_names(deferred_tools);
    assert!(deferred_tool_names.contains(&ToolName::namespaced("mcp__rmcp__", "tool")));
    assert!(!deferred_tool_names.contains(&ToolName::namespaced(
        "mcp__aws_docs__",
        "search_documentation"
    )));
}
