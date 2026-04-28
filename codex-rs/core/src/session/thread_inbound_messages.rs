use std::time::Duration;

use async_channel::Sender;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::Submission;
use codex_protocol::user_input::UserInput;
use codex_rollout::state_db;
use tracing::warn;

const THREAD_INBOUND_MESSAGE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const THREAD_INBOUND_MESSAGE_CLAIM_LIMIT: u32 = 20;

pub(super) fn start_thread_inbound_message_poller(
    thread_id: ThreadId,
    state_db: state_db::StateDbHandle,
    tx_sub: Sender<Submission>,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(THREAD_INBOUND_MESSAGE_POLL_INTERVAL).await;
            let messages = match state_db
                .claim_pending_thread_inbound_messages(
                    thread_id,
                    THREAD_INBOUND_MESSAGE_CLAIM_LIMIT,
                )
                .await
            {
                Ok(messages) => messages,
                Err(err) => {
                    warn!(%thread_id, "failed to claim thread inbound messages: {err}");
                    continue;
                }
            };
            for message in messages {
                let items = match serde_json::from_str::<Vec<UserInput>>(&message.payload_json) {
                    Ok(items) => items,
                    Err(err) => {
                        warn!(
                            %thread_id,
                            message_id = %message.id,
                            "failed to deserialize thread inbound message: {err}"
                        );
                        continue;
                    }
                };
                let submission = Submission {
                    id: message.id,
                    op: Op::UserInput {
                        items,
                        environments: None,
                        final_output_json_schema: None,
                        responsesapi_client_metadata: None,
                    },
                    trace: None,
                };
                if tx_sub.send(submission).await.is_err() {
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::start_thread_inbound_message_poller;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::Op;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;
    use std::time::Duration;
    use tokio::time::timeout;
    use uuid::Uuid;

    #[tokio::test]
    async fn thread_inbound_message_poller_injects_queued_user_input() {
        let codex_home =
            std::env::temp_dir().join(format!("codex-core-thread-inbox-test-{}", Uuid::new_v4()));
        let runtime =
            codex_state::StateRuntime::init(codex_home.clone(), "test-provider".to_string())
                .await
                .expect("initialize runtime");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000201").expect("thread id");
        let now = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, /*nsecs*/ 0)
            .expect("timestamp");
        runtime
            .upsert_thread(&codex_state::ThreadMetadata {
                id: thread_id,
                rollout_path: codex_home.join(format!("rollout-{thread_id}.jsonl")),
                created_at: now,
                updated_at: now,
                source: "cli".to_string(),
                agent_nickname: None,
                agent_role: None,
                agent_path: None,
                model_provider: "test-provider".to_string(),
                model: Some("gpt-test".to_string()),
                reasoning_effort: None,
                cwd: codex_home.clone(),
                cli_version: "0.0.0".to_string(),
                title: "target".to_string(),
                sandbox_policy: "read-only".to_string(),
                approval_mode: "on-request".to_string(),
                tokens_used: 0,
                first_user_message: None,
                archived_at: None,
                git_sha: None,
                git_branch: None,
                git_origin_url: None,
            })
            .await
            .expect("insert target thread");
        let input = vec![UserInput::Text {
            text: "continue from orchestrator".to_string(),
            text_elements: Vec::new(),
        }];
        runtime
            .enqueue_thread_inbound_message(
                thread_id,
                /*source_thread_id*/ None,
                serde_json::to_string(&input).expect("serialize input"),
            )
            .await
            .expect("enqueue inbound message");

        let (tx_sub, rx_sub) = async_channel::bounded(/*cap*/ 1);
        start_thread_inbound_message_poller(thread_id, runtime, tx_sub);
        let submission = timeout(Duration::from_secs(/*secs*/ 2), rx_sub.recv())
            .await
            .expect("receive queued message")
            .expect("submission channel open");
        assert_eq!(
            submission.op,
            Op::UserInput {
                items: input,
                environments: None,
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
            }
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }
}
