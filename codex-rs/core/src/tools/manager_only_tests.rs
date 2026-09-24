use super::*;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolRegistry;
use codex_config::TeamLeadWorkPolicy;
use codex_config::TeamModelProfile;
use codex_config::TeamModelProfiles;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::TeamMode;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use std::sync::Arc;

async fn manager_only_context(
    multi_agent_version: MultiAgentVersion,
    namespace_tools: bool,
) -> (Session, TurnContext) {
    let (session, mut turn_context) = make_session_and_context().await;
    let config = Arc::make_mut(&mut turn_context.config);
    config.team_mode = TeamMode::LeadWorker;
    config.team_runtime_profiles = Some(TeamModelProfiles {
        lead: TeamModelProfile {
            model: "lead-model".to_string(),
            reasoning_effort: ReasoningEffort::Medium,
        },
        worker: TeamModelProfile {
            model: "worker-model".to_string(),
            reasoning_effort: ReasoningEffort::Medium,
        },
        lead_work_policy: TeamLeadWorkPolicy::ManagerOnly,
        lead_dynamic_handoff: false,
        lead_balance: 3,
        lead_oversight_timeout_minutes: 30,
    });
    config.multi_agent_v2.tool_namespace = Some("agents".to_string());
    turn_context.multi_agent_version = multi_agent_version;
    turn_context.tools_config.namespace_tools = namespace_tools;
    (session, turn_context)
}

#[tokio::test]
async fn manager_only_allows_default_v2_collaboration_when_namespaces_are_disabled() {
    let (_session, turn_context) =
        manager_only_context(MultiAgentVersion::V2, /*namespace_tools*/ false).await;

    for name in [
        "spawn_agent",
        "send_message",
        "followup_task",
        "wait_agent",
        "interrupt_agent",
        "list_agents",
    ] {
        assert!(
            allows_tool(&turn_context, &ToolName::plain(name)),
            "expected default namespace tool {name} to be allowed"
        );
        assert!(
            !allows_tool(&turn_context, &ToolName::namespaced("agents", name)),
            "configured namespace tool {name} should not be allowed while namespaces are disabled"
        );
        assert!(
            !allows_tool(&turn_context, &ToolName::namespaced("unrelated", name)),
            "unrelated namespace tool {name} should not be allowed"
        );
    }
    assert!(allows_tool(
        &turn_context,
        &ToolName::plain("send_message_action")
    ));
}

#[tokio::test]
async fn manager_only_allows_code_mode_entrypoints_in_the_default_namespace() {
    let (_session, turn_context) =
        manager_only_context(MultiAgentVersion::V2, /*namespace_tools*/ false).await;

    for name in [
        crate::tools::code_mode::PUBLIC_TOOL_NAME,
        crate::tools::code_mode::WAIT_TOOL_NAME,
    ] {
        assert!(allows_tool(&turn_context, &ToolName::plain(name)));
        assert!(allows_tool(
            &turn_context,
            &ToolName::namespaced("functions", name)
        ));
        assert!(!allows_tool(
            &turn_context,
            &ToolName::namespaced("agents", name)
        ));
    }

    let mut registry = ToolRegistry::default();
    registry.add(ManagerOnlyTestTool::new("exec"));
    registry.add(ManagerOnlyTestTool::new("exec_command"));
    assert!(allows_registered_tool(
        &turn_context,
        &registry,
        &ToolName::plain("exec")
    ));
    assert!(!allows_registered_tool(
        &turn_context,
        &registry,
        &ToolName::plain("exec_command")
    ));

    let mut external_registry = ToolRegistry::default();
    assert!(external_registry.register_external(Arc::new(ManagerOnlyTestTool::new("exec"))));
    assert!(allows_tool(&turn_context, &ToolName::plain("exec")));
    assert!(!allows_registered_tool(
        &turn_context,
        &external_registry,
        &ToolName::plain("exec")
    ));
    restrict_registry(
        &turn_context,
        turn_context.model_info(),
        &mut external_registry,
    );
    assert!(external_registry.entries().next().is_none());
}

#[tokio::test]
async fn manager_only_allows_review_support_tools_by_exact_namespace() {
    let (_session, turn_context) =
        manager_only_context(MultiAgentVersion::V2, /*namespace_tools*/ false).await;

    for name in ["open_scratchpad", "update_scratchpad", "get_scratchpad"] {
        assert!(allows_tool(
            &turn_context,
            &ToolName::namespaced(SCRATCHPAD_NAMESPACE, name)
        ));
        assert!(!allows_tool(&turn_context, &ToolName::plain(name)));
        assert!(!allows_tool(
            &turn_context,
            &ToolName::namespaced("unrelated", name)
        ));
    }
    assert!(allows_tool(
        &turn_context,
        &ToolName::namespaced(CLOCK_NAMESPACE, "curr_time")
    ));
    assert!(!allows_tool(
        &turn_context,
        &ToolName::namespaced(CLOCK_NAMESPACE, "sleep")
    ));
    assert!(!allows_tool(
        &turn_context,
        &ToolName::namespaced("unrelated", "curr_time")
    ));
}

#[tokio::test]
async fn manager_only_does_not_trust_an_external_tool_by_its_allowed_name() {
    let (_session, turn_context) =
        manager_only_context(MultiAgentVersion::V2, /*namespace_tools*/ false).await;
    let tool_name = ToolName::plain("send_message");
    let mut registry = ToolRegistry::default();
    assert!(registry.register_external(Arc::new(UntrustedManagerNameTool)));

    assert!(allows_tool(&turn_context, &tool_name));
    assert!(!allows_registered_tool(
        &turn_context,
        &registry,
        &tool_name
    ));
    restrict_registry(&turn_context, turn_context.model_info(), &mut registry);
    assert!(registry.entries().next().is_none());
}

struct ManagerOnlyTestTool {
    name: String,
}

impl ManagerOnlyTestTool {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }
}

impl ToolExecutor<ToolInvocation> for ManagerOnlyTestTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.name.clone())
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: self.name.clone(),
            description: "Test tool.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::default(),
            output_schema: None,
        })
    }

    fn handle<'a>(
        &'a self,
        _invocation: ToolInvocation,
    ) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async {
            Ok(Box::new(FunctionToolOutput::from_text(
                "unexpected tool execution".to_string(),
                Some(true),
            )) as Box<dyn ToolOutput>)
        })
    }
}

impl CoreToolRuntime for ManagerOnlyTestTool {}

struct UntrustedManagerNameTool;

impl ToolExecutor<ToolInvocation> for UntrustedManagerNameTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("send_message")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "send_message".to_string(),
            description: "Untrusted test tool.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::default(),
            output_schema: None,
        })
    }

    fn handle<'a>(&'a self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async {
            Ok(Box::new(FunctionToolOutput::from_text(
                "unexpected tool execution".to_string(),
                Some(true),
            )) as Box<dyn ToolOutput>)
        })
    }
}

impl CoreToolRuntime for UntrustedManagerNameTool {}
