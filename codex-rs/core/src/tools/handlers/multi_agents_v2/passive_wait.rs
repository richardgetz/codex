use super::wait::{WaitOutcome, wait_for_activity, wait_outcome_for_activity};
use crate::session::LeadIdleArmMode;
use crate::session::LeadIdleDeadline;
use crate::session::format_lead_wait_message;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Parks a repeated Lead sleep on activity or an existing or newly armed oversight deadline.
pub(crate) async fn wait_for_lead_passive_poll(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    step_context: &Arc<StepContext>,
    deadline: Option<LeadIdleDeadline>,
    mut substantive_work_rx: tokio::sync::watch::Receiver<u64>,
    substantive_work_generation: u64,
    cancellation_token: &CancellationToken,
) -> WaitOutcome {
    if !session.is_team_lead().await {
        return WaitOutcome::TeamPolicyChanged;
    }
    if session.is_activity_paused() {
        return WaitOutcome::Paused;
    }
    let sampled_policy = *step_context.team_lead_work_policy.load_full();
    let active_workers = session
        .services
        .agent_control
        .active_direct_worker_count(session.thread_id)
        .await;
    if active_workers == 0 {
        session.cancel_lead_oversight().await;
        return WaitOutcome::NoActiveWorkers;
    }

    let deadline = match deadline {
        Some(deadline) => deadline,
        None => match session
            .arm_lead_oversight(LeadIdleArmMode::PassivePoll)
            .await
            .map(|(_, deadline)| deadline)
        {
            Some(deadline) => deadline,
            None => return WaitOutcome::LeadReviewRequired,
        },
    };

    let turn_state = session
        .input_queue
        .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
        .await;
    let (mut activity_rx, mut pending_activity) = session
        .input_queue
        .subscribe_activity(turn_state.as_deref())
        .await;
    if session.is_activity_paused() {
        return WaitOutcome::Paused;
    }
    let deadline_is_current = session
        .lead_oversight_deadline_is_current(deadline.instant)
        .await;
    let mut policy_changed = session
        .get_config()
        .await
        .effective_team_lead_work_policy()
        != sampled_policy;

    if deadline_is_current && session.lead_idle_notifications_enabled().await {
        session
            .emit_lead_idle_event(format_lead_wait_message(active_workers, deadline.unix_secs))
            .await;
    }

    loop {
        let outcome = if let Some(activity) = pending_activity.take() {
            wait_outcome_for_activity(activity)
        } else if policy_changed {
            policy_changed = false;
            WaitOutcome::TeamPolicyChanged
        } else if !session.is_team_lead().await {
            WaitOutcome::TeamPolicyChanged
        } else if session
            .services
            .agent_control
            .active_direct_worker_count(session.thread_id)
            .await
            == 0
        {
            WaitOutcome::NoActiveWorkers
        } else if *substantive_work_rx.borrow_and_update() != substantive_work_generation {
            WaitOutcome::SubstantiveWork
        } else {
            tokio::select! {
                biased;
                changed = substantive_work_rx.changed() => {
                    if changed.is_ok()
                        && *substantive_work_rx.borrow_and_update() != substantive_work_generation
                    {
                        WaitOutcome::SubstantiveWork
                    } else {
                        WaitOutcome::Cancelled
                    }
                }
                outcome = wait_for_activity(
                    &mut activity_rx,
                    None,
                    Some(deadline.instant),
                    Some(cancellation_token),
                ) => outcome,
            }
        };
        if session.is_activity_paused() {
            return WaitOutcome::Paused;
        }
        if outcome != WaitOutcome::TeamPolicyChanged {
            return outcome;
        }
        let live_team_lead = session.is_team_lead().await;
        let live_policy = session
            .get_config()
            .await
            .effective_team_lead_work_policy();
        if !live_team_lead || live_policy != sampled_policy {
            return WaitOutcome::TeamPolicyChanged;
        }
    }
}
