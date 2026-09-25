use crate::function_tool::FunctionCallError;
use crate::session::team::effective_role_for_session_source;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::multi_agents_common::function_arguments;
use crate::tools::handlers::multi_agents_common::tool_output_code_mode_result;
use crate::tools::handlers::multi_agents_common::tool_output_json_text;
use crate::tools::handlers::multi_agents_common::tool_output_response_item;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_config::TeamRole;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::TeamMode;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSearchSourceInfo;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::collections::BTreeMap;

const TOOL_NAME: &str = "worker_capacity";
const NAMESPACE_DESCRIPTION: &str = "Tools for spawning and managing sub-agents.";
const TOOL_DESCRIPTION: &str = "Read the current direct Worker concurrency budget for this Team Lead. Returns the configured direct-Worker limit, active direct Workers, pending spawns, and remaining direct slots. Grandchildren are excluded. This is a read-only snapshot and does not reserve a slot; capacity may change before a spawn, and runtime admission remains authoritative. Existing global agent-count, depth, and resource limits still apply.";

pub(crate) struct Handler {
    namespace: Option<String>,
}

impl Handler {
    pub(crate) fn new(namespace: Option<&str>) -> Self {
        Self {
            namespace: namespace.map(str::to_string),
        }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::new(self.namespace.clone(), TOOL_NAME.to_string())
    }

    fn spec(&self) -> ToolSpec {
        let function = ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::new(), Some(Vec::new()), Some(false.into())),
            output_schema: Some(
                json!({
                    "type": "object",
                    "properties": {
                        "direct_worker_limit": {"type": ["integer", "null"]},
                        "active_direct_workers": {"type": "integer"},
                        "pending_direct_spawns": {"type": "integer"},
                        "remaining_direct_slots": {"type": ["integer", "null"]}
                    },
                    "required": [
                        "direct_worker_limit",
                        "active_direct_workers",
                        "pending_direct_spawns",
                        "remaining_direct_slots"
                    ],
                    "additionalProperties": false
                })
                .into(),
            ),
        };
        match self.namespace.as_deref() {
            Some(namespace) => ToolSpec::Namespace(ResponsesApiNamespace {
                name: namespace.to_string(),
                description: NAMESPACE_DESCRIPTION.to_string(),
                tools: vec![ResponsesApiNamespaceTool::Function(function)],
            }),
            None => ToolSpec::Function(function),
        }
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        ToolSearchInfo::from_spec(
            "worker capacity direct workers concurrency limit remaining slots".to_string(),
            self.spec(),
            Some(ToolSearchSourceInfo {
                name: "Multi-agent tools".to_string(),
                description: Some("Spawn and manage sub-agents.".to_string()),
            }),
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                payload,
                ..
            } = invocation;
            let arguments = function_arguments(payload)?;
            let _: EmptyArguments = parse_arguments(&arguments)?;
            if turn.config.team_mode != TeamMode::LeadWorker
                || effective_role_for_session_source(&turn.config, &turn.session_source)
                    != Some(TeamRole::Lead)
            {
                return Err(FunctionCallError::RespondToModel(
                    "worker_capacity is available only to a Team Lead".to_string(),
                ));
            }
            let snapshot = session
                .services
                .agent_control
                .team_worker_capacity_snapshot();
            Ok(boxed_tool_output(WorkerCapacityOutput {
                direct_worker_limit: snapshot.max_concurrent,
                active_direct_workers: snapshot.active_workers,
                pending_direct_spawns: snapshot.pending_spawns,
                remaining_direct_slots: snapshot.remaining_slots,
            }))
        })
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArguments {}

#[derive(Debug, Serialize)]
struct WorkerCapacityOutput {
    direct_worker_limit: Option<usize>,
    active_direct_workers: usize,
    pending_direct_spawns: usize,
    remaining_direct_slots: Option<usize>,
}

impl ToolOutput for WorkerCapacityOutput {
    fn log_output(&self) -> String {
        tool_output_json_text(self, TOOL_NAME)
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), TOOL_NAME)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, TOOL_NAME)
    }
}
