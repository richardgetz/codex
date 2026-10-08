use super::super::session::Session;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use std::sync::Arc;

pub(in crate::session) async fn continue_usage(session: &Arc<Session>, submission_id: String) {
    let waiting = session
        .services
        .local_agent_control()
        .request_usage_resume_for_subtree(session.thread_id())
        .await
        > 0;
    let message = if waiting {
        "Requested an immediate usage check for the paused work."
    } else {
        "No usage-paused work is waiting for a usage check."
    };
    session
        .send_event_raw_without_materializing_rollout(Event {
            id: submission_id,
            msg: EventMsg::Warning(WarningEvent {
                message: message.to_string(),
            }),
        })
        .await;
}

pub(in crate::session) async fn pause_activity(session: &Arc<Session>, submission_id: String) {
    let snapshots = session
        .services
        .local_agent_control()
        .pause_activity_for_subtree()
        .await;
    session
        .send_event_raw_without_materializing_rollout(Event {
            id: submission_id,
            msg: EventMsg::Warning(WarningEvent {
                message: format!(
                    "Paused activity for {} loaded thread{}; in-flight operations finish at their cooperative boundary.",
                    snapshots.len(),
                    if snapshots.len() == 1 { "" } else { "s" }
                ),
            }),
        })
        .await;
}

pub(in crate::session) async fn continue_activity(session: &Arc<Session>, submission_id: String) {
    let snapshots = session
        .services
        .local_agent_control()
        .continue_activity_for_subtree()
        .await;
    let usage_waiting = session
        .services
        .local_agent_control()
        .request_usage_resume_for_subtree(session.thread_id())
        .await
        > 0;
    let usage_suffix = if usage_waiting {
        " An immediate usage check was requested for retained usage-paused work."
    } else {
        ""
    };
    session
        .send_event_raw_without_materializing_rollout(Event {
            id: submission_id,
            msg: EventMsg::Warning(WarningEvent {
                message: format!(
                    "Resumed activity for {} loaded thread{}; retained work will use its existing scheduler.{usage_suffix}",
                    snapshots.len(),
                    if snapshots.len() == 1 { "" } else { "s" }
                ),
            }),
        })
        .await;
}
