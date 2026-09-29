use super::WorkerQuestionRequest;
use crate::context::ContextualUserFragment;
use crate::context::InterAgentMessage;
use crate::context::InterAgentMessageType;
use codex_protocol::AgentPath;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ResponseItem;

#[test]
fn explicit_worker_question_sampling_input_requires_correlation() {
    assert!(!WorkerQuestionRequest::matches_sampling_input(
        "pending-question",
        &[],
    ));

    let stale_final_input = vec![ContextualUserFragment::into(WorkerQuestionRequest::new(
        "earlier-question".to_string(),
        "Old question".to_string(),
    ))];
    assert!(!WorkerQuestionRequest::matches_sampling_input(
        "pending-question",
        &stale_final_input,
    ));

    let correlated_question_input = vec![ContextualUserFragment::into(WorkerQuestionRequest::new(
        "pending-question".to_string(),
        "Current question".to_string(),
    ))];
    assert!(WorkerQuestionRequest::matches_sampling_input(
        "pending-question",
        &correlated_question_input,
    ));

    let v2_question = WorkerQuestionRequest::new(
        "pending-question".to_string(),
        "Current question".to_string(),
    );
    let v2_envelope = InterAgentMessage::new(
        InterAgentMessageType::NewTask,
        AgentPath::root().join("worker").expect("valid worker path"),
        AgentPath::root(),
        v2_question.render(),
    )
    .render();
    let v2_question_input = vec![ResponseItem::AgentMessage {
        id: None,
        author: "root".to_string(),
        recipient: "worker".to_string(),
        content: vec![AgentMessageInputContent::InputText { text: v2_envelope }],
        internal_chat_message_metadata_passthrough: None,
    }];
    assert!(WorkerQuestionRequest::matches_sampling_input(
        "pending-question",
        &v2_question_input,
    ));
}
