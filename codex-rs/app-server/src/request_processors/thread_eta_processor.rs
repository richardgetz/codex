use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadEtaAccuracy;
use codex_app_server_protocol::ThreadEtaAction;
use codex_app_server_protocol::ThreadEtaOverall;
use codex_app_server_protocol::ThreadEtaReadParams;
use codex_app_server_protocol::ThreadEtaReadResponse;
use codex_app_server_protocol::ThreadEtaRevision;
use codex_app_server_protocol::ThreadEtaSnapshot;
use codex_app_server_protocol::ThreadEtaStatus;
use codex_app_server_protocol::ThreadEtaTask;
use codex_app_server_protocol::ThreadEtaUpdateOperation;
use codex_app_server_protocol::ThreadEtaUpdateParams;
use codex_app_server_protocol::ThreadEtaUpdateResponse;
use codex_app_server_protocol::ThreadEtaUpdatedNotification;
use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadEtaOverallUpdatedEvent;
use codex_protocol::protocol::ThreadEtaTaskUpdatedEvent;
use codex_protocol::protocol::ThreadEtaUpdatedEvent;
use codex_rollout::StateDbHandle;
use codex_state::TaskEstimate;
use codex_state::TaskEstimateAction;
use codex_state::TaskEstimateMutation;
use codex_state::TaskEstimateOverall;
use codex_state::TaskEstimateRange;
use codex_state::TaskEstimateStatus;
use codex_state::TaskEstimateUpdateResult;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ThreadStore;
use chrono::DateTime;
use chrono::Utc;
use std::sync::Arc;

const DEFAULT_HISTORY_LIMIT: usize = 50;
const MAX_HISTORY_LIMIT: usize = 100;
const STALE_AFTER_SECONDS: i64 = 15 * 60;

#[derive(Clone)]
pub(crate) struct ThreadEtaRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    state_db: Option<StateDbHandle>,
    thread_manager: Arc<ThreadManager>,
    thread_store: Arc<dyn ThreadStore>,
}

impl ThreadEtaRequestProcessor {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        state_db: Option<StateDbHandle>,
        thread_manager: Arc<ThreadManager>,
        thread_store: Arc<dyn ThreadStore>,
    ) -> Self {
        Self {
            outgoing,
            state_db,
            thread_manager,
            thread_store,
        }
    }

    pub(crate) async fn read(
        &self,
        params: ThreadEtaReadParams,
    ) -> Result<Option<ClientResponsePayload>, codex_app_server_protocol::JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let state_db = self.state_db()?;
        let root_thread_id = state_db
            .root_thread_id(thread_id)
            .await
            .map_err(|err| internal_error(format!("failed to resolve ETA root: {err}")))?;
        if !self.root_exists(state_db, root_thread_id).await? {
            return Err(invalid_request("ETA root thread was not found"));
        }
        let snapshot = state_db
            .read_task_estimate_snapshot(
                root_thread_id,
                Utc::now(),
                params.cursor.as_deref(),
                Some(
                    params
                        .limit
                        .unwrap_or(DEFAULT_HISTORY_LIMIT as u32)
                        .clamp(1, MAX_HISTORY_LIMIT as u32)
                        as usize,
                ),
            )
            .await
            .map_err(|err| internal_error(format!("failed to read ETA snapshot: {err}")))?;
        Ok(Some(
            ThreadEtaReadResponse {
                snapshot: api_snapshot(snapshot),
            }
            .into(),
        ))
    }

    pub(crate) async fn update(
        &self,
        params: ThreadEtaUpdateParams,
    ) -> Result<Option<ClientResponsePayload>, codex_app_server_protocol::JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let state_db = self.state_db()?;
        let root_thread_id = state_db
            .root_thread_id(thread_id)
            .await
            .map_err(|err| internal_error(format!("failed to resolve ETA root: {err}")))?;
        if thread_id != root_thread_id {
            return Err(invalid_request(
                "ETA updates must target the root session thread",
            ));
        }
        if !self.root_exists(state_db, root_thread_id).await? {
            return Err(invalid_request("ETA root thread was not found"));
        }
        let mutations = params
            .operations
            .iter()
            .map(mutation_from_api)
            .collect::<Result<Vec<_>, _>>()?;
        self.ensure_root_persisted(root_thread_id).await?;
        let result = state_db
            .apply_task_estimate_mutations(root_thread_id, root_thread_id, &mutations, Utc::now())
            .await
            .map_err(|err| invalid_request(format!("invalid ETA update: {err}")))?;
        let response = api_update_response(&result);
        if !response.changed_tasks.is_empty() {
            self.outgoing
                .send_server_notification(ServerNotification::ThreadEtaUpdated(
                    ThreadEtaUpdatedNotification {
                        root_thread_id: response.root_thread_id.clone(),
                        generated_at: response.generated_at,
                        sequence: response.sequence,
                        changed_tasks: response.changed_tasks.clone(),
                        overall: response.overall.clone(),
                    },
                ))
                .await;
        }
        Ok(Some(response.into()))
    }

    pub(crate) fn state_db(
        &self,
    ) -> Result<&StateDbHandle, codex_app_server_protocol::JSONRPCErrorError> {
        self.state_db
            .as_ref()
            .ok_or_else(|| internal_error("sqlite state db unavailable for ETA"))
    }

    async fn ensure_root_persisted(
        &self,
        root_thread_id: ThreadId,
    ) -> Result<(), codex_app_server_protocol::JSONRPCErrorError> {
        let Ok(thread) = self.thread_manager.get_thread(root_thread_id).await else {
            return Ok(());
        };
        if thread.config_snapshot().await.ephemeral {
            return Err(invalid_request("ETA root thread was not found"));
        }

        // Thread starts may stage metadata before the lazy local writer has created its rollout.
        // ETA is durable session state, so force that existing writer through its normal
        // persistence barrier before recording the first task update. This keeps a cold restart
        // able to validate the root without starting a model turn.
        thread.ensure_rollout_materialized().await;
        thread.flush_rollout().await.map_err(|err| {
            internal_error(format!(
                "failed to persist ETA root thread {root_thread_id}: {err}"
            ))
        })
    }

    async fn root_exists(
        &self,
        state_db: &StateDbHandle,
        root_thread_id: ThreadId,
    ) -> Result<bool, codex_app_server_protocol::JSONRPCErrorError> {
        if let Some(metadata) = state_db
            .get_thread(root_thread_id)
            .await
            .map_err(|err| internal_error(format!("failed to validate ETA root: {err}")))?
        {
            if !metadata.rollout_path.as_os_str().is_empty() {
                return Ok(true);
            }

            if let Ok(thread) = self.thread_manager.get_thread(root_thread_id).await {
                let config = thread.config_snapshot().await;
                return Ok(!config.ephemeral && thread.rollout_path().is_some());
            }

            // A thread can be durably staged in SQLite before its rollout is materialized. The
            // thread store is the canonical persistence boundary for this case; probing it keeps
            // valid cold-resume roots readable while still rejecting arbitrary state rows.
            return Ok(self
                .thread_store
                .read_thread(ReadThreadParams {
                    thread_id: root_thread_id,
                    include_archived: false,
                    include_history: false,
                })
                .await
                .is_ok());
        }
        let Ok(thread) = self.thread_manager.get_thread(root_thread_id).await else {
            return Ok(false);
        };
        let config = thread.config_snapshot().await;
        Ok(!config.ephemeral && thread.rollout_path().is_some())
    }
}

pub(crate) fn api_snapshot(snapshot: codex_state::TaskEstimateSnapshot) -> ThreadEtaSnapshot {
    let now = snapshot.generated_at;
    let active = snapshot
        .active
        .iter()
        .map(|task| api_task(task, now))
        .collect::<Vec<_>>();
    let mut overall = api_overall(snapshot.overall);
    if active.iter().any(|task| task.is_stale) {
        overall = ThreadEtaOverall {
            finish_at: None,
            remaining_lower_seconds: None,
            remaining_upper_seconds: None,
            unknown_reason: Some("stale task update".to_string()),
        };
    }
    ThreadEtaSnapshot {
        root_thread_id: snapshot.root_thread_id.to_string(),
        generated_at: now.timestamp(),
        sequence: snapshot.sequence,
        active,
        history: snapshot
            .history
            .iter()
            .map(|task| api_task(task, now))
            .collect(),
        next_cursor: snapshot.next_cursor,
        overall,
    }
}

pub(crate) fn api_task(task: &TaskEstimate, now: DateTime<Utc>) -> ThreadEtaTask {
    let current_range = if task.status.is_terminal() {
        task.current_range()
    } else {
        task.remaining_range(now)
    };
    ThreadEtaTask {
        task_id: task.task_id.clone(),
        root_thread_id: task.root_thread_id.to_string(),
        owner_thread_id: task.owner_thread_id.to_string(),
        parent_task_id: task.parent_task_id.clone(),
        depends_on_task_ids: task.depends_on_task_ids.clone(),
        title: task.title.clone(),
        status: api_status(task.status),
        current_lower_seconds: current_range.lower_seconds,
        current_upper_seconds: current_range.upper_seconds,
        original_lower_seconds: task.original_lower_seconds,
        original_upper_seconds: task.original_upper_seconds,
        created_at: task.created_at.timestamp(),
        started_at: task.started_at.map(|value| value.timestamp()),
        terminal_at: task.terminal_at.map(|value| value.timestamp()),
        actual_elapsed_seconds: task.actual_elapsed_seconds,
        updated_at: task.updated_at.timestamp(),
        is_stale: !task.status.is_terminal()
            && now
                .timestamp()
                .saturating_sub(task.updated_at.timestamp())
                > STALE_AFTER_SECONDS,
        accuracy: accuracy(task),
        revisions: task.revisions.iter().map(api_revision).collect(),
    }
}

fn api_status(status: TaskEstimateStatus) -> ThreadEtaStatus {
    match status {
        TaskEstimateStatus::Pending => ThreadEtaStatus::Pending,
        TaskEstimateStatus::Active => ThreadEtaStatus::Active,
        TaskEstimateStatus::Blocked => ThreadEtaStatus::Blocked,
        TaskEstimateStatus::Completed => ThreadEtaStatus::Completed,
        TaskEstimateStatus::Cancelled => ThreadEtaStatus::Cancelled,
    }
}

fn accuracy(task: &TaskEstimate) -> ThreadEtaAccuracy {
    if task.status == TaskEstimateStatus::Cancelled {
        return ThreadEtaAccuracy::Unknown;
    }
    let (Some(actual), Some(lower), Some(upper)) = (
        task.actual_elapsed_seconds,
        task.original_lower_seconds,
        task.original_upper_seconds,
    ) else {
        return ThreadEtaAccuracy::Unknown;
    };
    if actual < lower {
        ThreadEtaAccuracy::Early
    } else if actual > upper {
        ThreadEtaAccuracy::Late
    } else {
        ThreadEtaAccuracy::Within
    }
}

fn api_revision(revision: &codex_state::TaskEstimateRevision) -> ThreadEtaRevision {
    ThreadEtaRevision {
        lower_seconds: revision.lower_seconds,
        upper_seconds: revision.upper_seconds,
        reason: revision.reason.clone(),
        updated_at: revision.updated_at.timestamp(),
        actor_thread_id: revision.actor_thread_id.to_string(),
    }
}

fn api_overall(overall: TaskEstimateOverall) -> ThreadEtaOverall {
    ThreadEtaOverall {
        finish_at: overall.finish_at.map(|value| value.timestamp()),
        remaining_lower_seconds: overall.remaining_lower_seconds,
        remaining_upper_seconds: overall.remaining_upper_seconds,
        unknown_reason: overall.unknown_reason,
    }
}

fn api_update_response(result: &TaskEstimateUpdateResult) -> ThreadEtaUpdateResponse {
    ThreadEtaUpdateResponse {
        root_thread_id: result.root_thread_id.to_string(),
        generated_at: result.generated_at.timestamp(),
        sequence: result.sequence,
        changed_tasks: result
            .changed_tasks
            .iter()
            .map(|task| api_task(task, result.generated_at))
            .collect(),
        overall: api_overall(result.overall.clone()),
    }
}

fn mutation_from_api(
    operation: &ThreadEtaUpdateOperation,
) -> Result<TaskEstimateMutation, codex_app_server_protocol::JSONRPCErrorError> {
    let estimate = match (
        operation.estimate_lower_seconds,
        operation.estimate_upper_seconds,
    ) {
        (None, None) => None,
        (lower_seconds, upper_seconds) => Some(TaskEstimateRange {
            lower_seconds,
            upper_seconds,
        }),
    };
    Ok(TaskEstimateMutation {
        action: match operation.action {
            ThreadEtaAction::Create => TaskEstimateAction::Create,
            ThreadEtaAction::Start => TaskEstimateAction::Start,
            ThreadEtaAction::Revise => TaskEstimateAction::Revise,
            ThreadEtaAction::Block => TaskEstimateAction::Block,
            ThreadEtaAction::Complete => TaskEstimateAction::Complete,
            ThreadEtaAction::Cancel => TaskEstimateAction::Cancel,
        },
        task_id: operation.task_id.clone(),
        title: operation.title.clone(),
        parent_task_id: operation.parent_task_id.clone(),
        depends_on_task_ids: operation.depends_on_task_ids.clone(),
        estimate,
        reason: operation.reason.clone(),
    })
}

pub(crate) fn api_notification_from_event(
    event: ThreadEtaUpdatedEvent,
) -> ThreadEtaUpdatedNotification {
    ThreadEtaUpdatedNotification {
        root_thread_id: event.root_thread_id.to_string(),
        generated_at: event.generated_at,
        sequence: event.sequence,
        changed_tasks: event
            .changed_tasks
            .into_iter()
            .map(api_task_from_event)
            .collect(),
        overall: api_overall_from_event(event.overall),
    }
}

fn api_task_from_event(task: ThreadEtaTaskUpdatedEvent) -> ThreadEtaTask {
    let status = match task.status.as_str() {
        "pending" => ThreadEtaStatus::Pending,
        "active" => ThreadEtaStatus::Active,
        "blocked" => ThreadEtaStatus::Blocked,
        "completed" => ThreadEtaStatus::Completed,
        "cancelled" => ThreadEtaStatus::Cancelled,
        _ => ThreadEtaStatus::Unknown,
    };
    let accuracy = match (status, task.actual_elapsed_seconds) {
        (ThreadEtaStatus::Cancelled, _) | (_, None) => ThreadEtaAccuracy::Unknown,
        (_, Some(actual)) => match (
            task.original_lower_seconds,
            task.original_upper_seconds,
        ) {
            (Some(lower), Some(upper)) if actual < lower => ThreadEtaAccuracy::Early,
            (Some(_), Some(upper)) if actual > upper => ThreadEtaAccuracy::Late,
            (Some(_), Some(_)) => ThreadEtaAccuracy::Within,
            _ => ThreadEtaAccuracy::Unknown,
        },
    };
    ThreadEtaTask {
        task_id: task.task_id,
        root_thread_id: task.root_thread_id.to_string(),
        owner_thread_id: task.owner_thread_id.to_string(),
        parent_task_id: task.parent_task_id,
        depends_on_task_ids: task.depends_on_task_ids,
        title: task.title,
        status,
        current_lower_seconds: task.current_lower_seconds,
        current_upper_seconds: task.current_upper_seconds,
        original_lower_seconds: task.original_lower_seconds,
        original_upper_seconds: task.original_upper_seconds,
        created_at: task.created_at,
        started_at: task.started_at,
        terminal_at: task.terminal_at,
        actual_elapsed_seconds: task.actual_elapsed_seconds,
        updated_at: task.updated_at,
        is_stale: false,
        accuracy,
        revisions: task
            .revisions
            .into_iter()
            .map(|revision| ThreadEtaRevision {
                lower_seconds: revision.lower_seconds,
                upper_seconds: revision.upper_seconds,
                reason: revision.reason,
                updated_at: revision.updated_at,
                actor_thread_id: revision.actor_thread_id.to_string(),
            })
            .collect(),
    }
}

fn api_overall_from_event(overall: ThreadEtaOverallUpdatedEvent) -> ThreadEtaOverall {
    ThreadEtaOverall {
        finish_at: overall.finish_at,
        remaining_lower_seconds: overall.remaining_lower_seconds,
        remaining_upper_seconds: overall.remaining_upper_seconds,
        unknown_reason: overall.unknown_reason,
    }
}

fn parse_thread_id(
    value: &str,
) -> Result<ThreadId, codex_app_server_protocol::JSONRPCErrorError> {
    ThreadId::from_string(value).map_err(|err| invalid_request(format!("invalid thread id: {err}")))
}

#[cfg(test)]
#[path = "thread_eta_processor_tests.rs"]
mod tests;
