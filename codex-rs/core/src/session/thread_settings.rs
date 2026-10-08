//! Handles persistent thread-settings updates and serializes their persistence
//! with checkpoints written directly to storage.

use super::session::Session;
use super::session::SessionSettingsUpdate;
use super::step_settings::StepSettingsUpdate;
use crate::agent::control::HandoffAdmissionGuard;
use crate::WithTurnExtensionData;
use crate::config::ConstraintResult;
use codex_config::TeamLeadWorkPolicy;
use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
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
use tokio::sync::oneshot;

impl Session {
    /// Captures and flushes current settings under the shared persistence permit.
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

/// Applies standalone thread settings and holds the persistence permit through event delivery.
pub(super) async fn update(
    session: &Arc<Session>,
    submission_id: String,
    overrides: impl Into<WithTurnExtensionData<ThreadSettingsOverrides>>,
    usage_policy_update: Option<ThreadUsagePolicyUpdate>,
    handoff_admission: Option<&HandoffAdmissionGuard>,
    reply: Option<oneshot::Sender<CodexResult<()>>>,
) {
    let mut updates = prepare_update(overrides);
    updates.usage_policy_update = usage_policy_update;
    if let Err(error) = apply_update_with_policy(
        session,
        submission_id.clone(),
        updates,
        handoff_admission,
        PendingContinuationUpdate::Supersede,
        reply,
    )
    .await
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
    }
}

/// Converts protocol overrides into the internal settings update shape.
pub(super) fn prepare_update(
    overrides: impl Into<WithTurnExtensionData<ThreadSettingsOverrides>>,
) -> SessionSettingsUpdate {
    let WithTurnExtensionData {
        request: overrides,
        turn_extension_init,
    } = overrides.into();
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
        turn_extension_init,
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
) -> ConstraintResult<()> {
    apply_update_with_policy(
        session,
        submission_id,
        updates,
        None,
        PendingContinuationUpdate::Preserve,
        None,
    )
    .await
}

/// Applies settings while keeping an admitted operation live through any completion flush.
pub(super) async fn apply_update_with_admission(
    session: &Arc<Session>,
    submission_id: String,
    updates: SessionSettingsUpdate,
    handoff_admission: Option<&HandoffAdmissionGuard>,
) -> ConstraintResult<()> {
    apply_update_with_policy(
        session,
        submission_id,
        updates,
        handoff_admission,
        PendingContinuationUpdate::Preserve,
        None,
    )
    .await
}

#[derive(Clone, Copy)]
enum PendingContinuationUpdate {
    Preserve,
    Supersede,
}

async fn apply_update_with_policy(
    session: &Arc<Session>,
    submission_id: String,
    updates: SessionSettingsUpdate,
    handoff_admission: Option<&HandoffAdmissionGuard>,
    pending_continuation_update: PendingContinuationUpdate,
    reply: Option<oneshot::Sender<CodexResult<()>>>,
) -> ConstraintResult<()> {
    let _settings_guard = acquire_persistence_lock(session).await;
    let release_pending_manager_completions = updates
        .team
        .as_ref()
        .is_some_and(|team| team.lead_work_policy == Some(TeamLeadWorkPolicy::PromptGuided))
        && session.get_config().await.effective_team_lead_work_policy()
            == TeamLeadWorkPolicy::ManagerOnly;
    let commit = match session.update_settings(updates).await {
        Ok(commit) => commit,
        Err(error) => {
            if let Some(reply) = reply {
                let message = format!("invalid thread settings override: {error}");
                let _ = reply.send(Err(CodexErr::InvalidRequest(message)));
                return Ok(());
            }
            return Err(error);
        }
    };
    if matches!(
        pending_continuation_update,
        PendingContinuationUpdate::Supersede
    ) {
        session.state.lock().await.last_started_turn_id = None;
    }
    if let Some(reply) = reply {
        let _ = reply.send(Ok(()));
    }
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
    let should_materialize = usage_policy_changed
        || snapshot.usage_policy != ThreadUsagePolicy::default()
        || snapshot.team.is_some();
    let msg = EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
        thread_id: Some(session.thread_id()),
        thread_settings: snapshot,
    });
    let event = Event {
        id: submission_id,
        msg,
    };
    if should_materialize {
        session.send_event_raw(event).await;
    } else {
        session
            .send_event_raw_without_materializing_rollout(event)
            .await;
    }
}

/// Builds a current thread-owned snapshot for storage checkpoints.
pub(super) async fn applied_event(session: &Session) -> EventMsg {
    EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
        thread_id: Some(session.thread_id()),
        thread_settings: session.thread_settings_snapshot().await,
    })
}
