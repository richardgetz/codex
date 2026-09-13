use std::time::Duration;

use async_channel::Sender;
use crate::agent::control::AgentControl;
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
    agent_control: AgentControl,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(THREAD_INBOUND_MESSAGE_POLL_INTERVAL).await;
            let Ok(_admission) = agent_control.begin_handoff_admission() else {
                // A blocked handoff is reversible. Keep polling so aborting the attempt does not
                // permanently disable durable inbound delivery.
                continue;
            };
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
            if !enqueue_claimed_messages(
                thread_id,
                &messages,
                &state_db,
                &tx_sub,
                &agent_control,
            )
            .await
            {
                return;
            }
        }
    });
}

async fn enqueue_claimed_messages(
    thread_id: ThreadId,
    messages: &[codex_state::ThreadInboundMessage],
    state_db: &state_db::StateDbHandle,
    tx_sub: &Sender<Submission>,
    agent_control: &AgentControl,
) -> bool {
    for (index, message) in messages.iter().enumerate() {
        if agent_control.handoff_admission_sealed() {
            for pending in &messages[index..] {
                let _ = state_db.unclaim_thread_inbound_message(&pending.id).await;
            }
            break;
        }
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
            id: message.id.clone(),
            client_user_message_id: None,
            op: Op::UserInput {
                items,
                additional_context: Default::default(),
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                thread_settings: Default::default(),
            },
            parent_turn_id: None,
            trace: None,
            root_turn_id: None,
        };
        if tx_sub.send(submission).await.is_err() {
            for pending in &messages[index..] {
                let _ = state_db.unclaim_thread_inbound_message(&pending.id).await;
            }
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::enqueue_claimed_messages;
    use super::start_thread_inbound_message_poller;
    use crate::agent::control::AgentControl;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::Op;
    use codex_protocol::protocol::Submission;
    use codex_protocol::user_input::UserInput;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;
    use std::time::Duration;
    use tokio::time::timeout;
    use uuid::Uuid;

    async fn create_poller_fixture(
        thread_id: ThreadId,
    ) -> (codex_rollout::state_db::StateDbHandle, std::path::PathBuf) {
        let codex_home =
            std::env::temp_dir().join(format!("codex-core-thread-inbox-test-{}", Uuid::new_v4()));
        let runtime = codex_state::StateRuntime::init(
            codex_state::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
            "test-provider".to_string(),
        )
        .await
        .expect("initialize runtime");
        let now = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, /*nsecs*/ 0)
            .expect("timestamp");
        runtime
            .upsert_thread(&codex_state::ThreadMetadata {
                originator: None,
                id: thread_id,
                rollout_path: codex_home.join(format!("rollout-{thread_id}.jsonl")),
                created_at: now,
                updated_at: now,
                recency_at: now,
                source: "cli".to_string(),
                agent_nickname: None,
                agent_role: None,
                agent_path: None,
                model_provider: "test-provider".to_string(),
                model: Some("gpt-test".to_string()),
                reasoning_effort: None,
                history_mode: Default::default(),
                thread_source: None,
                cwd: codex_home.clone(),
                cli_version: "0.0.0".to_string(),
                name: None,
                title: "target".to_string(),
                preview: None,
                section: None,
                section_position: None,
                section_entered_at: None,
                project_id: None,
                daybreak_enabled: None,
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
        (runtime, codex_home)
    }

    #[tokio::test]
    async fn thread_inbound_message_poller_injects_queued_user_input() {
        let (runtime, codex_home) = create_poller_fixture(
            ThreadId::from_string("00000000-0000-0000-0000-000000000201").expect("thread id"),
        )
        .await;
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000201").expect("thread id");
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
        let control = AgentControl::default();
        let handoff = control.begin_handoff().expect("seal handoff");
        let release_handoff = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            drop(handoff);
        });
        start_thread_inbound_message_poller(thread_id, runtime, tx_sub, control);
        let submission = timeout(Duration::from_secs(/*secs*/ 4), rx_sub.recv())
            .await
            .expect("receive queued message")
            .expect("submission channel open");
        match submission.op {
            Op::UserInput {
                items,
                additional_context,
                final_output_json_schema,
                responsesapi_client_metadata,
                thread_settings,
            } => {
                assert_eq!(items, input);
                assert!(additional_context.is_empty());
                assert_eq!(final_output_json_schema, None);
                assert_eq!(responsesapi_client_metadata, None);
                assert_eq!(thread_settings, Default::default());
            }
            other => panic!("expected user input submission, got {other:?}"),
        }

        release_handoff.await.expect("release handoff");
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn thread_inbound_message_poller_retries_after_seal_wins_after_claim() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000202").expect("thread id");
        let (runtime, codex_home) = create_poller_fixture(thread_id).await;
        let first_input = vec![UserInput::Text {
            text: "first inbound message".to_string(),
            text_elements: Vec::new(),
        }];
        let second_input = vec![UserInput::Text {
            text: "second inbound message".to_string(),
            text_elements: Vec::new(),
        }];
        for input in [&first_input, &second_input] {
            runtime
                .enqueue_thread_inbound_message(
                    thread_id,
                    /*source_thread_id*/ None,
                    serde_json::to_string(input).expect("serialize input"),
                )
                .await
                .expect("enqueue inbound message");
        }

        let (tx_sub, rx_sub) = async_channel::bounded(/*cap*/ 1);
        tx_sub
            .send(Submission {
                id: "sentinel".to_string(),
                client_user_message_id: None,
                op: Op::Shutdown,
                parent_turn_id: None,
                trace: None,
                root_turn_id: None,
            })
            .await
            .expect("fill submission channel");
        let control = AgentControl::default();
        start_thread_inbound_message_poller(thread_id, runtime, tx_sub, control.clone());
        // The full channel holds the poller after it claims both rows and sends the first one.
        // Sealing here exercises the post-claim branch; the second row must be unclaimed and
        // delivered after the reversible handoff releases.
        tokio::time::sleep(Duration::from_millis(2200)).await;
        let handoff = control.begin_handoff().expect("seal after claim");
        let sentinel = timeout(Duration::from_secs(/*secs*/ 1), rx_sub.recv())
            .await
            .expect("receive sentinel")
            .expect("submission channel open");
        assert_eq!(sentinel.id, "sentinel");
        let first = timeout(Duration::from_secs(/*secs*/ 2), rx_sub.recv())
            .await
            .expect("receive first inbound message")
            .expect("submission channel open");
        match first.op {
            Op::UserInput { items, .. } => assert_eq!(items, first_input),
            other => panic!("expected first user input submission, got {other:?}"),
        }
        drop(handoff);

        let second = timeout(Duration::from_secs(/*secs*/ 4), rx_sub.recv())
            .await
            .expect("receive retried inbound message")
            .expect("submission channel open");
        match second.op {
            Op::UserInput { items, .. } => assert_eq!(items, second_input),
            other => panic!("expected retried user input submission, got {other:?}"),
        }
        drop(rx_sub);
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn claimed_inbound_suffix_is_requeued_when_handoff_releases_between_indices() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000203").expect("thread id");
        let (runtime, codex_home) = create_poller_fixture(thread_id).await;
        for text in ["first inbound message", "second inbound message"] {
            let input = vec![UserInput::Text {
                text: text.to_string(),
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
        }
        let claimed = runtime
            .claim_pending_thread_inbound_messages(thread_id, /*limit*/ 20)
            .await
            .expect("claim inbound messages");
        assert_eq!(claimed.len(), 2);

        let (tx_sub, rx_sub) = async_channel::bounded(/*cap*/ 2);
        let control = AgentControl::default();
        let handoff = control.begin_handoff().expect("seal handoff");
        let release_handoff = tokio::spawn(async move {
            tokio::task::yield_now().await;
            drop(handoff);
        });
        assert!(enqueue_claimed_messages(
            thread_id,
            &claimed,
            &runtime,
            &tx_sub,
            &control,
        )
        .await);
        release_handoff.await.expect("release handoff");
        assert!(rx_sub.try_recv().is_err());

        let reclaimed = runtime
            .claim_pending_thread_inbound_messages(thread_id, /*limit*/ 20)
            .await
            .expect("reclaim inbound messages");
        assert_eq!(reclaimed.len(), 2);
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

}
