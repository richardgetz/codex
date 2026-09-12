use super::ActiveGoalStopReason;
use super::GoalAccountingState;
use super::GoalAnalytics;
use super::GoalBackgroundWaitClaim;
use super::GoalEventEmitter;
use super::GoalMetrics;
use super::GoalRuntimeConfig;
use super::GoalRuntimeHandle;
use codex_analytics::AnalyticsEventsClient;
use codex_extension_api::ExtensionData;
use codex_extension_api::NoopExtensionEventSink;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::protocol::TokenUsage;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_state::ThreadGoalStatus;
use codex_tools::ToolWaitHandle;
use codex_utils_absolute_path::test_support::PathExt;
use std::sync::Arc;
use std::sync::Weak;
use tempfile::TempDir;

#[tokio::test]
async fn completed_background_wait_preserves_terminal_handling_for_same_turn() -> anyhow::Result<()>
{
    for (reason, expected_status) in [
        (ActiveGoalStopReason::TurnError, ThreadGoalStatus::Blocked),
        (
            ActiveGoalStopReason::UsageLimit,
            ThreadGoalStatus::UsageLimited,
        ),
    ] {
        let tempdir = TempDir::new()?;
        let state_dbs = Arc::new(
            StateRuntime::init(
                SqliteConfig::new_for_testing(tempdir.path().abs()),
                "test-provider".to_string(),
            )
            .await?,
        );
        let thread_id = ThreadId::from_string("11111111-1111-4111-8111-111111111111")?;
        let goal = state_dbs
            .thread_goals()
            .replace_thread_goal(
                thread_id,
                "wait for the process",
                ThreadGoalStatus::Active,
                None,
            )
            .await?;
        let accounting_state = Arc::new(GoalAccountingState::default());
        let runtime = GoalRuntimeHandle::new(
            thread_id,
            Arc::clone(&state_dbs),
            GoalEventEmitter::new(Arc::new(NoopExtensionEventSink)),
            GoalMetrics::default(),
            Weak::new(),
            Arc::clone(&accounting_state),
            GoalRuntimeConfig {
                analytics: GoalAnalytics::new(AnalyticsEventsClient::disabled()),
                enabled: true,
                tools_available_for_thread: true,
                root_accounting_state: None,
            },
        );

        let turn_id = "turn-1";
        accounting_state.start_turn(turn_id, ModeKind::Default, &TokenUsage::default());
        let intent_generation = runtime.begin_background_wait_turn(turn_id).await;
        accounting_state.set_turn_intent_generation(turn_id, intent_generation);
        accounting_state.mark_turn_goal_active(turn_id, goal.goal_id.clone());

        let turn_store = ExtensionData::new(turn_id);
        runtime
            .capture_tool_wait_scope(&turn_store, "call-wait")
            .await;
        let wait_handle = ToolWaitHandle::new("42");
        runtime
            .register_background_wait(
                turn_id,
                &turn_store,
                "call-wait",
                &wait_handle,
            )
            .await;
        let wait = runtime
            .claim_background_wait()
            .await
            .expect("registered process should be claimable");
        let GoalBackgroundWaitClaim::Start(wait) = wait else {
            panic!("a new process wait should start a watcher");
        };
        assert!(
            runtime
                .complete_background_wait(wait.generation, &wait.process_ids)
                .await,
            "the watcher should complete the exact registered process set"
        );

        runtime.stop_active_goal_for_turn(turn_id, reason).await?;
        let goal = state_dbs
            .thread_goals()
            .get_thread_goal(thread_id)
            .await?
            .expect("goal should remain persisted");
        assert_eq!(expected_status, goal.status);
    }
    Ok(())
}
