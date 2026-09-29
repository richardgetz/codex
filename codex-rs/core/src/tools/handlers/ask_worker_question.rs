//! Explicit parent-to-Worker questions with replies delivered independently of completion.

use crate::TurnStartOptions;
use crate::agent::agent_resolver::resolve_agent_target;
use crate::agent::api::AgentControl;
use crate::agent::api::AgentInput;
use crate::agent::api::AgentTarget;
use crate::agent::api::SendRequest;
use crate::agent::types::AgentMessage;
use crate::agent::types::MessageDeliveryMode;
use crate::context::ContextualUserFragment;
use crate::context::WorkerQuestionRequest;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::team::effective_role_for_session_source;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::multi_agents_common::function_arguments;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_config::TeamRole;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::user_input::UserInput;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

const MAX_QUESTION_BYTES: usize = 2_048;

pub(crate) struct Handler;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskWorkerQuestionArgs {
    target: String,
    question: String,
}

#[derive(Debug, Serialize)]
struct AskWorkerQuestionResult {
    question_id: String,
    target: String,
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("ask_worker_question")
    }

    fn spec(&self) -> ToolSpec {
        crate::tools::handlers::multi_agents_spec::create_ask_worker_question_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            ..
        } = invocation;
        if turn.config.team_mode != codex_protocol::protocol::TeamMode::LeadWorker
            || effective_role_for_session_source(&turn.config, &turn.session_source)
                != Some(TeamRole::Lead)
            || !session.is_team_lead().await
        {
            return Err(FunctionCallError::RespondToModel(
                "only a Team Lead can ask a Worker question".to_string(),
            ));
        }
        let arguments = function_arguments(payload)?;
        let args: AskWorkerQuestionArgs = parse_arguments(&arguments)?;
        if args.question.trim().is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "question must not be empty".to_string(),
            ));
        }
        if args.question.len() > MAX_QUESTION_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "question must be no more than {MAX_QUESTION_BYTES} bytes"
            )));
        }

        let worker_thread_id = resolve_agent_target(&session, &turn, &args.target).await?;
        let agent_control = &session.services.agent_control;
        let question_id = agent_control
            .register_worker_question(session.thread_id, worker_thread_id)
            .await
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let message = WorkerQuestionRequest::new(question_id.clone(), args.question).render();
        let start_options = TurnStartOptions {
            parent_turn_id: Some(turn.sub_id.clone()),
            root_turn_id: turn.turn_metadata_state.root_turn_id(),
            turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
            cyber_access_program: turn.cyber_access_program,
            ..Default::default()
        };
        let delivery =
            deliver_question(&session, &turn, worker_thread_id, message, start_options).await;
        if let Err(err) = delivery {
            agent_control.remove_worker_question(worker_thread_id, &question_id);
            return Err(FunctionCallError::RespondToModel(err));
        }

        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            serde_json::to_string(&AskWorkerQuestionResult {
                question_id,
                target: args.target,
            })
            .unwrap_or_else(|err| {
                json!({"error": format!("failed to serialize question result: {err}")}).to_string()
            }),
            Some(true),
        )))
    }
}

async fn deliver_question(
    session: &Session,
    turn: &TurnContext,
    worker_thread_id: codex_protocol::ThreadId,
    message: String,
    start_options: TurnStartOptions,
) -> Result<(), String> {
    let control = &session.services.agent_control;
    match turn.multi_agent_version {
        MultiAgentVersion::V1 => control
            .send_input(
                worker_thread_id,
                vec![UserInput::Text {
                    text: message,
                    text_elements: Vec::new(),
                }],
                start_options,
            )
            .await
            .map(|_| ())
            .map_err(|err| err.to_string()),
        MultiAgentVersion::V2 => {
            let resume_config = crate::agent::child_config::build_agent_resume_config(turn)?;
            control
                .send(SendRequest {
                    caller: session.thread_id,
                    target: AgentTarget::Id(worker_thread_id),
                    resume_config,
                    input: AgentInput::Message {
                        message: AgentMessage::Plaintext(message),
                        mode: MessageDeliveryMode::Action,
                    },
                    start_options,
                })
                .await
                .map(|_| ())
                .map_err(|err| err.to_string())
        }
        MultiAgentVersion::Disabled => Err("multi-agent support is disabled".to_string()),
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}
