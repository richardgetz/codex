use crate::thread_state::ThreadState;
use codex_core::CodexThread;
use codex_core::OrchestratorSupervisionPollState;
use codex_core::ThreadConfigSnapshot;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_state::StateRuntime;
use codex_state::ThreadControlMode;
use codex_state::ThreadControlRecord;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub(crate) async fn clear_router_tick(thread_state: &Arc<Mutex<ThreadState>>) {
    thread_state.lock().await.cancel_router_tick();
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
                "failed to load orchestrator control state: {err}"
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

    let cancel_token = {
        let mut state = thread_state.lock().await;
        state.replace_router_tick()
    };

    spawn_router_tick_task(
        conversation,
        thread_state,
        state_db,
        control,
        watch_interval_seconds,
        cancel_token,
    );
}

fn spawn_router_tick_task(
    conversation: Arc<CodexThread>,
    thread_state: Arc<Mutex<ThreadState>>,
    state_db: Arc<StateRuntime>,
    control: ThreadControlRecord,
    watch_interval_seconds: u32,
    cancel_token: CancellationToken,
) {
    tokio::spawn(async move {
        let sleep = tokio::time::sleep(Duration::from_secs(u64::from(watch_interval_seconds)));
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {}
            _ = cancel_token.cancelled() => return,
        }
        let active_control = match state_db.get_active_thread_control(control.thread_id).await {
            Ok(active_control) => active_control,
            Err(err) => {
                warn!(
                    thread_id = %control.thread_id,
                    "failed to revalidate orchestrator control before wake-up: {err}"
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
        if cancel_token.is_cancelled() {
            return;
        }
        if matches!(conversation.agent_status().await, AgentStatus::Running) {
            return;
        }

        let latest_control = match state_db.get_active_thread_control(control.thread_id).await {
            Ok(latest_control) => latest_control,
            Err(err) => {
                warn!(
                    thread_id = %control.thread_id,
                    "failed to confirm orchestrator control immediately before wake-up submit: {err}"
                );
                conversation.active_thread_control().await
            }
        };
        if latest_control != Some(control.clone()) {
            return;
        }
        if cancel_token.is_cancelled() {
            return;
        }
        let poll_state = match conversation.orchestrator_supervision_poll_state().await {
            Ok(poll_state) => poll_state,
            Err(err) => {
                warn!(
                    thread_id = %control.thread_id,
                    "failed to inspect orchestrator supervision state before wake-up: {err}"
                );
                arm_router_tick(Arc::clone(&conversation), thread_state, state_db, control).await;
                return;
            }
        };
        let active_agent_checkin_interval = Duration::from_secs(u64::from(
            conversation
                .orchestrator_active_agent_checkin_seconds()
                .await,
        ));
        let should_submit = {
            let mut state = thread_state.lock().await;
            should_submit_router_tick(&mut state, &poll_state, active_agent_checkin_interval)
        };
        if !should_submit {
            arm_router_tick(Arc::clone(&conversation), thread_state, state_db, control).await;
            return;
        }

        let config_snapshot = conversation.config_snapshot().await;
        let (model, reasoning_effort, collaboration_mode) =
            conversation.resolve_router_turn_settings().await;
        let latest_control = match state_db.get_active_thread_control(control.thread_id).await {
            Ok(latest_control) => latest_control,
            Err(err) => {
                warn!(
                    thread_id = %control.thread_id,
                    "failed to revalidate orchestrator control after resolving wake-up settings: {err}"
                );
                conversation.active_thread_control().await
            }
        };
        if latest_control != Some(control.clone()) || cancel_token.is_cancelled() {
            return;
        }
        let submit = conversation.submit(build_router_tick_turn(
            &control,
            &config_snapshot,
            &model,
            reasoning_effort,
            &collaboration_mode,
        ));
        tokio::pin!(submit);
        let submit_result = tokio::select! {
            _ = cancel_token.cancelled() => return,
            result = &mut submit => result,
        };
        if let Err(err) = submit_result {
            warn!(
                thread_id = %control.thread_id,
                "failed to submit orchestrator wake-up turn: {err}"
            );
            if !cancel_token.is_cancelled() {
                arm_router_tick(Arc::clone(&conversation), thread_state, state_db, control).await;
            }
        }
    });
}

fn should_submit_router_tick(
    thread_state: &mut ThreadState,
    poll_state: &OrchestratorSupervisionPollState,
    active_agent_checkin_interval: Duration,
) -> bool {
    if !poll_state.has_supervised_workers {
        return false;
    }

    let supervision_changed =
        thread_state.last_router_supervision_updated_at != poll_state.last_updated_at;
    if supervision_changed {
        thread_state.last_router_supervision_updated_at = poll_state.last_updated_at.clone();
        thread_state.last_router_model_check_at = Some(Instant::now());
        return true;
    }

    if !poll_state.has_nonterminal_workers || active_agent_checkin_interval.is_zero() {
        return false;
    }

    let due = thread_state
        .last_router_model_check_at
        .is_none_or(|last_check| last_check.elapsed() >= active_agent_checkin_interval);
    if due {
        thread_state.last_router_model_check_at = Some(Instant::now());
    }
    due
}

fn build_router_tick_message(control: &ThreadControlRecord) -> String {
    let mut lines = vec![
        "Orchestrator mode is still active for this thread.".to_string(),
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
        "Check supervised sessions directly for new progress, blockers, or operator instructions; do not spawn a child agent just to inspect supervision state."
            .to_string(),
    );
    lines.join("\n")
}

fn build_router_tick_turn(
    control: &ThreadControlRecord,
    config_snapshot: &ThreadConfigSnapshot,
    model: &str,
    reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    collaboration_mode: &CollaborationMode,
) -> Op {
    Op::UserTurn {
        items: vec![UserInput::Text {
            text: build_router_tick_message(control),
            text_elements: Vec::new(),
        }],
        cwd: config_snapshot.cwd.clone().to_path_buf(),
        approval_policy: config_snapshot.approval_policy,
        approvals_reviewer: Some(config_snapshot.approvals_reviewer),
        sandbox_policy: config_snapshot.sandbox_policy.clone(),
        model: model.to_string(),
        effort: reasoning_effort,
        summary: None,
        service_tier: None,
        final_output_json_schema: None,
        collaboration_mode: Some(collaboration_mode.clone()),
        personality: None,
        environments: None,
    }
}

#[cfg(test)]
mod tests {
    use super::build_router_tick_message;
    use super::build_router_tick_turn;
    use super::should_submit_router_tick;
    use crate::thread_state::ThreadState;
    use chrono::TimeZone;
    use chrono::Utc;
    use codex_core::OrchestratorSupervisionPollState;
    use codex_core::ThreadConfigSnapshot;
    use codex_protocol::ThreadId;
    use codex_protocol::config_types::ApprovalsReviewer;
    use codex_protocol::config_types::CollaborationMode;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::config_types::Personality;
    use codex_protocol::config_types::Settings;
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::Op;
    use codex_protocol::protocol::SandboxPolicy;
    use codex_protocol::protocol::SessionSource;
    use codex_state::ThreadControlMode;
    use codex_state::ThreadControlRecord;
    use pretty_assertions::assert_eq;
    use std::time::Duration;
    use std::time::Instant;

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
            "Orchestrator mode is still active for this thread.\n\
Reason: Keep supervising the worker pool\n\
Watch interval: 45 seconds.\n\
Release channel: imessage.\n\
Monitored thread ids: 00000000-0000-0000-0000-000000000012, 00000000-0000-0000-0000-000000000013.\n\
Check supervised sessions directly for new progress, blockers, or operator instructions; do not spawn a child agent just to inspect supervision state."
        );
    }

    #[test]
    fn router_tick_turn_uses_resolved_router_settings() {
        let control = ThreadControlRecord {
            thread_id: ThreadId::from_string("00000000-0000-0000-0000-000000000011")
                .expect("thread id"),
            mode: ThreadControlMode::Router,
            reason: "Keep supervising the worker pool".to_string(),
            release_channel: Some("imessage".to_string()),
            watch_interval_seconds: Some(45),
            released_at: None,
            updated_at: Utc
                .timestamp_opt(1_700_000_123, 0)
                .single()
                .expect("updated_at"),
            target_thread_ids: vec![],
        };
        let config_snapshot = ThreadConfigSnapshot {
            model: "gpt-5".to_string(),
            model_provider_id: "openai".to_string(),
            service_tier: None,
            approval_policy: AskForApproval::OnRequest,
            approvals_reviewer: ApprovalsReviewer::User,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            permission_profile: codex_protocol::models::PermissionProfile::default(),
            cwd: codex_utils_absolute_path::AbsolutePathBuf::try_from(std::path::PathBuf::from(
                "/tmp/router",
            ))
            .expect("absolute path"),
            ephemeral: false,
            reasoning_effort: Some(ReasoningEffort::High),
            personality: Some(Personality::Friendly),
            session_source: SessionSource::default(),
        };
        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Plan,
            settings: Settings {
                model: "gpt-5.3-codex-spark".to_string(),
                reasoning_effort: Some(ReasoningEffort::Medium),
                developer_instructions: Some("Stay in routing mode.".to_string()),
            },
        };

        let turn = build_router_tick_turn(
            &control,
            &config_snapshot,
            "gpt-5.3-codex-spark",
            Some(ReasoningEffort::Medium),
            &collaboration_mode,
        );

        let Op::UserTurn {
            model,
            effort,
            collaboration_mode,
            approvals_reviewer,
            sandbox_policy,
            cwd,
            ..
        } = turn
        else {
            panic!("expected router tick to submit a user turn");
        };
        assert_eq!(model, "gpt-5.3-codex-spark");
        assert_eq!(effort, Some(ReasoningEffort::Medium));
        assert_eq!(
            collaboration_mode,
            Some(CollaborationMode {
                mode: ModeKind::Plan,
                settings: Settings {
                    model: "gpt-5.3-codex-spark".to_string(),
                    reasoning_effort: Some(ReasoningEffort::Medium),
                    developer_instructions: Some("Stay in routing mode.".to_string()),
                },
            })
        );
        assert_eq!(approvals_reviewer, Some(ApprovalsReviewer::User));
        assert_eq!(sandbox_policy, SandboxPolicy::DangerFullAccess);
        assert_eq!(cwd, std::path::PathBuf::from("/tmp/router"));
    }

    #[test]
    fn router_tick_skips_when_no_supervised_workers_exist() {
        let mut thread_state = ThreadState::default();
        let poll_state = OrchestratorSupervisionPollState {
            last_updated_at: None,
            has_supervised_workers: false,
            has_nonterminal_workers: false,
        };

        assert!(!should_submit_router_tick(
            &mut thread_state,
            &poll_state,
            Duration::from_secs(600),
        ));
    }

    #[test]
    fn router_tick_skips_when_supervision_is_unchanged_and_no_active_workers_remain() {
        let mut thread_state = ThreadState::default();
        let poll_state = OrchestratorSupervisionPollState {
            last_updated_at: Some("2026-04-24T12:00:00Z".to_string()),
            has_supervised_workers: true,
            has_nonterminal_workers: false,
        };

        assert!(should_submit_router_tick(
            &mut thread_state,
            &poll_state,
            Duration::from_secs(600),
        ));
        assert!(!should_submit_router_tick(
            &mut thread_state,
            &poll_state,
            Duration::from_secs(600),
        ));
    }

    #[test]
    fn router_tick_allows_periodic_checkins_for_unchanged_active_workers() {
        let mut thread_state = ThreadState::default();
        let poll_state = OrchestratorSupervisionPollState {
            last_updated_at: Some("2026-04-24T12:00:00Z".to_string()),
            has_supervised_workers: true,
            has_nonterminal_workers: true,
        };

        assert!(should_submit_router_tick(
            &mut thread_state,
            &poll_state,
            Duration::from_secs(600),
        ));
        assert!(!should_submit_router_tick(
            &mut thread_state,
            &poll_state,
            Duration::from_secs(600),
        ));
        thread_state.last_router_model_check_at = Some(
            Instant::now()
                .checked_sub(Duration::from_secs(601))
                .expect("subtract duration"),
        );
        assert!(should_submit_router_tick(
            &mut thread_state,
            &poll_state,
            Duration::from_secs(600),
        ));
    }
}
