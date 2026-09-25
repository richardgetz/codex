use crate::session::team::effective_role_for_session_source;
use crate::session::turn_context::TurnContext;
use crate::tools::registry::ToolExposure;
use crate::tools::registry::ToolRegistry;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::TeamMode;
use codex_tools::ToolName;

const TEAM_MANAGER_V1_TOOLS: &[&str] = &[
    "spawn_agent",
    "worker_capacity",
    "send_input",
    "resume_agent",
    "close_agent",
    "wait_agent",
];

const TEAM_MANAGER_V2_TOOLS: &[&str] = &[
    "spawn_agent",
    "worker_capacity",
    "send_message",
    "followup_task",
    "wait_agent",
    "interrupt_agent",
    "list_agents",
];

pub(crate) fn is_manager_only_lead(turn_context: &TurnContext) -> bool {
    turn_context.config.team_mode == TeamMode::LeadWorker
        && effective_role_for_session_source(&turn_context.config, &turn_context.session_source)
            == Some(codex_config::TeamRole::Lead)
        && turn_context.config.effective_team_lead_work_policy()
            == codex_config::TeamLeadWorkPolicy::ManagerOnly
}

/// Makes Team coordination tools available inside Code Mode when the Lead's
/// selected model exposes only the Code Mode entry points directly.
///
/// `manager_only` changes the Lead's work guidance and completion scheduling;
/// it must not restrict the tools handled by the normal tool router.
pub(crate) fn enable_code_mode_for_manager_coordination(
    turn_context: &TurnContext,
    model_info: &ModelInfo,
    registry: &mut ToolRegistry,
) {
    if !is_manager_only_lead(turn_context) {
        return;
    }

    let tool_mode = crate::tools::effective_tool_mode(turn_context, model_info);
    let code_mode_exposure = match tool_mode {
        ToolMode::CodeMode => ToolExposure::Direct,
        ToolMode::CodeModeOnly => ToolExposure::CodeModeOnly,
        ToolMode::Direct => return,
    };

    for tool in registry.entries_mut() {
        if !is_manager_coordination_tool(turn_context, &tool.runtime.tool_name())
            || tool.exposure != ToolExposure::DirectModelOnly
        {
            continue;
        }
        // V2 coordination is direct-only by default. ManagerOnly can still
        // reach it through Code Mode; the configured namespace override runs next.
        tool.exposure = code_mode_exposure;
    }
}

fn is_manager_coordination_tool(turn_context: &TurnContext, tool_name: &ToolName) -> bool {
    if tool_name.namespace.as_deref()
        == Some(crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE)
    {
        return TEAM_MANAGER_V1_TOOLS.contains(&tool_name.name.as_str());
    }

    if turn_context.multi_agent_version != MultiAgentVersion::V2 {
        return false;
    }

    if tool_name.is_default_namespace() && tool_name.name == "send_message_action" {
        return true;
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
