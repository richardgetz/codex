use crate::session::team::effective_role_for_session_source;
use crate::session::turn_context::TurnContext;
use crate::tools::registry::ToolRegistry;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::TeamMode;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

const TEAM_MANAGER_V1_TOOLS: &[&str] = &[
    "spawn_agent",
    "send_input",
    "resume_agent",
    "close_agent",
    "wait_agent",
];

const TEAM_MANAGER_V2_TOOLS: &[&str] = &[
    "spawn_agent",
    "send_message",
    "followup_task",
    "wait_agent",
    "interrupt_agent",
    "list_agents",
];

const MANAGER_TOOLS: &[&str] = &[
    "update_plan",
    "update_eta",
    TOOL_SEARCH_TOOL_NAME,
    "send_message_action",
    "request_user_input",
    "request_user_input_async",
    "send_user_message_async",
    "send_message_to_user_async",
    "current_time",
    "get_context_remaining",
    "view_image",
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
    "open_scratchpad",
    "resume_scratchpad",
    "get_scratchpad",
    "get_scratchpad_summary",
    "append_scratchpad_note",
    "set_next_steps",
    "set_pending_waits",
    "set_action_policy",
    "mark_wait_checked",
    "update_scratchpad",
    "archive_scratchpad",
    "unarchive_scratchpad",
    "lookup_scratchpads",
    "get_scratchpad_schema",
    "check_action_allowed",
    "record_outcome",
    "export_outcomes",
    "record_delegation",
];

pub(crate) fn is_manager_only_lead(turn_context: &TurnContext) -> bool {
    turn_context.config.team_mode == TeamMode::LeadWorker
        && effective_role_for_session_source(&turn_context.config, &turn_context.session_source)
            == Some(codex_config::TeamRole::Lead)
        && turn_context.config.effective_team_lead_work_policy()
            == codex_config::TeamLeadWorkPolicy::ManagerOnly
}

pub(crate) fn allows_tool(turn_context: &TurnContext, tool_name: &ToolName) -> bool {
    if !is_manager_only_lead(turn_context) {
        return true;
    }

    if tool_name.namespace.as_deref()
        == Some(crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE)
    {
        return TEAM_MANAGER_V1_TOOLS.contains(&tool_name.name.as_str());
    }

    let tool_name = tool_name.clone().with_default_namespace();
    if tool_name.is_default_namespace() && MANAGER_TOOLS.contains(&tool_name.name.as_str()) {
        return true;
    }

    if turn_context.multi_agent_version != MultiAgentVersion::V2 {
        return false;
    }

    let v2_namespace_matches = if crate::tools::spec_plan::namespace_tools_enabled(turn_context) {
        match turn_context.config.multi_agent_v2.tool_namespace.as_deref() {
            Some(namespace) => tool_name.namespace.as_deref() == Some(namespace),
            None => tool_name.is_default_namespace(),
        }
    } else {
        tool_name.is_default_namespace()
    };
    v2_namespace_matches && TEAM_MANAGER_V2_TOOLS.contains(&tool_name.name.as_str())
}

pub(crate) fn allows_registered_tool(
    turn_context: &TurnContext,
    registry: &ToolRegistry,
    tool_name: &ToolName,
) -> bool {
    !is_manager_only_lead(turn_context)
        || (registry.is_trusted_tool(tool_name) && allows_tool(turn_context, tool_name))
}

pub(crate) fn restrict_registry(turn_context: &TurnContext, registry: &mut ToolRegistry) {
    if !is_manager_only_lead(turn_context) {
        return;
    }

    let disallowed_tools = registry
        .entries()
        .map(|tool| tool.runtime.tool_name())
        .filter(|tool_name| !allows_registered_tool(turn_context, registry, tool_name))
        .collect::<Vec<_>>();
    for tool_name in disallowed_tools {
        registry.remove(&tool_name);
    }
}

pub(crate) fn denial_message(tool_name: &ToolName) -> String {
    format!(
        "{} is unavailable to a manager_only Team Lead. Delegate execution work to a Worker; the Lead can use team coordination, planning, and review tools.",
        tool_name
    )
}

pub(crate) fn filter_tool_spec(turn_context: &TurnContext, spec: ToolSpec) -> Option<ToolSpec> {
    if !is_manager_only_lead(turn_context) {
        return Some(spec);
    }

    match spec {
        ToolSpec::Function(tool) => allows_tool(
            turn_context,
            &ToolName::plain(tool.name.clone()).with_default_namespace(),
        )
        .then_some(ToolSpec::Function(tool)),
        ToolSpec::Freeform(tool) => allows_tool(
            turn_context,
            &ToolName::plain(tool.name.clone()).with_default_namespace(),
        )
        .then_some(ToolSpec::Freeform(tool)),
        ToolSpec::Namespace(mut namespace) => {
            namespace.tools.retain(|tool| {
                let name = match tool {
                    ResponsesApiNamespaceTool::Function(tool) => &tool.name,
                    ResponsesApiNamespaceTool::Custom(tool) => &tool.name,
                };
                allows_tool(
                    turn_context,
                    &ToolName::namespaced(namespace.name.clone(), name.clone()),
                )
            });
            (!namespace.tools.is_empty()).then_some(ToolSpec::Namespace(namespace))
        }
        spec @ ToolSpec::ToolSearch { .. }
            if allows_tool(turn_context, &ToolName::plain(TOOL_SEARCH_TOOL_NAME)) =>
        {
            Some(spec)
        }
        ToolSpec::ToolSearch { .. } | ToolSpec::WebSearch { .. } => None,
    }
}
