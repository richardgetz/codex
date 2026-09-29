//! Bounded context fragments for explicit Lead-to-Worker questions and replies.

use super::ContextualUserFragment;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;

pub(crate) struct WorkerQuestionRequest {
    question_id: String,
    question: String,
}

impl WorkerQuestionRequest {
    pub(crate) fn new(question_id: String, question: String) -> Self {
        Self {
            question_id,
            question,
        }
    }

    pub(crate) fn matches_sampling_input(question_id: &str, input: &[ResponseItem]) -> bool {
        let question_id_line = format!("Question ID: {question_id}\n");
        input.iter().any(|item| {
            let text_matches =
                |text: &str| Self::matches_text(text) && text.contains(&question_id_line);
            match item {
                ResponseItem::Message { role, content, .. } if role == "user" => {
                    content.iter().any(|content| {
                        matches!(content, ContentItem::InputText { text } if text_matches(text))
                    })
                }
                ResponseItem::AgentMessage { content, .. } => content.iter().any(|content| {
                    matches!(content, AgentMessageInputContent::InputText { text }
                        if text
                            .split_once("Payload:\n")
                            .is_some_and(|(_, payload)| text_matches(payload)))
                }),
                _ => false,
            }
        })
    }
}

impl ContextualUserFragment for WorkerQuestionRequest {
    fn role(&self) -> &'static str {
        "user"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.worker_question".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<worker_question>", "</worker_question>")
    }

    fn body(&self) -> String {
        format!(
            "Question ID: {}\nQuestion: {}\nAnswer this question briefly in your final response. That response will be sent to the Lead as a separate reply, and your Worker turn will continue with the assigned task.",
            self.question_id, self.question
        )
    }
}

#[cfg(test)]
#[path = "worker_question_tests.rs"]
mod tests;

pub(crate) struct WorkerQuestionAnswered {
    question_id: String,
}

pub(crate) struct WorkerQuestionReply {
    question_id: String,
    answer: String,
}

impl WorkerQuestionReply {
    pub(crate) fn new(question_id: String, answer: String) -> Self {
        Self {
            question_id,
            answer,
        }
    }
}

impl ContextualUserFragment for WorkerQuestionReply {
    fn role(&self) -> &'static str {
        "assistant"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.worker_question_reply".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<worker_question_reply>", "</worker_question_reply>")
    }

    fn body(&self) -> String {
        format!("Question ID: {}\nReply: {}", self.question_id, self.answer)
    }
}

impl WorkerQuestionAnswered {
    pub(crate) fn new(question_id: String) -> Self {
        Self { question_id }
    }
}

impl ContextualUserFragment for WorkerQuestionAnswered {
    fn role(&self) -> &'static str {
        "user"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.worker_question_answered".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<worker_question_answered>", "</worker_question_answered>")
    }

    fn body(&self) -> String {
        format!(
            "Your answer to question {} was delivered to the Lead. Continue the assigned task and report completion only when that work is finished.",
            self.question_id
        )
    }
}
