use crate::thread_state::ThreadState;
use codex_core::CodexThread;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_state::StateRuntime;
use codex_state::ThreadControlMode;
use codex_state::ThreadControlRecord;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tracing::warn;

pub(crate) async fn clear_router_tick(thread_state: &Arc<Mutex<ThreadState>>) {
    thread_state.lock().await.clear_router_tick();
}

pub(crate) async fn refresh_router_tick(
    conversation: Arc<CodexThread>,
    thread_state: Arc<Mutex<ThreadState>>,
    state_db: Arc<StateRuntime>,
) {
    let conversation_id = conversation.id();
    let control = match state_db.get_active_thread_control(conversation_id).await {
        Ok(control) => control,
        Err(err) => {
            warn!(
                thread_id = %conversation_id,
                "failed to load router control state: {err}"
            );
            conversation.active_thread_control().await
        }
    };

    let should_schedule = matches!(
        control,
        Some(ThreadControlRecord {
            mode: ThreadControlMode::Router,
            released_at: None,
            ..
        })
    );
    if !should_schedule {
        clear_router_tick(&thread_state).await;
        return;
    }

    let has_active_turn = {
        let state = thread_state.lock().await;
        state.active_turn_snapshot().is_some()
    };
    if has_active_turn || matches!(conversation.agent_status().await, AgentStatus::Running) {
        return;
    }

    let Some(control) = control else {
        return;
    };
    arm_router_tick(conversation, thread_state, state_db, control).await;
}

async fn arm_router_tick(
    conversation: Arc<CodexThread>,
    thread_state: Arc<Mutex<ThreadState>>,
    state_db: Arc<StateRuntime>,
    control: ThreadControlRecord,
) {
    let watch_interval_seconds = control.watch_interval_seconds.unwrap_or(0);
    if watch_interval_seconds == 0 {
        clear_router_tick(&thread_state).await;
        return;
    }

    let (cancel_tx, cancel_rx) = oneshot::channel();
    {
        let mut state = thread_state.lock().await;
        state.replace_router_tick(cancel_tx);
    }

    tokio::spawn(async move {
        let sleep = tokio::time::sleep(Duration::from_secs(u64::from(watch_interval_seconds)));
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {}
            _ = cancel_rx => return,
        }

        clear_router_tick(&thread_state).await;
        let active_control = match state_db.get_active_thread_control(control.thread_id).await {
            Ok(active_control) => active_control,
            Err(err) => {
                warn!(
                    thread_id = %control.thread_id,
                    "failed to revalidate router control before wake-up: {err}"
                );
                conversation.active_thread_control().await
            }
        };
        let Some(active_control) = active_control else {
            return;
        };
        if active_control != control || !matches!(active_control.mode, ThreadControlMode::Router) {
            return;
        }
        if matches!(conversation.agent_status().await, AgentStatus::Running) {
            return;
        }

        let message = build_router_tick_message(&control);
        if let Err(err) = conversation
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: message,
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
            })
            .await
        {
            warn!(
                thread_id = %control.thread_id,
                "failed to submit router wake-up turn: {err}"
            );
        }
    });
}

fn build_router_tick_message(control: &ThreadControlRecord) -> String {
    let mut lines = vec![
        "Router mode is still active for this thread.".to_string(),
        format!("Reason: {}", control.reason),
        format!(
            "Watch interval: {} seconds.",
            control.watch_interval_seconds.unwrap_or_default()
        ),
    ];
    if let Some(release_channel) = control.release_channel.as_deref() {
        lines.push(format!("Release channel: {release_channel}."));
    }
    if !control.target_thread_ids.is_empty() {
        let targets = control
            .target_thread_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("Monitored thread ids: {targets}."));
    }
    lines.push(
        "Check supervised sessions for new progress, blockers, or operator instructions and continue routing work."
            .to_string(),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::build_router_tick_message;
    use chrono::TimeZone;
    use chrono::Utc;
    use codex_protocol::ThreadId;
    use codex_state::ThreadControlMode;
    use codex_state::ThreadControlRecord;
    use pretty_assertions::assert_eq;

    #[test]
    fn router_tick_message_includes_release_channel_and_targets() {
        let updated_at = Utc
            .timestamp_opt(1_700_000_123, 0)
            .single()
            .expect("updated_at");
        let message = build_router_tick_message(&ThreadControlRecord {
            thread_id: ThreadId::from_string("00000000-0000-0000-0000-000000000011")
                .expect("thread id"),
            mode: ThreadControlMode::Router,
            reason: "Keep supervising the worker pool".to_string(),
            release_channel: Some("imessage".to_string()),
            watch_interval_seconds: Some(45),
            released_at: None,
            updated_at,
            target_thread_ids: vec![
                ThreadId::from_string("00000000-0000-0000-0000-000000000012").expect("target a"),
                ThreadId::from_string("00000000-0000-0000-0000-000000000013").expect("target b"),
            ],
        });

        assert_eq!(
            message,
            "Router mode is still active for this thread.\n\
Reason: Keep supervising the worker pool\n\
Watch interval: 45 seconds.\n\
Release channel: imessage.\n\
Monitored thread ids: 00000000-0000-0000-0000-000000000012, 00000000-0000-0000-0000-000000000013.\n\
Check supervised sessions for new progress, blockers, or operator instructions and continue routing work."
        );
    }
}
