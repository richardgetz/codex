use crate::config_manager::ConfigManager;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::OutgoingMessageSender;
use chrono::DateTime;
use chrono::Utc;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadEtaAccuracy;
use codex_app_server_protocol::ThreadEtaAction;
use codex_app_server_protocol::ThreadEtaListParams;
use codex_app_server_protocol::ThreadEtaListResponse;
use codex_app_server_protocol::ThreadEtaOverall;
use codex_app_server_protocol::ThreadEtaReadParams;
use codex_app_server_protocol::ThreadEtaReadResponse;
use codex_app_server_protocol::ThreadEtaRevision;
use codex_app_server_protocol::ThreadEtaSessionInfo;
use codex_app_server_protocol::ThreadEtaSessionTask;
use codex_app_server_protocol::ThreadEtaSnapshot;
use codex_app_server_protocol::ThreadEtaStatus;
use codex_app_server_protocol::ThreadEtaTask;
use codex_app_server_protocol::ThreadEtaUpdateOperation;
use codex_app_server_protocol::ThreadEtaUpdateParams;
use codex_app_server_protocol::ThreadEtaUpdateResponse;
use codex_app_server_protocol::ThreadEtaUpdatedNotification;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_core::config::DEFAULT_ETA_HISTORY_RETENTION_DAYS;
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
use codex_state::TaskEstimateSessionRow;
use codex_state::TaskEstimateStatus;
use codex_state::TaskEstimateUpdateResult;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ThreadStore;
use codex_utils_path_uri::LegacyAppPathString;
use std::sync::Arc;
use tracing::warn;

const DEFAULT_HISTORY_LIMIT: usize = 50;
const MAX_HISTORY_LIMIT: usize = 100;
const DEFAULT_FRESHNESS_MINIMUM_SECONDS: i64 = 15 * 60;

#[derive(Clone)]
pub(crate) struct ThreadEtaRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    state_db: Option<StateDbHandle>,
    config_manager: ConfigManager,
    thread_manager: Arc<ThreadManager>,
    thread_store: Arc<dyn ThreadStore>,
}

impl ThreadEtaRequestProcessor {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        state_db: Option<StateDbHandle>,
        config_manager: ConfigManager,
        thread_manager: Arc<ThreadManager>,
        thread_store: Arc<dyn ThreadStore>,
    ) -> Self {
        Self {
            outgoing,
            state_db,
            config_manager,
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
        let root_thread = self.thread_manager.get_thread(root_thread_id).await.ok();
        let (snapshot, freshness_minimum_seconds) = if let Some(thread) = root_thread.as_ref() {
            let _eta_dispatch = thread.lock_eta_reminders().await;
            let freshness_minimum_seconds = self
                .freshness_minimum_seconds(state_db, root_thread_id, Some(thread))
                .await;
            let snapshot = state_db
                .read_task_estimate_snapshot_with_freshness_minimum(
                    root_thread_id,
                    Utc::now(),
                    params.cursor.as_deref(),
                    Some(
                        params
                            .limit
                            .unwrap_or(DEFAULT_HISTORY_LIMIT as u32)
                            .clamp(1, MAX_HISTORY_LIMIT as u32) as usize,
                    ),
                    freshness_minimum_seconds,
                )
                .await
                .map_err(|err| internal_error(format!("failed to read ETA snapshot: {err}")))?;
            (snapshot, freshness_minimum_seconds)
        } else {
            let freshness_minimum_seconds = self
                .freshness_minimum_seconds(state_db, root_thread_id, None)
                .await;
            let snapshot = state_db
                .read_task_estimate_snapshot_with_freshness_minimum(
                    root_thread_id,
                    Utc::now(),
                    params.cursor.as_deref(),
                    Some(
                        params
                            .limit
                            .unwrap_or(DEFAULT_HISTORY_LIMIT as u32)
                            .clamp(1, MAX_HISTORY_LIMIT as u32) as usize,
                    ),
                    freshness_minimum_seconds,
                )
                .await
                .map_err(|err| internal_error(format!("failed to read ETA snapshot: {err}")))?;
            (snapshot, freshness_minimum_seconds)
        };
        Ok(Some(
            ThreadEtaReadResponse {
                snapshot: api_snapshot_with_freshness_minimum(snapshot, freshness_minimum_seconds),
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
        let root_thread = self.thread_manager.get_thread(root_thread_id).await.ok();
        let (result, freshness_minimum_seconds) = if let Some(thread) = root_thread.as_ref() {
            let eta_dispatch = thread.lock_eta_reminders().await;
            let freshness_minimum_seconds = self
                .freshness_minimum_seconds(state_db, root_thread_id, Some(thread))
                .await;
            let result = state_db
                .apply_task_estimate_mutations_with_freshness_minimum(
                    root_thread_id,
                    root_thread_id,
                    &mutations,
                    Utc::now(),
                    freshness_minimum_seconds,
                )
                .await
                .map_err(|err| invalid_request(format!("invalid ETA update: {err}")))?;
            if !result.changed_tasks.is_empty() {
                thread
                    .schedule_eta_reminders_locked(
                        result.root_thread_id,
                        &result.changed_tasks,
                        &eta_dispatch,
                    )
                    .await;
            }
            (result, freshness_minimum_seconds)
        } else {
            let freshness_minimum_seconds = self
                .freshness_minimum_seconds(state_db, root_thread_id, None)
                .await;
            state_db
                .apply_task_estimate_mutations_with_freshness_minimum(
                    root_thread_id,
                    root_thread_id,
                    &mutations,
                    Utc::now(),
                    freshness_minimum_seconds,
                )
                .await
                .map(|result| (result, freshness_minimum_seconds))
                .map_err(|err| invalid_request(format!("invalid ETA update: {err}")))?
        };
        let response =
            api_update_response_with_freshness_minimum(&result, freshness_minimum_seconds);
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

    pub(crate) async fn list(
        &self,
        params: ThreadEtaListParams,
    ) -> Result<Option<ClientResponsePayload>, codex_app_server_protocol::JSONRPCErrorError> {
        let state_db = self.state_db()?;
        self.prune_history(state_db).await?;
        let configured_freshness_minimum_seconds =
            self.configured_freshness_minimum_seconds().await;
        state_db
            .initialize_missing_eta_freshness_minimum_seconds(configured_freshness_minimum_seconds)
            .await
            .map_err(|err| internal_error(format!("failed to initialize ETA freshness: {err}")))?;
        let generated_at = Utc::now();
        let page = state_db
            .list_task_estimate_sessions_at(
                params.cursor.as_deref(),
                Some(
                    params
                        .limit
                        .unwrap_or(DEFAULT_HISTORY_LIMIT as u32)
                        .clamp(1, MAX_HISTORY_LIMIT as u32) as usize,
                ),
                params.include_nested,
                generated_at,
            )
            .await
            .map_err(|err| internal_error(format!("failed to read all-session ETA: {err}")))?;
        Ok(Some(
            ThreadEtaListResponse {
                data: page.rows.into_iter().map(api_session_task).collect(),
                next_cursor: page.next_cursor,
            }
            .into(),
        ))
    }

    async fn configured_freshness_minimum_seconds(&self) -> i64 {
        match self
            .config_manager
            .load_latest_config(/*fallback_cwd*/ None)
            .await
        {
            Ok(config) => config
                .eta
                .freshness_minimum_minutes
                .saturating_mul(60)
                .min(i64::MAX as u64) as i64,
            Err(error) => {
                warn!(%error, "failed to load ETA freshness policy; using default");
                DEFAULT_FRESHNESS_MINIMUM_SECONDS
            }
        }
    }

    async fn prune_history(
        &self,
        state_db: &StateDbHandle,
    ) -> Result<(), codex_app_server_protocol::JSONRPCErrorError> {
        let retention_days = match self
            .config_manager
            .load_latest_config(/*fallback_cwd*/ None)
            .await
        {
            Ok(config) => config.eta.history_retention_days,
            Err(error) => {
                warn!(%error, "failed to load ETA history retention policy; using default");
                DEFAULT_HISTORY_RETENTION_DAYS
            }
        };
        state_db
            .prune_task_estimate_history(retention_days, Utc::now())
            .await
            .map(|_| ())
            .map_err(|err| internal_error(format!("failed to prune ETA history: {err}")))
    }

    pub(crate) fn state_db(
        &self,
    ) -> Result<&StateDbHandle, codex_app_server_protocol::JSONRPCErrorError> {
        self.state_db
            .as_ref()
            .ok_or_else(|| internal_error("sqlite state db unavailable for ETA"))
    }

    async fn freshness_minimum_seconds(
        &self,
        state_db: &StateDbHandle,
        root_thread_id: ThreadId,
        root_thread: Option<&CodexThread>,
    ) -> i64 {
        if let Ok(Some(seconds)) = state_db.eta_freshness_minimum_seconds(root_thread_id).await {
            return seconds.clamp(0, i64::MAX);
        }
        if let Some(thread) = root_thread {
            let freshness_minimum_seconds = thread
                .eta_freshness_minimum_seconds()
                .await
                .min(i64::MAX as u64) as i64;
            if let Err(error) = state_db
                .set_eta_freshness_minimum_seconds(root_thread_id, freshness_minimum_seconds)
                .await
            {
                warn!(%error, %root_thread_id, "failed to persist ETA freshness policy");
            }
            return freshness_minimum_seconds;
        }
        let configured_freshness_minimum_seconds =
            self.configured_freshness_minimum_seconds().await;
        match state_db
            .initialize_eta_freshness_minimum_seconds(
                root_thread_id,
                configured_freshness_minimum_seconds.min(i64::MAX as u64) as i64,
            )
            .await
        {
            Ok(persisted) => persisted.clamp(0, i64::MAX),
            Err(error) => {
                warn!(%error, %root_thread_id, "failed to persist ETA freshness policy");
                configured_freshness_minimum_seconds.min(i64::MAX as u64) as i64
            }
        }
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
    api_snapshot_with_freshness_minimum(snapshot, DEFAULT_FRESHNESS_MINIMUM_SECONDS)
}

fn api_snapshot_with_freshness_minimum(
    snapshot: codex_state::TaskEstimateSnapshot,
    freshness_minimum_seconds: i64,
) -> ThreadEtaSnapshot {
    let now = snapshot.generated_at;
    let active = snapshot
        .active
        .iter()
        .map(|task| api_task_with_freshness_minimum(task, now, freshness_minimum_seconds))
        .collect::<Vec<_>>();
    let mut overall = api_overall(snapshot.overall);
    // Grouping rows inherit the freshness of their executable children. A stale parent with a
    // fresh active child must not hide a valid aggregate or produce a duplicate warning.
    if active.iter().any(|task| {
        task.is_stale
            && !active.iter().any(|child| {
                child.parent_task_id.as_deref() == Some(task.task_id.as_str())
                    && !matches!(
                        child.status,
                        ThreadEtaStatus::Completed | ThreadEtaStatus::Cancelled
                    )
            })
    }) {
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
            .map(|task| api_task_with_freshness_minimum(task, now, freshness_minimum_seconds))
            .collect(),
        next_cursor: snapshot.next_cursor,
        overall,
    }
}

pub(crate) fn api_task(task: &TaskEstimate, now: DateTime<Utc>) -> ThreadEtaTask {
    api_task_with_freshness_minimum(task, now, DEFAULT_FRESHNESS_MINIMUM_SECONDS)
}

fn api_task_with_freshness_minimum(
    task: &TaskEstimate,
    now: DateTime<Utc>,
    freshness_minimum_seconds: i64,
) -> ThreadEtaTask {
    let is_stale = !task.status.is_terminal()
        && task.started_at.is_some()
        && now.timestamp().saturating_sub(task.updated_at.timestamp())
            >= task.freshness_delay_seconds(freshness_minimum_seconds);
    let current_range = if task.status.is_terminal() || is_stale {
        // Preserve the saved estimate once freshness expires. Consumers can show the warning
        // marker and explain that this range needs owner reassessment instead of losing it to a
        // synthetic "stale" value.
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
        is_stale,
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

fn api_session_task(task: TaskEstimateSessionRow) -> ThreadEtaSessionTask {
    ThreadEtaSessionTask {
        task_id: task.task_id,
        root_thread_id: task.root_thread_id.to_string(),
        owner_thread_id: task.owner_thread_id.to_string(),
        parent_task_id: task.parent_task_id,
        title: task.title,
        status: api_status(task.status),
        current_lower_seconds: task.current_lower_seconds,
        current_upper_seconds: task.current_upper_seconds,
        original_lower_seconds: task.original_lower_seconds,
        original_upper_seconds: task.original_upper_seconds,
        created_at: task.created_at.timestamp(),
        started_at: task.started_at.map(|value| value.timestamp()),
        terminal_at: task.terminal_at.map(|value| value.timestamp()),
        actual_elapsed_seconds: task.actual_elapsed_seconds,
        updated_at: task.updated_at.timestamp(),
        is_stale: task.is_stale,
        session: ThreadEtaSessionInfo {
            thread_id: task.root_thread_id.to_string(),
            title: task.session_title,
            name: task.session_name,
            preview: task.session_preview,
            created_at: task.session_created_at.timestamp(),
            updated_at: task.session_updated_at.timestamp(),
            archived_at: task.session_archived_at.map(|value| value.timestamp()),
            cwd: LegacyAppPathString::from_path(&task.session_cwd),
        },
        nested_task_count: task.nested_task_count.try_into().unwrap_or(u32::MAX),
        active_nested_task_count: task.active_nested_task_count.try_into().unwrap_or(u32::MAX),
        nested_lower_seconds: task.nested_lower_seconds,
        nested_upper_seconds: task.nested_upper_seconds,
    }
}

fn api_update_response(result: &TaskEstimateUpdateResult) -> ThreadEtaUpdateResponse {
    api_update_response_with_freshness_minimum(result, DEFAULT_FRESHNESS_MINIMUM_SECONDS)
}

fn api_update_response_with_freshness_minimum(
    result: &TaskEstimateUpdateResult,
    freshness_minimum_seconds: i64,
) -> ThreadEtaUpdateResponse {
    ThreadEtaUpdateResponse {
        root_thread_id: result.root_thread_id.to_string(),
        generated_at: result.generated_at.timestamp(),
        sequence: result.sequence,
        changed_tasks: result
            .changed_tasks
            .iter()
            .map(|task| {
                api_task_with_freshness_minimum(
                    task,
                    result.generated_at,
                    freshness_minimum_seconds,
                )
            })
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
    let owner_thread_id = operation
        .owner_thread_id
        .as_deref()
        .map(parse_thread_id)
        .transpose()?;
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
        owner_thread_id,
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
        (_, Some(actual)) => match (task.original_lower_seconds, task.original_upper_seconds) {
            (Some(lower), Some(_upper)) if actual < lower => ThreadEtaAccuracy::Early,
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
        is_stale: task.is_stale,
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

fn parse_thread_id(value: &str) -> Result<ThreadId, codex_app_server_protocol::JSONRPCErrorError> {
    ThreadId::from_string(value).map_err(|err| invalid_request(format!("invalid thread id: {err}")))
}

#[cfg(test)]
#[path = "thread_eta_processor_tests.rs"]
mod tests;
