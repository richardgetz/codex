use super::StateRuntime;
use crate::TaskEstimate;
use crate::TaskEstimateAction;
use crate::TaskEstimateHistoryPage;
use crate::TaskEstimateMutation;
use crate::TaskEstimateOverall;
use crate::TaskEstimateSnapshot;
use crate::TaskEstimateStatus;
use crate::TaskEstimateUpdateResult;
use crate::model::TaskEstimateRow;
use crate::model::datetime_to_epoch_seconds;
use crate::model::task_estimate_from_row;
use crate::model::validate_dependencies;
use crate::model::validate_dependency_graph;
use crate::model::validate_task_reason;
use crate::model::validate_task_title;
use crate::model::validate_task_id;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;

mod task_estimate_math;
use self::task_estimate_math::compute_overall;

const MAX_TASKS_PER_ROOT: usize = 256;
const MAX_REVISIONS_PER_TASK: usize = 32;
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

    /// Read active tasks and a bounded terminal history page without changing any state.
    pub async fn read_snapshot(
        &self,
        root_thread_id: ThreadId,
        now: DateTime<Utc>,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<TaskEstimateSnapshot> {
        let limit = limit.unwrap_or(DEFAULT_HISTORY_LIMIT).clamp(1, MAX_HISTORY_LIMIT);
        let sequence = sqlx::query_scalar::<_, i64>(
            "SELECT sequence FROM eta_roots WHERE root_thread_id = ?",
        )
        .bind(root_thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
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
        .fetch_all(self.pool.as_ref())
        .await?;
        let mut active = Vec::with_capacity(active_rows.len());
        for row in active_rows {
            active.push(self.task_from_row_with_revisions(row).await?);
        }

        let (cursor_terminal_at, cursor_task_id) = cursor
            .map(parse_history_cursor)
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
        .fetch_all(self.pool.as_ref())
        .await?;
        let mut history = Vec::with_capacity(history_rows.len().min(limit));
        for row in history_rows {
            history.push(self.task_from_row_with_revisions(row).await?);
        }
        let next_cursor = if history.len() > limit {
            history.pop().map(|task| history_cursor(&task))
        } else {
            None
        };

        // Keep aggregate dependency resolution independent from the paginated History output.
        // Root task count is bounded at creation, so this read remains bounded even when the
        // caller is viewing a later History page.
        let all_rows = sqlx::query(
            r#"
SELECT task_id, root_thread_id, owner_thread_id, parent_task_id, depends_on_task_ids, title,
       status, current_lower_seconds, current_upper_seconds, original_lower_seconds,
       original_upper_seconds, created_at, started_at, terminal_at, actual_elapsed_seconds,
       updated_at, updated_sequence
FROM eta_tasks
WHERE root_thread_id = ?
            "#,
        )
        .bind(root_thread_id.to_string())
        .fetch_all(self.pool.as_ref())
        .await?;
        let all_tasks = all_rows
            .into_iter()
            .map(|row| task_estimate_from_row(TaskEstimateRow::try_from_row(&row)?))
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(TaskEstimateSnapshot {
            root_thread_id,
            generated_at: now,
            sequence,
            overall: compute_overall(&all_tasks, now),
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
        if mutations.is_empty() {
            let snapshot = self
                .read_snapshot(root_thread_id, now, None, Some(DEFAULT_HISTORY_LIMIT))
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
            && !actor_is_descendant(&mut tx, root_thread_id, actor_thread_id).await?
        {
            return Err(anyhow::anyhow!(
                "task estimate actor is not part of the requested root session"
            ));
        }
        let previous_sequence = sqlx::query_scalar::<_, i64>(
            "SELECT sequence FROM eta_roots WHERE root_thread_id = ?",
        )
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
            let task_map = load_task_map(&mut tx, root_thread_id).await?;
            validate_dependency_graph(&task_map)?;
            validate_parent_graph(&task_map)?;
        }
        let all_tasks = load_task_map(&mut tx, root_thread_id).await?;
        if changed_ids.is_empty() {
            let overall = compute_overall(&all_tasks.values().cloned().collect::<Vec<_>>(), now);
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
            let revisions = load_revisions(&mut tx, root_thread_id, &task_id).await?;
            changed_tasks.push(TaskEstimate {
                revisions,
                ..task
            });
        }
        let overall = compute_overall(&all_tasks.values().cloned().collect::<Vec<_>>(), now);
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
                    "SELECT COUNT(*) FROM eta_tasks WHERE root_thread_id = ?",
                )
                .bind(root_thread_id.to_string())
                .fetch_one(&mut **tx)
                .await?;
                if usize::try_from(existing_count).unwrap_or(MAX_TASKS_PER_ROOT) >= MAX_TASKS_PER_ROOT
                {
                    return Err(anyhow::anyhow!(
                        "a root session may contain at most {MAX_TASKS_PER_ROOT} ETA tasks"
                    ));
                }
                let parent_task_id = mutation.parent_task_id.as_deref();
                let depends_on_task_ids = mutation
                    .depends_on_task_ids
                    .clone()
                    .unwrap_or_default();
                validate_dependencies(&task_id, parent_task_id, &depends_on_task_ids)?;
                ensure_related_tasks_exist(
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
                .bind(actor_thread_id.to_string())
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
                    insert_revision(
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
                let Some(existing) = load_task(tx, root_thread_id, task_id).await? else {
                    return Err(anyhow::anyhow!("task not found: {task_id}"));
                };
                ensure_actor_can_mutate(&existing, root_thread_id, actor_thread_id)?;
                if existing.status.is_terminal() {
                    if matches!(action, TaskEstimateAction::Complete | TaskEstimateAction::Cancel)
                        && matches_terminal_action(existing.status, action)
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
                    ensure_related_tasks_exist(
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
                let started_at = if matches!(action, TaskEstimateAction::Start)
                    && existing.started_at.is_none()
                {
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
                    } else if started_at.is_some_and(|_| current_range.is_known())
                        && matches!(action, TaskEstimateAction::Start)
                    {
                        (current_range.lower_seconds, current_range.upper_seconds)
                    } else {
                        (existing.original_lower_seconds, existing.original_upper_seconds)
                    };
                let terminal_at = status.is_terminal()
                    .then_some(existing.terminal_at.unwrap_or(now));
                let actual_elapsed_seconds = status.is_terminal()
                    .then(|| {
                        started_at
                            .map(|started_at| (terminal_at.unwrap_or(now) - started_at).num_seconds().max(0))
                    })
                    .flatten();
                sqlx::query(
                    r#"
UPDATE eta_tasks
SET parent_task_id = COALESCE(?, parent_task_id),
    depends_on_task_ids = COALESCE(?, depends_on_task_ids),
    title = COALESCE(?, title),
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
                .bind(status.as_str())
                .bind(current_range.lower_seconds)
                .bind(current_range.upper_seconds)
                .bind(original_lower_seconds)
                .bind(original_upper_seconds)
                .bind(started_at.map(datetime_to_epoch_seconds))
                .bind(terminal_at.map(datetime_to_epoch_seconds))
                .bind(actual_elapsed_seconds)
                .bind(datetime_to_epoch_seconds(now))
                .bind(sequence)
                .bind(task_id)
                .bind(root_thread_id.to_string())
                .execute(&mut **tx)
                .await?;
                if mutation.estimate.is_some() {
                    insert_revision(
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
                trim_revisions(tx, root_thread_id, task_id).await?;
                Ok(Some(task_id.to_string()))
            }
        }
    }

    async fn task_from_row_with_revisions(
        &self,
        row: sqlx::sqlite::SqliteRow,
    ) -> anyhow::Result<TaskEstimate> {
        let task = task_estimate_from_row(TaskEstimateRow::try_from_row(&row)?)?;
        let revisions = sqlx::query(
            r#"SELECT lower_seconds, upper_seconds, reason, updated_at, actor_thread_id
               FROM eta_task_revisions WHERE root_thread_id = ? AND task_id = ? ORDER BY revision"#,
        )
        .bind(task.root_thread_id.to_string())
        .bind(&task.task_id)
        .fetch_all(self.pool.as_ref())
        .await?
        .iter()
        .map(crate::model::task_estimate_revision_from_row)
        .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(TaskEstimate { revisions, ..task })
    }
}

impl StateRuntime {
    pub async fn read_task_estimate_snapshot(
        &self,
        root_thread_id: ThreadId,
        now: DateTime<Utc>,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<TaskEstimateSnapshot> {
        self.task_estimates()
            .read_snapshot(root_thread_id, now, cursor, limit)
            .await
    }

    pub async fn apply_task_estimate_mutations(
        &self,
        root_thread_id: ThreadId,
        actor_thread_id: ThreadId,
        mutations: &[TaskEstimateMutation],
        now: DateTime<Utc>,
    ) -> anyhow::Result<TaskEstimateUpdateResult> {
        self.task_estimates()
            .apply_mutations(root_thread_id, actor_thread_id, mutations, now)
            .await
    }
}

async fn actor_is_descendant(
    tx: &mut Transaction<'_, Sqlite>,
    root_thread_id: ThreadId,
    actor_thread_id: ThreadId,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"
SELECT EXISTS(
    WITH RECURSIVE subtree(thread_id) AS (
        SELECT child_thread_id FROM thread_spawn_edges WHERE parent_thread_id = ?
        UNION ALL
        SELECT edge.child_thread_id
        FROM thread_spawn_edges edge JOIN subtree ON edge.parent_thread_id = subtree.thread_id
    )
    SELECT 1 FROM subtree WHERE thread_id = ?
)
        "#,
    )
    .bind(root_thread_id.to_string())
    .bind(actor_thread_id.to_string())
    .fetch_one(&mut **tx)
    .await?
        != 0)
}

async fn ensure_related_tasks_exist(
    tx: &mut Transaction<'_, Sqlite>,
    root_thread_id: ThreadId,
    parent_task_id: Option<&str>,
    depends_on_task_ids: &[String],
) -> anyhow::Result<()> {
    let mut related = depends_on_task_ids.iter().map(String::as_str).collect::<Vec<_>>();
    if let Some(parent_task_id) = parent_task_id {
        related.push(parent_task_id);
    }
    for task_id in related {
        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM eta_tasks WHERE root_thread_id = ? AND task_id = ?)",
        )
        .bind(root_thread_id.to_string())
        .bind(task_id)
        .fetch_one(&mut **tx)
        .await?;
        if exists == 0 {
            return Err(anyhow::anyhow!("task relationship references unknown task `{task_id}`"));
        }
    }
    Ok(())
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

async fn insert_revision(
    tx: &mut Transaction<'_, Sqlite>,
    root_thread_id: ThreadId,
    task_id: &str,
    estimate: crate::TaskEstimateRange,
    reason: Option<&str>,
    now: DateTime<Utc>,
    actor_thread_id: ThreadId,
) -> anyhow::Result<()> {
    let next_revision = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(revision), 0) + 1 FROM eta_task_revisions WHERE root_thread_id = ? AND task_id = ?",
    )
    .bind(root_thread_id.to_string())
    .bind(task_id)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO eta_task_revisions (root_thread_id, task_id, revision, lower_seconds, upper_seconds, reason, updated_at, actor_thread_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(root_thread_id.to_string())
    .bind(task_id)
    .bind(next_revision)
    .bind(estimate.lower_seconds)
    .bind(estimate.upper_seconds)
    .bind(reason)
    .bind(datetime_to_epoch_seconds(now))
    .bind(actor_thread_id.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn trim_revisions(
    tx: &mut Transaction<'_, Sqlite>,
    root_thread_id: ThreadId,
    task_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
DELETE FROM eta_task_revisions
WHERE root_thread_id = ? AND task_id = ?
  AND revision NOT IN (
      SELECT revision FROM eta_task_revisions
      WHERE root_thread_id = ? AND task_id = ?
      ORDER BY revision DESC LIMIT ?
  )
        "#,
    )
    .bind(root_thread_id.to_string())
    .bind(task_id)
    .bind(root_thread_id.to_string())
    .bind(task_id)
    .bind(i64::try_from(MAX_REVISIONS_PER_TASK).unwrap_or(i64::MAX))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn load_task(
    tx: &mut Transaction<'_, Sqlite>,
    root_thread_id: ThreadId,
    task_id: &str,
) -> anyhow::Result<Option<TaskEstimate>> {
    let row = sqlx::query(
        r#"
SELECT task_id, root_thread_id, owner_thread_id, parent_task_id, depends_on_task_ids, title,
       status, current_lower_seconds, current_upper_seconds, original_lower_seconds,
       original_upper_seconds, created_at, started_at, terminal_at, actual_elapsed_seconds,
       updated_at, updated_sequence
FROM eta_tasks WHERE root_thread_id = ? AND task_id = ?
        "#,
    )
    .bind(root_thread_id.to_string())
    .bind(task_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| task_estimate_from_row(TaskEstimateRow::try_from_row(&row)?))
        .transpose()
}

async fn load_task_map(
    tx: &mut Transaction<'_, Sqlite>,
    root_thread_id: ThreadId,
) -> anyhow::Result<BTreeMap<String, TaskEstimate>> {
    let rows = sqlx::query(
        r#"
SELECT task_id, root_thread_id, owner_thread_id, parent_task_id, depends_on_task_ids, title,
       status, current_lower_seconds, current_upper_seconds, original_lower_seconds,
       original_upper_seconds, created_at, started_at, terminal_at, actual_elapsed_seconds,
       updated_at, updated_sequence
FROM eta_tasks WHERE root_thread_id = ?
        "#,
    )
    .bind(root_thread_id.to_string())
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter()
        .map(|row| {
            let task = task_estimate_from_row(TaskEstimateRow::try_from_row(&row)?)?;
            Ok((task.task_id.clone(), task))
        })
        .collect()
}

async fn load_revisions(
    tx: &mut Transaction<'_, Sqlite>,
    root_thread_id: ThreadId,
    task_id: &str,
) -> anyhow::Result<Vec<crate::TaskEstimateRevision>> {
    sqlx::query(
        "SELECT lower_seconds, upper_seconds, reason, updated_at, actor_thread_id FROM eta_task_revisions WHERE root_thread_id = ? AND task_id = ? ORDER BY revision",
    )
    .bind(root_thread_id.to_string())
    .bind(task_id)
    .fetch_all(&mut **tx)
    .await?
    .iter()
    .map(crate::model::task_estimate_revision_from_row)
    .collect()
}

fn parse_history_cursor(cursor: &str) -> anyhow::Result<(i64, String)> {
    let (timestamp, task_id) = cursor
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid ETA history cursor"))?;
    let timestamp = timestamp
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("invalid ETA history cursor timestamp"))?;
    validate_task_id(task_id)?;
    Ok((timestamp, task_id.to_string()))
}

fn history_cursor(task: &TaskEstimate) -> String {
    format!(
        "{}:{}",
        task.terminal_at
            .map(datetime_to_epoch_seconds)
            .unwrap_or_default(),
        task.task_id
    )
}

fn validate_parent_graph(tasks: &BTreeMap<String, TaskEstimate>) -> anyhow::Result<()> {
    for task in tasks.values() {
        let mut current = task.parent_task_id.as_deref();
        let mut seen = BTreeSet::new();
        while let Some(parent_id) = current {
            if !seen.insert(parent_id) {
                return Err(anyhow::anyhow!("task parent graph contains a cycle"));
            }
            let parent = tasks
                .get(parent_id)
                .ok_or_else(|| anyhow::anyhow!("task parent references unknown task `{parent_id}`"))?;
            current = parent.parent_task_id.as_deref();
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "task_estimates_tests.rs"]
mod tests;
