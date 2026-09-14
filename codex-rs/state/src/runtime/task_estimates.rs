use super::StateRuntime;
use super::task_estimate_storage;
use crate::TaskEstimate;
use crate::TaskEstimateAction;
use crate::TaskEstimateHistoryPage;
use crate::TaskEstimateMutation;
#[cfg(test)]
use crate::TaskEstimateOverall;
use crate::TaskEstimateSnapshot;
use crate::TaskEstimateStatus;
use crate::TaskEstimateUpdateResult;
use crate::model::datetime_to_epoch_seconds;
use crate::model::validate_dependencies;
use crate::model::validate_dependency_graph;
use crate::model::validate_task_id;
use crate::model::validate_task_reason;
use crate::model::validate_task_title;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;

#[path = "task_estimate_math.rs"]
mod task_estimate_math;
use self::task_estimate_math::DEFAULT_FRESHNESS_MINIMUM_SECONDS;
use self::task_estimate_math::compute_overall_with_freshness_minimum;

pub(super) const MAX_TASKS_PER_ROOT: usize = 256;
pub(super) const MAX_REVISIONS_PER_TASK: usize = 32;
const DEFAULT_HISTORY_LIMIT: usize = 50;
const MAX_HISTORY_LIMIT: usize = 100;

#[derive(Clone)]
pub struct TaskEstimateStore {
    pool: Arc<SqlitePool>,
}

impl TaskEstimateStore {
    pub(crate) fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    /// Remove a deleted root's ledger and preserve a deleted worker's history while making its
    /// unfinished tasks explicitly blocked for the remaining root session.
    pub(super) async fn handle_deleted_threads(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        thread_ids: &[ThreadId],
    ) -> anyhow::Result<()> {
        let now = datetime_to_epoch_seconds(Utc::now());
        for thread_id in thread_ids {
            let thread_id = thread_id.to_string();
            let is_root = sqlx::query_scalar::<_, i64>(
                "SELECT EXISTS(SELECT 1 FROM eta_roots WHERE root_thread_id = ?) OR EXISTS(SELECT 1 FROM eta_tasks WHERE root_thread_id = ?)",
            )
            .bind(&thread_id)
            .bind(&thread_id)
            .fetch_one(&mut **tx)
            .await?
                != 0;
            if is_root {
                sqlx::query("DELETE FROM eta_task_revisions WHERE root_thread_id = ?")
                    .bind(&thread_id)
                    .execute(&mut **tx)
                    .await?;
                sqlx::query("DELETE FROM eta_tasks WHERE root_thread_id = ?")
                    .bind(&thread_id)
                    .execute(&mut **tx)
                    .await?;
                sqlx::query("DELETE FROM eta_roots WHERE root_thread_id = ?")
                    .bind(&thread_id)
                    .execute(&mut **tx)
                    .await?;
                continue;
            }

            let roots = sqlx::query_scalar::<_, String>(
                "SELECT DISTINCT root_thread_id FROM eta_tasks WHERE owner_thread_id = ? AND status IN ('pending', 'active')",
            )
            .bind(&thread_id)
            .fetch_all(&mut **tx)
            .await?;
            for root_thread_id in roots {
                sqlx::query(
                    "UPDATE eta_roots SET sequence = sequence + 1 WHERE root_thread_id = ?",
                )
                .bind(&root_thread_id)
                .execute(&mut **tx)
                .await?;
                let sequence = sqlx::query_scalar::<_, i64>(
                    "SELECT sequence FROM eta_roots WHERE root_thread_id = ?",
                )
                .bind(&root_thread_id)
                .fetch_one(&mut **tx)
                .await?;
                sqlx::query(
                    "UPDATE eta_tasks SET status = 'blocked', updated_at = ?, updated_sequence = ? WHERE root_thread_id = ? AND owner_thread_id = ? AND status IN ('pending', 'active')",
                )
                .bind(now)
                .bind(sequence)
                .bind(&root_thread_id)
                .bind(&thread_id)
                .execute(&mut **tx)
                .await?;
            }
        }
        Ok(())
    }

    /// Read active tasks and a bounded terminal history page without changing any state.
    pub async fn read_snapshot(
        &self,
        root_thread_id: ThreadId,
        now: DateTime<Utc>,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<TaskEstimateSnapshot> {
        self.read_snapshot_with_freshness_minimum(
            root_thread_id,
            now,
            cursor,
            limit,
            DEFAULT_FRESHNESS_MINIMUM_SECONDS,
        )
        .await
    }

    /// Read a snapshot using a caller-selected freshness minimum for aggregate stale detection.
    pub async fn read_snapshot_with_freshness_minimum(
        &self,
        root_thread_id: ThreadId,
        now: DateTime<Utc>,
        cursor: Option<&str>,
        limit: Option<usize>,
        freshness_minimum_seconds: i64,
    ) -> anyhow::Result<TaskEstimateSnapshot> {
        let limit = limit
            .unwrap_or(DEFAULT_HISTORY_LIMIT)
            .clamp(1, MAX_HISTORY_LIMIT);
        // Keep the sequence, task rows, revisions, and aggregate on one SQLite snapshot. A
        // sequence of pool reads can otherwise observe an update half-way through the response.
        let mut tx = self.pool.begin().await?;
        let sequence =
            sqlx::query_scalar::<_, i64>("SELECT sequence FROM eta_roots WHERE root_thread_id = ?")
                .bind(root_thread_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or_default();

        let active_rows = sqlx::query(
            r#"
SELECT task_id, root_thread_id, owner_thread_id, parent_task_id, depends_on_task_ids, title,
       status, current_lower_seconds, current_upper_seconds, original_lower_seconds,
       original_upper_seconds, created_at, started_at, terminal_at, actual_elapsed_seconds,
       updated_at, updated_sequence
FROM eta_tasks
WHERE root_thread_id = ? AND status IN ('pending', 'active', 'blocked')
ORDER BY created_at, task_id
            "#,
        )
        .bind(root_thread_id.to_string())
        .fetch_all(&mut *tx)
        .await?;
        let mut active = Vec::with_capacity(active_rows.len());
        for row in active_rows {
            active.push(task_estimate_storage::task_from_row_with_revisions(&mut tx, row).await?);
        }

        let (cursor_terminal_at, cursor_task_id) = cursor
            .map(task_estimate_storage::parse_history_cursor)
            .transpose()?
            .unwrap_or((i64::MIN, String::new()));
        let history_rows = sqlx::query(
            r#"
SELECT task_id, root_thread_id, owner_thread_id, parent_task_id, depends_on_task_ids, title,
       status, current_lower_seconds, current_upper_seconds, original_lower_seconds,
       original_upper_seconds, created_at, started_at, terminal_at, actual_elapsed_seconds,
       updated_at, updated_sequence
FROM eta_tasks
WHERE root_thread_id = ?
  AND status IN ('completed', 'cancelled')
  AND (terminal_at > ? OR (terminal_at = ? AND task_id > ?))
ORDER BY terminal_at, task_id
LIMIT ?
            "#,
        )
        .bind(root_thread_id.to_string())
        .bind(cursor_terminal_at)
        .bind(cursor_terminal_at)
        .bind(cursor_task_id)
        .bind(i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX))
        .fetch_all(&mut *tx)
        .await?;
        let mut history = Vec::with_capacity(history_rows.len().min(limit));
        for row in history_rows {
            history.push(task_estimate_storage::task_from_row_with_revisions(&mut tx, row).await?);
        }
        let next_cursor = if history.len() > limit {
            history
                .pop()
                .map(|task| task_estimate_storage::history_cursor(&task))
        } else {
            None
        };

        // Keep aggregate dependency resolution independent from the paginated History output.
        // Read every unfinished task plus only the terminal rows explicitly referenced by their
        // parent/dependency graph; lifetime History is intentionally not scanned here.
        let all_tasks = task_estimate_storage::load_relevant_task_map(
            &mut tx,
            root_thread_id,
            &BTreeSet::new(),
        )
        .await?;

        tx.commit().await?;

        Ok(TaskEstimateSnapshot {
            root_thread_id,
            generated_at: now,
            sequence,
            overall: compute_overall_with_freshness_minimum(
                &all_tasks.values().cloned().collect::<Vec<_>>(),
                now,
                freshness_minimum_seconds,
            ),
            active,
            history,
            next_cursor,
        })
    }

    /// Read only terminal tasks for callers that need a separate History page.
    pub async fn read_history_page(
        &self,
        root_thread_id: ThreadId,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<TaskEstimateHistoryPage> {
        let snapshot = self
            .read_snapshot(root_thread_id, Utc::now(), cursor, limit)
            .await?;
        Ok(TaskEstimateHistoryPage {
            tasks: snapshot.history,
            next_cursor: snapshot.next_cursor,
        })
    }

    /// Apply a bounded atomic batch of model/app-server updates.
    ///
    /// The root and actor identities are supplied by trusted runtime callers. An actor other than
    /// the root can mutate only tasks it owns and only when its thread is a persisted descendant
    /// of the root. Timestamps come from the harness and are never accepted from a mutation.
    pub async fn apply_mutations(
        &self,
        root_thread_id: ThreadId,
        actor_thread_id: ThreadId,
        mutations: &[TaskEstimateMutation],
        now: DateTime<Utc>,
    ) -> anyhow::Result<TaskEstimateUpdateResult> {
        self.apply_mutations_with_freshness_minimum(
            root_thread_id,
            actor_thread_id,
            mutations,
            now,
            DEFAULT_FRESHNESS_MINIMUM_SECONDS,
        )
        .await
    }

    /// Apply mutations while using a caller-selected freshness minimum for aggregate stale detection.
    pub async fn apply_mutations_with_freshness_minimum(
        &self,
        root_thread_id: ThreadId,
        actor_thread_id: ThreadId,
        mutations: &[TaskEstimateMutation],
        now: DateTime<Utc>,
        freshness_minimum_seconds: i64,
    ) -> anyhow::Result<TaskEstimateUpdateResult> {
        if mutations.is_empty() {
            let snapshot = self
                .read_snapshot_with_freshness_minimum(
                    root_thread_id,
                    now,
                    None,
                    Some(DEFAULT_HISTORY_LIMIT),
                    freshness_minimum_seconds,
                )
                .await?;
            return Ok(TaskEstimateUpdateResult {
                root_thread_id,
                generated_at: now,
                sequence: snapshot.sequence,
                changed_tasks: Vec::new(),
                overall: snapshot.overall,
            });
        }
        if mutations.len() > MAX_TASKS_PER_ROOT {
            return Err(anyhow::anyhow!(
                "an ETA update may contain at most {MAX_TASKS_PER_ROOT} operations"
            ));
        }

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO eta_roots (root_thread_id, sequence) VALUES (?, 0) ON CONFLICT(root_thread_id) DO NOTHING",
        )
        .bind(root_thread_id.to_string())
        .execute(&mut *tx)
        .await?;
        if actor_thread_id != root_thread_id
            && !task_estimate_storage::actor_is_descendant(&mut tx, root_thread_id, actor_thread_id)
                .await?
        {
            return Err(anyhow::anyhow!(
                "task estimate actor is not part of the requested root session"
            ));
        }
        if mutations
            .iter()
            .any(|mutation| mutation.owner_thread_id.is_some())
            && actor_thread_id != root_thread_id
        {
            return Err(anyhow::anyhow!(
                "only the root Lead may assign an ETA task owner"
            ));
        }
        let previous_sequence =
            sqlx::query_scalar::<_, i64>("SELECT sequence FROM eta_roots WHERE root_thread_id = ?")
                .bind(root_thread_id.to_string())
                .fetch_one(&mut *tx)
                .await?;
        let sequence = previous_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("ETA update sequence overflow"))?;

        let mut changed_ids = BTreeSet::new();
        for mutation in mutations {
            let changed = self
                .apply_mutation(
                    &mut tx,
                    root_thread_id,
                    actor_thread_id,
                    sequence,
                    mutation,
                    now,
                )
                .await?;
            if let Some(task_id) = changed {
                changed_ids.insert(task_id);
            }
            let task_map = task_estimate_storage::load_relevant_task_map(
                &mut tx,
                root_thread_id,
                &changed_ids,
            )
            .await?;
            validate_dependency_graph(&task_map)?;
            task_estimate_storage::validate_parent_graph(&task_map)?;
        }
        let all_tasks =
            task_estimate_storage::load_relevant_task_map(&mut tx, root_thread_id, &changed_ids)
                .await?;
        if changed_ids.is_empty() {
            let overall = compute_overall_with_freshness_minimum(
                &all_tasks.values().cloned().collect::<Vec<_>>(),
                now,
                freshness_minimum_seconds,
            );
            tx.commit().await?;
            return Ok(TaskEstimateUpdateResult {
                root_thread_id,
                generated_at: now,
                sequence: previous_sequence,
                changed_tasks: Vec::new(),
                overall,
            });
        }
        sqlx::query("UPDATE eta_roots SET sequence = ? WHERE root_thread_id = ?")
            .bind(sequence)
            .bind(root_thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        let mut changed_tasks = Vec::new();
        for task_id in changed_ids {
            let Some(task) = all_tasks.get(&task_id).cloned() else {
                continue;
            };
            let revisions =
                task_estimate_storage::load_revisions(&mut tx, root_thread_id, &task_id).await?;
            changed_tasks.push(TaskEstimate { revisions, ..task });
        }
        let overall = compute_overall_with_freshness_minimum(
            &all_tasks.values().cloned().collect::<Vec<_>>(),
            now,
            freshness_minimum_seconds,
        );
        tx.commit().await?;

        Ok(TaskEstimateUpdateResult {
            root_thread_id,
            generated_at: now,
            sequence,
            changed_tasks,
            overall,
        })
    }

    async fn apply_mutation(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        root_thread_id: ThreadId,
        actor_thread_id: ThreadId,
        sequence: i64,
        mutation: &TaskEstimateMutation,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<String>> {
        validate_task_reason(mutation.reason.as_deref())?;
        if let Some(estimate) = mutation.estimate {
            estimate.validate()?;
        }
        if let Some(owner_thread_id) = mutation.owner_thread_id
            && !task_estimate_storage::thread_is_in_root(tx, root_thread_id, owner_thread_id)
                .await?
        {
            return Err(anyhow::anyhow!(
                "ETA task owner is not part of the requested root session"
            ));
        }
        match mutation.action {
            TaskEstimateAction::Create => {
                let title = mutation
                    .title
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("creating a task requires a title"))?;
                validate_task_title(title)?;
                let task_id = mutation
                    .task_id
                    .clone()
                    .unwrap_or_else(|| Uuid::now_v7().to_string());
                validate_task_id(&task_id)?;
                let existing_count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM eta_tasks WHERE root_thread_id = ? AND status IN ('pending', 'active', 'blocked')",
                )
                .bind(root_thread_id.to_string())
                .fetch_one(&mut **tx)
                .await?;
                if usize::try_from(existing_count).unwrap_or(MAX_TASKS_PER_ROOT)
                    >= MAX_TASKS_PER_ROOT
                {
                    return Err(anyhow::anyhow!(
                        "a root session may contain at most {MAX_TASKS_PER_ROOT} ETA tasks"
                    ));
                }
                let parent_task_id = mutation.parent_task_id.as_deref();
                let depends_on_task_ids = mutation.depends_on_task_ids.clone().unwrap_or_default();
                validate_dependencies(&task_id, parent_task_id, &depends_on_task_ids)?;
                task_estimate_storage::ensure_related_tasks_exist(
                    tx,
                    root_thread_id,
                    parent_task_id,
                    &depends_on_task_ids,
                )
                .await?;
                let estimate = mutation.estimate.unwrap_or(crate::TaskEstimateRange {
                    lower_seconds: None,
                    upper_seconds: None,
                });
                sqlx::query(
                    r#"
INSERT INTO eta_tasks (
    task_id, root_thread_id, owner_thread_id, parent_task_id, depends_on_task_ids, title, status,
    current_lower_seconds, current_upper_seconds, original_lower_seconds, original_upper_seconds,
    created_at, started_at, terminal_at, actual_elapsed_seconds, updated_at, updated_sequence
) VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?, NULL, NULL, ?, NULL, NULL, NULL, ?, ?)
                    "#,
                )
                .bind(&task_id)
                .bind(root_thread_id.to_string())
                .bind(
                    mutation
                        .owner_thread_id
                        .unwrap_or(actor_thread_id)
                        .to_string(),
                )
                .bind(parent_task_id)
                .bind(serde_json::to_string(&depends_on_task_ids)?)
                .bind(title.trim())
                .bind(estimate.lower_seconds)
                .bind(estimate.upper_seconds)
                .bind(datetime_to_epoch_seconds(now))
                .bind(datetime_to_epoch_seconds(now))
                .bind(sequence)
                .execute(&mut **tx)
                .await?;
                if mutation.estimate.is_some() {
                    task_estimate_storage::insert_revision(
                        tx,
                        root_thread_id,
                        &task_id,
                        estimate,
                        mutation.reason.as_deref(),
                        now,
                        actor_thread_id,
                    )
                    .await?;
                }
                Ok(Some(task_id))
            }
            action => {
                let task_id = mutation
                    .task_id
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("task updates require task_id"))?;
                validate_task_id(task_id)?;
                let Some(existing) =
                    task_estimate_storage::load_task(tx, root_thread_id, task_id).await?
                else {
                    return Err(anyhow::anyhow!("task not found: {task_id}"));
                };
                ensure_actor_can_mutate(&existing, root_thread_id, actor_thread_id)?;
                if existing.status.is_terminal() {
                    if matches!(
                        action,
                        TaskEstimateAction::Complete | TaskEstimateAction::Cancel
                    ) && matches_terminal_action(existing.status, action)
                    {
                        return Ok(None);
                    }
                    return Err(anyhow::anyhow!(
                        "terminal ETA task cannot transition to another state"
                    ));
                }
                let mut current_range = existing.current_range();
                if let Some(estimate) = mutation.estimate {
                    current_range = estimate;
                }
                if let Some(title) = mutation.title.as_deref() {
                    validate_task_title(title)?;
                }
                let parent_task_id = mutation
                    .parent_task_id
                    .as_deref()
                    .or(existing.parent_task_id.as_deref());
                let depends_on_task_ids = mutation
                    .depends_on_task_ids
                    .as_deref()
                    .unwrap_or(&existing.depends_on_task_ids);
                if mutation.parent_task_id.is_some() || mutation.depends_on_task_ids.is_some() {
                    validate_dependencies(task_id, parent_task_id, depends_on_task_ids)?;
                    task_estimate_storage::ensure_related_tasks_exist(
                        tx,
                        root_thread_id,
                        parent_task_id,
                        depends_on_task_ids,
                    )
                    .await?;
                }
                let status = match action {
                    TaskEstimateAction::Start | TaskEstimateAction::Revise => {
                        if matches!(action, TaskEstimateAction::Revise)
                            && mutation.estimate.is_none()
                            && mutation.owner_thread_id.is_none()
                        {
                            return Err(anyhow::anyhow!(
                                "revising a task requires an estimate range"
                            ));
                        }
                        if matches!(action, TaskEstimateAction::Start) {
                            TaskEstimateStatus::Active
                        } else {
                            existing.status
                        }
                    }
                    TaskEstimateAction::Block => TaskEstimateStatus::Blocked,
                    TaskEstimateAction::Complete => TaskEstimateStatus::Completed,
                    TaskEstimateAction::Cancel => TaskEstimateStatus::Cancelled,
                    TaskEstimateAction::Create => unreachable!(),
                };
                let starts_now =
                    matches!(action, TaskEstimateAction::Start) && existing.started_at.is_none();
                let started_at = if starts_now {
                    Some(now)
                } else {
                    existing.started_at
                };
                let (original_lower_seconds, original_upper_seconds) =
                    if existing.original_range().is_known() {
                        (
                            existing.original_lower_seconds,
                            existing.original_upper_seconds,
                        )
                    } else if starts_now && current_range.is_known() {
                        (current_range.lower_seconds, current_range.upper_seconds)
                    } else {
                        (
                            existing.original_lower_seconds,
                            existing.original_upper_seconds,
                        )
                    };
                let terminal_at = status
                    .is_terminal()
                    .then_some(existing.terminal_at.unwrap_or(now));
                let actual_elapsed_seconds = status
                    .is_terminal()
                    .then(|| {
                        started_at.map(|started_at| {
                            (terminal_at.unwrap_or(now) - started_at)
                                .num_seconds()
                                .max(0)
                        })
                    })
                    .flatten();
                // Reassignment changes the reminder recipient, not the estimate itself. Keep the
                // saved update timestamp so an already-aged task is re-armed for its new owner
                // at the same freshness/overdue deadlines instead of receiving a free extension.
                let owner_only_reassignment = matches!(action, TaskEstimateAction::Revise)
                    && mutation.owner_thread_id.is_some()
                    && mutation.estimate.is_none()
                    && mutation.title.is_none()
                    && mutation.parent_task_id.is_none()
                    && mutation.depends_on_task_ids.is_none();
                let updated_at = if owner_only_reassignment {
                    existing.updated_at
                } else {
                    now
                };
                sqlx::query(
                    r#"
UPDATE eta_tasks
SET parent_task_id = COALESCE(?, parent_task_id),
    depends_on_task_ids = COALESCE(?, depends_on_task_ids),
    title = COALESCE(?, title),
    owner_thread_id = COALESCE(?, owner_thread_id),
    status = ?,
    current_lower_seconds = ?,
    current_upper_seconds = ?,
    original_lower_seconds = ?,
    original_upper_seconds = ?,
    started_at = ?,
    terminal_at = ?,
    actual_elapsed_seconds = ?,
    updated_at = ?,
    updated_sequence = ?
WHERE task_id = ? AND root_thread_id = ?
                    "#,
                )
                .bind(mutation.parent_task_id.as_deref())
                .bind(
                    mutation
                        .depends_on_task_ids
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                )
                .bind(mutation.title.as_deref().map(str::trim))
                .bind(mutation.owner_thread_id.map(|owner| owner.to_string()))
                .bind(status.as_str())
                .bind(current_range.lower_seconds)
                .bind(current_range.upper_seconds)
                .bind(original_lower_seconds)
                .bind(original_upper_seconds)
                .bind(started_at.map(datetime_to_epoch_seconds))
                .bind(terminal_at.map(datetime_to_epoch_seconds))
                .bind(actual_elapsed_seconds)
                .bind(datetime_to_epoch_seconds(updated_at))
                .bind(sequence)
                .bind(task_id)
                .bind(root_thread_id.to_string())
                .execute(&mut **tx)
                .await?;
                if mutation.estimate.is_some() {
                    task_estimate_storage::insert_revision(
                        tx,
                        root_thread_id,
                        task_id,
                        current_range,
                        mutation.reason.as_deref(),
                        now,
                        actor_thread_id,
                    )
                    .await?;
                }
                task_estimate_storage::trim_revisions(tx, root_thread_id, task_id).await?;
                Ok(Some(task_id.to_string()))
            }
        }
    }
}

impl StateRuntime {
    /// Persist the root-owned freshness minimum used by ETA projections and reminders.
    pub async fn set_eta_freshness_minimum_seconds(
        &self,
        root_thread_id: ThreadId,
        freshness_minimum_seconds: i64,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO eta_roots (root_thread_id, sequence) VALUES (?, 0) ON CONFLICT(root_thread_id) DO NOTHING",
        )
        .bind(root_thread_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE eta_roots SET freshness_minimum_seconds = ? WHERE root_thread_id = ?")
            .bind(freshness_minimum_seconds.max(0))
            .bind(root_thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Initialize a missing root-owned freshness minimum without replacing a concurrent policy.
    pub async fn initialize_eta_freshness_minimum_seconds(
        &self,
        root_thread_id: ThreadId,
        freshness_minimum_seconds: i64,
    ) -> anyhow::Result<i64> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO eta_roots (root_thread_id, sequence, freshness_minimum_seconds) VALUES (?, 0, ?) ON CONFLICT(root_thread_id) DO UPDATE SET freshness_minimum_seconds = COALESCE(eta_roots.freshness_minimum_seconds, excluded.freshness_minimum_seconds)",
        )
        .bind(root_thread_id.to_string())
        .bind(freshness_minimum_seconds.max(0))
        .execute(&mut *tx)
        .await?;
        let persisted = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT freshness_minimum_seconds FROM eta_roots WHERE root_thread_id = ?",
        )
        .bind(root_thread_id.to_string())
        .fetch_one(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("ETA freshness policy initialization returned NULL"))?;
        tx.commit().await?;
        Ok(persisted)
    }

    /// Read the persisted root-owned freshness minimum, if the ETA root exists.
    pub async fn eta_freshness_minimum_seconds(
        &self,
        root_thread_id: ThreadId,
    ) -> anyhow::Result<Option<i64>> {
        Ok(sqlx::query_scalar::<_, Option<i64>>(
            "SELECT freshness_minimum_seconds FROM eta_roots WHERE root_thread_id = ?",
        )
        .bind(root_thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?
        .flatten())
    }

    pub async fn read_task_estimate_snapshot(
        &self,
        root_thread_id: ThreadId,
        now: DateTime<Utc>,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<TaskEstimateSnapshot> {
        self.read_task_estimate_snapshot_with_freshness_minimum(
            root_thread_id,
            now,
            cursor,
            limit,
            DEFAULT_FRESHNESS_MINIMUM_SECONDS,
        )
        .await
    }

    pub async fn read_task_estimate_snapshot_with_freshness_minimum(
        &self,
        root_thread_id: ThreadId,
        now: DateTime<Utc>,
        cursor: Option<&str>,
        limit: Option<usize>,
        freshness_minimum_seconds: i64,
    ) -> anyhow::Result<TaskEstimateSnapshot> {
        self.task_estimates()
            .read_snapshot_with_freshness_minimum(
                root_thread_id,
                now,
                cursor,
                limit,
                freshness_minimum_seconds,
            )
            .await
    }

    pub async fn apply_task_estimate_mutations(
        &self,
        root_thread_id: ThreadId,
        actor_thread_id: ThreadId,
        mutations: &[TaskEstimateMutation],
        now: DateTime<Utc>,
    ) -> anyhow::Result<TaskEstimateUpdateResult> {
        self.apply_task_estimate_mutations_with_freshness_minimum(
            root_thread_id,
            actor_thread_id,
            mutations,
            now,
            DEFAULT_FRESHNESS_MINIMUM_SECONDS,
        )
        .await
    }

    pub async fn apply_task_estimate_mutations_with_freshness_minimum(
        &self,
        root_thread_id: ThreadId,
        actor_thread_id: ThreadId,
        mutations: &[TaskEstimateMutation],
        now: DateTime<Utc>,
        freshness_minimum_seconds: i64,
    ) -> anyhow::Result<TaskEstimateUpdateResult> {
        self.task_estimates()
            .apply_mutations_with_freshness_minimum(
                root_thread_id,
                actor_thread_id,
                mutations,
                now,
                freshness_minimum_seconds,
            )
            .await
    }
}

fn ensure_actor_can_mutate(
    task: &TaskEstimate,
    root_thread_id: ThreadId,
    actor_thread_id: ThreadId,
) -> anyhow::Result<()> {
    if actor_thread_id != root_thread_id && task.owner_thread_id != actor_thread_id {
        return Err(anyhow::anyhow!("task estimate is owned by another agent"));
    }
    Ok(())
}

fn matches_terminal_action(status: TaskEstimateStatus, action: TaskEstimateAction) -> bool {
    matches!(
        (status, action),
        (TaskEstimateStatus::Completed, TaskEstimateAction::Complete)
            | (TaskEstimateStatus::Cancelled, TaskEstimateAction::Cancel)
    )
}

#[cfg(test)]
#[path = "task_estimates_tests.rs"]
mod tests;
