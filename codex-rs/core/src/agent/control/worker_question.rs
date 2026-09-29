//! Tracks explicit parent questions until a Worker produces its next final response.

use super::LocalAgentControl;
use crate::TurnStartOptions;
use crate::agent::api::AgentControl as AgentControlTrait;
use crate::agent::api::AgentInput;
use crate::agent::api::AgentTarget;
use crate::agent::api::SendRequest;
use crate::agent::types::AgentMessage;
use crate::agent::types::MessageDeliveryMode;
use crate::config::Config;
use crate::context::ContextualUserFragment;
use crate::context::WorkerQuestionReply;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::user_input::UserInput;
use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;

const MAX_PENDING_WORKER_QUESTIONS: usize = 64;

#[derive(Clone)]
pub(crate) struct PendingWorkerQuestion {
    pub(crate) question_id: String,
    pub(crate) requester_thread_id: ThreadId,
}

#[derive(Default)]
pub(crate) struct WorkerQuestionRegistry {
    pending: Mutex<HashMap<ThreadId, PendingWorkerQuestion>>,
}

impl WorkerQuestionRegistry {
    fn register(
        &self,
        requester_thread_id: ThreadId,
        worker_thread_id: ThreadId,
    ) -> Result<String> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.contains_key(&worker_thread_id) {
            return Err(CodexErr::InvalidRequest(
                "this Worker already has an unanswered question".to_string(),
            ));
        }
        if pending.len() >= MAX_PENDING_WORKER_QUESTIONS {
            return Err(CodexErr::InvalidRequest(
                "too many unanswered Worker questions".to_string(),
            ));
        }
        let question_id = Uuid::now_v7().to_string();
        pending.insert(
            worker_thread_id,
            PendingWorkerQuestion {
                question_id: question_id.clone(),
                requester_thread_id,
            },
        );
        Ok(question_id)
    }

    fn get(&self, worker_thread_id: ThreadId) -> Option<PendingWorkerQuestion> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&worker_thread_id)
            .cloned()
    }

    fn remove(&self, worker_thread_id: ThreadId, question_id: &str) -> bool {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .get(&worker_thread_id)
            .is_some_and(|pending| pending.question_id == question_id)
        {
            pending.remove(&worker_thread_id);
            return true;
        }
        false
    }
}

impl LocalAgentControl {
    pub(crate) async fn register_worker_question(
        &self,
        requester_thread_id: ThreadId,
        worker_thread_id: ThreadId,
    ) -> Result<String> {
        let requester = self.ensure_agent_known(requester_thread_id)?;
        let worker = self.ensure_agent_known(worker_thread_id)?;
        let direct_child = match (requester.agent_path, worker.agent_path) {
            (Some(requester_path), Some(worker_path)) => worker_path
                .as_str()
                .rsplit_once('/')
                .is_some_and(|(parent, _)| parent == requester_path.as_str()),
            _ => {
                let state = self.upgrade()?;
                let worker_thread = state.get_thread(worker_thread_id).await?;
                worker_thread.session_source().parent_thread_id() == Some(requester_thread_id)
            }
        };
        if !direct_child {
            return Err(CodexErr::InvalidRequest(
                "questions can only be sent to a direct Worker".to_string(),
            ));
        }
        self.runtime
            .worker_questions
            .register(requester_thread_id, worker_thread_id)
    }

    pub(crate) fn pending_worker_question(
        &self,
        worker_thread_id: ThreadId,
    ) -> Option<PendingWorkerQuestion> {
        self.runtime.worker_questions.get(worker_thread_id)
    }

    pub(crate) fn remove_worker_question(
        &self,
        worker_thread_id: ThreadId,
        question_id: &str,
    ) -> bool {
        self.runtime
            .worker_questions
            .remove(worker_thread_id, question_id)
    }

    pub(crate) fn clear_worker_question(&self, worker_thread_id: ThreadId) {
        let Some(question) = self.pending_worker_question(worker_thread_id) else {
            return;
        };
        self.remove_worker_question(worker_thread_id, &question.question_id);
    }

    pub(crate) async fn forward_worker_question_reply(
        &self,
        worker_thread_id: ThreadId,
        answer: &str,
        version: MultiAgentVersion,
        resume_config: Config,
        start_options: TurnStartOptions,
    ) -> Result<bool> {
        let Some(question) = self.pending_worker_question(worker_thread_id) else {
            return Ok(false);
        };
        let answer = crate::session::truncate_message(answer);
        let message = WorkerQuestionReply::new(question.question_id.clone(), answer).render();
        match version {
            MultiAgentVersion::V1 => {
                self.send_input(
                    question.requester_thread_id,
                    vec![UserInput::Text {
                        text: message,
                        text_elements: Vec::new(),
                    }],
                    start_options,
                )
                .await?;
            }
            MultiAgentVersion::V2 => {
                <Self as AgentControlTrait>::send(
                    self,
                    SendRequest {
                        caller: worker_thread_id,
                        target: AgentTarget::Id(question.requester_thread_id),
                        resume_config,
                        input: AgentInput::Message {
                            message: AgentMessage::Plaintext(message),
                            mode: MessageDeliveryMode::Action,
                        },
                        start_options,
                    },
                )
                .await?;
            }
            MultiAgentVersion::Disabled => return Ok(false),
        }
        self.remove_worker_question(worker_thread_id, &question.question_id);
        Ok(true)
    }
}
