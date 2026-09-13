use std::time::Duration;

use async_channel::Sender;
use crate::agent::control::AgentControl;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::Submission;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use codex_rollout::state_db;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

const THREAD_INBOUND_MESSAGE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const THREAD_INBOUND_MESSAGE_UNSUPPORTED_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const THREAD_INBOUND_MESSAGE_CLAIM_LIMIT: u32 = 20;
const HANDOFF_INBOUND_MESSAGE_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum DurableInboundMessage {
    InterAgentCommunication {
        schema_version: u8,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
        team_lead_completion: bool,
    },
}

enum DecodedInboundMessage {
    UserInput(Vec<UserInput>),
    InterAgentCommunication {
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
        team_lead_completion: bool,
    },
}

/// Persist a sealed-handoff inter-agent delivery so a replacement daemon can claim it from the
/// state database. The identity is derived from the complete envelope and sender/recipient IDs,
/// making watcher retries idempotent without replaying a model turn or synthetic user prompt.
pub(crate) async fn persist_handoff_inter_agent_communication(
    state_db: &state_db::StateDbHandle,
    target_thread_id: ThreadId,
    source_thread_id: Option<ThreadId>,
    communication: &InterAgentCommunication,
    start_options: &TurnStartOptions,
    team_lead_completion: bool,
) -> anyhow::Result<String> {
    let envelope = DurableInboundMessage::InterAgentCommunication {
        schema_version: HANDOFF_INBOUND_MESSAGE_SCHEMA_VERSION,
        communication: communication.clone(),
        start_options: start_options.clone(),
        team_lead_completion,
    };
    let payload_json = serde_json::to_string(&envelope)?;
    let identity = format!(
        "codex-handoff-inbound:{target_thread_id}:{}:{payload_json}",
        source_thread_id
            .map(|thread_id| thread_id.to_string())
            .unwrap_or_default()
    );
    let message_id =
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, identity.as_bytes()).to_string();
    state_db
        .enqueue_thread_inbound_message_with_id(
            message_id.clone(),
            target_thread_id,
            source_thread_id,
            payload_json,
        )
        .await?;
    Ok(message_id)
}

fn decode_inbound_message(payload_json: &str) -> anyhow::Result<DecodedInboundMessage> {
    if let Ok(DurableInboundMessage::InterAgentCommunication {
        schema_version,
        communication,
        start_options,
        team_lead_completion,
    }) = serde_json::from_str(payload_json)
    {
        if schema_version != HANDOFF_INBOUND_MESSAGE_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported handoff inbound message schema version {schema_version}"
            );
        }
        return Ok(DecodedInboundMessage::InterAgentCommunication {
            communication,
            start_options,
            team_lead_completion,
        });
    }
    Ok(DecodedInboundMessage::UserInput(
        serde_json::from_str(payload_json)?,
    ))
}

pub(super) fn start_thread_inbound_message_poller(
    thread_id: ThreadId,
    state_db: state_db::StateDbHandle,
    tx_sub: Sender<Submission>,
    agent_control: AgentControl,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(THREAD_INBOUND_MESSAGE_POLL_INTERVAL).await;
            if agent_control.handoff_inbound_unsupported() {
                // Leave the row pending but avoid reclaiming it every second. A replacement with a
                // compatible schema gets a fresh control handle and retries it from the database.
                tokio::time::sleep(THREAD_INBOUND_MESSAGE_UNSUPPORTED_RETRY_INTERVAL).await;
                continue;
            }
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
                if let Err(error) = state_db.unclaim_thread_inbound_message(&pending.id).await {
                    agent_control.mark_handoff_delivery_failed();
                    warn!(
                        %thread_id,
                        message_id = %pending.id,
                        %error,
                        "failed to return sealed inbound message to pending queue"
                    );
                }
            }
            break;
        }
        let (op, parent_turn_id, root_turn_id) = match decode_inbound_message(&message.payload_json)
        {
            Ok(DecodedInboundMessage::UserInput(items)) => (
                Op::UserInput {
                    items,
                    additional_context: Default::default(),
                    final_output_json_schema: None,
                    responsesapi_client_metadata: None,
                    thread_settings: Default::default(),
                },
                None,
                None,
            ),
            Ok(DecodedInboundMessage::InterAgentCommunication {
                communication,
                start_options,
                team_lead_completion,
            }) => {
                let parent_turn_id = start_options.parent_turn_id.clone();
                let root_turn_id = start_options.root_turn_id.clone();
                let op = if team_lead_completion {
                    Op::TeamLeadCompletion {
                        communication,
                        start_options,
                    }
                } else {
                    Op::InterAgentCommunication {
                        communication,
                        start_options,
                    }
                };
                (op, parent_turn_id, root_turn_id)
            }
            Err(err) => {
                // Keep unsupported durable data pending for a compatible replacement. Replaying it
                // as user input would invent model work, while consuming it would lose the receipt.
                warn!(
                    %thread_id,
                    message_id = %message.id,
                    "failed to deserialize durable thread inbound message: {err}"
                );
                agent_control.mark_handoff_inbound_unsupported();
                if let Err(unclaim_error) =
                    state_db.unclaim_thread_inbound_message(&message.id).await
                {
                    agent_control.mark_handoff_delivery_failed();
                    warn!(
                        %thread_id,
                        message_id = %message.id,
                        %unclaim_error,
                        "failed to return unsupported durable inbound message to pending queue"
                    );
                }
                continue;
            }
        };
        let submission = Submission {
            id: message.id.clone(),
            client_user_message_id: None,
            op,
            parent_turn_id,
            trace: None,
            root_turn_id,
        };
        if tx_sub.send(submission).await.is_err() {
            for pending in &messages[index..] {
                if let Err(error) = state_db.unclaim_thread_inbound_message(&pending.id).await {
                    agent_control.mark_handoff_delivery_failed();
                    warn!(
                        %thread_id,
                        message_id = %pending.id,
                        %error,
                        "failed to return inbound message after submission channel closed"
                    );
                }
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
    use codex_protocol::AgentPath;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::InterAgentCommunication;
    use codex_protocol::protocol::Op;
    use codex_protocol::protocol::Submission;
    use codex_protocol::turn_input::TurnStartOptions;
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
    async fn durable_handoff_communication_is_delivered_after_replacement() {
        let target_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000204").expect("target");
        let source_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000205").expect("source");
        let (runtime, codex_home) = create_poller_fixture(target_thread_id).await;
        let communication = InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::root(),
            Vec::new(),
            "worker completed while handoff was sealed".to_string(),
            true,
        );
        let start_options = TurnStartOptions {
            parent_turn_id: Some("parent-turn".to_string()),
            root_turn_id: Some("root-turn".to_string()),
            ..Default::default()
        };
        let message_id = persist_handoff_inter_agent_communication(
            &runtime,
            target_thread_id,
            Some(source_thread_id),
            &communication,
            &start_options,
            true,
        )
        .await
        .expect("persist handoff communication");
        let claimed = runtime
            .claim_pending_thread_inbound_messages(target_thread_id, /*limit*/ 1)
            .await
            .expect("claim handoff communication");
        let (tx_sub, rx_sub) = async_channel::bounded(/*cap*/ 1);
        let control = AgentControl::default();
        assert!(enqueue_claimed_messages(
            target_thread_id,
            &claimed,
            &runtime,
            &tx_sub,
            &control,
        )
        .await);
        let submission = rx_sub.recv().await.expect("receive handoff communication");
        assert_eq!(submission.id, message_id);
        match submission.op {
            Op::TeamLeadCompletion {
                communication: recovered,
                start_options: recovered_options,
            } => {
                assert_eq!(recovered, communication);
                assert_eq!(recovered_options.parent_turn_id, start_options.parent_turn_id);
                assert_eq!(recovered_options.root_turn_id, start_options.root_turn_id);
            }
            other => panic!("expected Team Lead completion, got {other:?}"),
        }

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn unsupported_durable_message_stays_pending_for_compatible_runtime() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000206").expect("thread id");
        let (runtime, codex_home) = create_poller_fixture(thread_id).await;
        runtime
            .enqueue_thread_inbound_message(
                thread_id,
                /*source_thread_id*/ None,
                r#"{"type":"futureInterAgentCommunication","schemaVersion":9}"#.to_string(),
            )
            .await
            .expect("enqueue unsupported message");
        let claimed = runtime
            .claim_pending_thread_inbound_messages(thread_id, /*limit*/ 1)
            .await
            .expect("claim unsupported message");
        let (tx_sub, _rx_sub) = async_channel::bounded(/*cap*/ 1);
        let control = AgentControl::default();
        assert!(enqueue_claimed_messages(
            thread_id,
            &claimed,
            &runtime,
            &tx_sub,
            &control,
        )
        .await);
        assert!(control.handoff_inbound_unsupported());
        let pending = runtime
            .claim_pending_thread_inbound_messages(thread_id, /*limit*/ 1)
            .await
            .expect("reclaim unsupported message");
        assert_eq!(pending.len(), 1);

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
