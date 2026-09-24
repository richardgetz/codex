//! Handles persistent thread-settings updates and serializes their persistence
//! with compaction checkpoints.

use super::session::Session;
use super::session::SessionSettingsUpdate;
use super::step_settings::StepSettingsUpdate;
use crate::agent::control::HandoffAdmissionGuard;
use crate::config::ConstraintResult;
use codex_config::TeamLeadWorkPolicy;
use codex_history::RolloutItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::protocol::ThreadUsagePolicy;
use codex_protocol::protocol::ThreadUsagePolicyUpdate;
use codex_thread_store::ThreadStoreResult;
use std::sync::Arc;
use tokio::sync::SemaphorePermit;

impl Session {
    /// Persists the current settings snapshot without emitting a live settings event.
    ///
    /// Resume/revert paths use this checkpoint so effective runtime roots survive
    /// a cold restart even when no subsequent turn is submitted.
    pub(crate) async fn checkpoint_thread_settings(&self) -> ThreadStoreResult<()> {
        let _settings_guard = acquire_persistence_lock(self).await;
        if let Some(live_thread) = self.live_thread() {
            live_thread
                .append_items(&[RolloutItem::EventMsg(applied_event(self).await)])
                .await?;
            live_thread.flush().await?;
        }
        Ok(())
    }
}

/// Applies standalone thread settings and reports invalid overrides through the
/// normal event stream.
pub(super) async fn update(
    session: &Arc<Session>,
    submission_id: String,
    overrides: ThreadSettingsOverrides,
    usage_policy_update: Option<ThreadUsagePolicyUpdate>,
    handoff_admission: Option<&HandoffAdmissionGuard>,
) {
    let mut updates = prepare_update(overrides);
    updates.usage_policy_update = usage_policy_update;
    if let Err(error) =
        apply_update(session, submission_id.clone(), updates, handoff_admission).await
    {
        session
            .send_event_raw(Event {
                id: submission_id,
                msg: EventMsg::Error(ErrorEvent {
                    misalignment: None,
                    message: format!("invalid thread settings override: {error}"),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                }),
            })
            .await;
    } else {
        // Standalone settings changes supersede a pending automatic continuation.
        session.state.lock().await.last_started_turn_id = None;
    }
}

/// Converts protocol overrides into the internal settings update shape.
pub(super) fn prepare_update(overrides: ThreadSettingsOverrides) -> SessionSettingsUpdate {
    let ThreadSettingsOverrides {
        environments,
        runtime_workspace_roots,
        profile_workspace_roots,
        approval_policy,
        approvals_reviewer,
        sandbox_policy,
        permission_profile,
        active_permission_profile,
        windows_sandbox_level,
        model,
        effort,
        summary,
        service_tier,
        collaboration_mode,
        personality,
        disabled_plugin_ids,
        usage_policy,
        team,
    } = overrides;
    SessionSettingsUpdate {
        step_settings: StepSettingsUpdate {
            model,
            effort,
            collaboration_mode,
            reasoning_summary: summary,
            service_tier,
            personality,
            approval_policy,
            approvals_reviewer,
        },
        environments,
        runtime_workspace_roots,
        profile_workspace_roots,
        sandbox_policy,
        permission_profile,
        active_permission_profile,
        windows_sandbox_level,
        disabled_plugin_ids,
        usage_policy,
        team,
        ..Default::default()
    }
}

/// Acquires the shared permit before capturing or changing persistent settings.
pub(super) async fn acquire_persistence_lock(session: &Session) -> SemaphorePermit<'_> {
    session
        .thread_settings_persistence
        .acquire()
        .await
        .unwrap_or_else(|_| unreachable!("thread settings persistence semaphore is never closed"))
}

/// Applies persistent settings and emits the resulting thread-owned snapshot.
pub(super) async fn apply_update(
    session: &Arc<Session>,
    submission_id: String,
    updates: SessionSettingsUpdate,
    handoff_admission: Option<&HandoffAdmissionGuard>,
) -> ConstraintResult<()> {
    let _settings_guard = acquire_persistence_lock(session).await;
    let release_pending_manager_completions = updates
        .team
        .as_ref()
        .is_some_and(|team| team.lead_work_policy == Some(TeamLeadWorkPolicy::PromptGuided))
        && session.get_config().await.effective_team_lead_work_policy()
            == TeamLeadWorkPolicy::ManagerOnly;
    let commit = session.update_settings(updates).await?;
    emit_applied(
        session,
        submission_id,
        commit.snapshot,
        commit.usage_policy_changed,
    )
    .await;
    drop(_settings_guard);
    if release_pending_manager_completions
        && let Some(generation) = session
            .input_queue
            .pending_manager_completion_generation()
            .await
    {
        session
            .flush_manager_completion_batch(generation, handoff_admission)
            .await;
    }
    Ok(())
}

/// Emits the snapshot published by one successful settings update.
pub(super) async fn emit_applied(
    session: &Session,
    submission_id: String,
    snapshot: ThreadSettingsSnapshot,
    usage_policy_changed: bool,
) {
    let msg = EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
        thread_id: Some(session.thread_id()),
        thread_settings: snapshot,
    });
    let event = Event {
        id: submission_id,
        msg,
    };
    let EventMsg::ThreadSettingsApplied(applied) = &event.msg else {
        unreachable!("usage policy persistence only receives thread settings events");
    };
    if usage_policy_changed
        || applied.thread_settings.usage_policy != ThreadUsagePolicy::default()
        || applied.thread_settings.team.is_some()
    {
        // Usage policy and team state are thread-owned durable state. Materialize a lazy thread
        // when either is first enabled so a later cold resume or fork can recover it.
        session.send_event_raw(event).await;
    } else {
        session
            .send_event_raw_without_materializing_rollout(event)
            .await;
    }
}

/// Builds a current thread-owned snapshot for fork and compaction persistence.
pub(super) async fn applied_event(session: &Session) -> EventMsg {
    EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
        thread_id: Some(session.thread_id()),
        thread_settings: session.thread_settings_snapshot().await,
    })
}
