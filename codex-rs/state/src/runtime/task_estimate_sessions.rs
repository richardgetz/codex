use super::super::StateRuntime;
use crate::TaskEstimateSessionPage;
use crate::TaskEstimateSessionRow;
use crate::TaskEstimateStatus;
use crate::model::datetime_to_epoch_seconds;
use crate::model::epoch_millis_to_datetime;
use crate::model::epoch_seconds_to_datetime;
use crate::model::validate_task_id;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::sqlite::SqliteRow;

use super::TaskEstimateStore;

const DEFAULT_HISTORY_LIMIT: usize = 50;
const MAX_HISTORY_LIMIT: usize = 100;

impl TaskEstimateStore {
    /// Delete terminal task history older than `retention_days`.
    ///
    /// A zero-day setting means unlimited retention. Unfinished tasks are never removed, and a
    /// terminal grouping row with a retained child is kept so its parent metadata remains
    /// available to aggregate readers.
    pub async fn prune_history(
        &self,
        retention_days: u64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<u64> {
        if retention_days == 0 {
            return Ok(0);
        }
        let retention_seconds = i64::try_from(retention_days)
            .ok()
            .and_then(|days| days.checked_mul(24 * 60 * 60))
            .unwrap_or(i64::MAX);
        let cutoff = datetime_to_epoch_seconds(now).saturating_sub(retention_seconds);
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            r#"
WITH RECURSIVE task_edges(root_thread_id, child_task_id, ancestor_task_id) AS (
    SELECT root_thread_id, task_id, parent_task_id
    FROM eta_tasks
    WHERE parent_task_id IS NOT NULL
    UNION ALL
    SELECT eta_tasks.root_thread_id, eta_tasks.task_id, dependency.value
    FROM eta_tasks, json_each(eta_tasks.depends_on_task_ids) dependency
), retained(root_thread_id, task_id) AS (
    SELECT root_thread_id, task_id
    FROM eta_tasks
    WHERE status NOT IN ('completed', 'cancelled')
       OR terminal_at IS NULL
       OR terminal_at >= ?
    UNION
    SELECT task_edges.root_thread_id, task_edges.ancestor_task_id
    FROM retained
    JOIN task_edges
      ON task_edges.root_thread_id = retained.root_thread_id
     AND task_edges.child_task_id = retained.task_id
)
DELETE FROM eta_tasks
WHERE status IN ('completed', 'cancelled')
  AND terminal_at IS NOT NULL
  AND terminal_at < ?
  AND NOT EXISTS (
      SELECT 1
      FROM retained
      WHERE retained.root_thread_id = eta_tasks.root_thread_id
        AND retained.task_id = eta_tasks.task_id
  )
            "#,
        )
        .bind(cutoff)
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    /// Read a bounded task-first page spanning every persisted ETA root.
    pub async fn list_sessions(
        &self,
        cursor: Option<&str>,
        limit: Option<usize>,
        include_nested: bool,
    ) -> anyhow::Result<TaskEstimateSessionPage> {
        let limit = limit
            .unwrap_or(DEFAULT_HISTORY_LIMIT)
            .clamp(1, MAX_HISTORY_LIMIT);
        let cursor = cursor.map(parse_session_cursor).transpose()?;
        let mut builder = sqlx::QueryBuilder::<Sqlite>::new(
            r#"
WITH RECURSIVE descendants(root_thread_id, ancestor_task_id, descendant_task_id) AS (
    SELECT root_thread_id, parent_task_id, task_id
    FROM eta_tasks
    WHERE parent_task_id IS NOT NULL
    UNION
    SELECT descendants.root_thread_id,
           descendants.ancestor_task_id,
           eta_tasks.task_id
    FROM descendants
    JOIN eta_tasks
      ON eta_tasks.root_thread_id = descendants.root_thread_id
     AND eta_tasks.parent_task_id = descendants.descendant_task_id
), nested_summary AS (
    SELECT descendants.root_thread_id,
           descendants.ancestor_task_id,
           COUNT(*) AS nested_task_count,
           SUM(CASE
                   WHEN eta_tasks.status NOT IN ('completed', 'cancelled') THEN 1
                   ELSE 0
               END) AS active_nested_task_count,
           CASE
               WHEN SUM(CASE
                            WHEN eta_tasks.status NOT IN ('completed', 'cancelled')
                             AND eta_tasks.current_lower_seconds IS NULL THEN 1
                            ELSE 0
                        END) > 0 THEN NULL
               ELSE SUM(CASE
                            WHEN eta_tasks.status NOT IN ('completed', 'cancelled')
                            THEN eta_tasks.current_lower_seconds
                            ELSE 0
                        END)
           END AS nested_lower_seconds,
           CASE
               WHEN SUM(CASE
                            WHEN eta_tasks.status NOT IN ('completed', 'cancelled')
                             AND eta_tasks.current_upper_seconds IS NULL THEN 1
                            ELSE 0
                        END) > 0 THEN NULL
               ELSE SUM(CASE
                            WHEN eta_tasks.status NOT IN ('completed', 'cancelled')
                            THEN eta_tasks.current_upper_seconds
                            ELSE 0
                        END)
           END AS nested_upper_seconds
    FROM descendants
    JOIN eta_tasks
      ON eta_tasks.root_thread_id = descendants.root_thread_id
     AND eta_tasks.task_id = descendants.descendant_task_id
    GROUP BY descendants.root_thread_id, descendants.ancestor_task_id
)
SELECT eta_tasks.task_id,
       eta_tasks.root_thread_id,
       eta_tasks.owner_thread_id,
       eta_tasks.parent_task_id,
       eta_tasks.title,
       eta_tasks.status,
       eta_tasks.current_lower_seconds,
       eta_tasks.current_upper_seconds,
       eta_tasks.original_lower_seconds,
       eta_tasks.original_upper_seconds,
       eta_tasks.created_at,
       eta_tasks.started_at,
       eta_tasks.terminal_at,
       eta_tasks.actual_elapsed_seconds,
       eta_tasks.updated_at,
       threads.title AS session_title,
       threads.name AS session_name,
       threads.preview AS session_preview,
       COALESCE(threads.created_at_ms, threads.created_at * 1000) AS session_created_at,
       COALESCE(threads.updated_at_ms, threads.updated_at * 1000) AS session_updated_at,
       threads.archived_at AS session_archived_at,
       threads.cwd AS session_cwd,
       COALESCE(nested_summary.nested_task_count, 0) AS nested_task_count,
       COALESCE(nested_summary.active_nested_task_count, 0) AS active_nested_task_count,
       nested_summary.nested_lower_seconds,
       nested_summary.nested_upper_seconds
FROM eta_tasks
JOIN threads ON threads.id = eta_tasks.root_thread_id
LEFT JOIN nested_summary
  ON nested_summary.root_thread_id = eta_tasks.root_thread_id
 AND nested_summary.ancestor_task_id = eta_tasks.task_id
WHERE (
            "#,
        );
        builder
            .push_bind(if include_nested { 1_i64 } else { 0_i64 })
            .push(" = 1 OR eta_tasks.parent_task_id IS NULL)");
        builder.push(" AND (");
        if let Some((updated_at, root_thread_id, task_id)) = cursor.as_ref() {
            builder
                .push("eta_tasks.updated_at < ")
                .push_bind(*updated_at)
                .push(" OR (eta_tasks.updated_at = ")
                .push_bind(*updated_at)
                .push(" AND (eta_tasks.root_thread_id > ")
                .push_bind(root_thread_id)
                .push(" OR (eta_tasks.root_thread_id = ")
                .push_bind(root_thread_id)
                .push(" AND eta_tasks.task_id > ")
                .push_bind(task_id)
                .push(")))");
        } else {
            builder.push("1 = 1");
        }
        builder
            .push(") ORDER BY eta_tasks.updated_at DESC, eta_tasks.root_thread_id, eta_tasks.task_id LIMIT ")
            .push_bind(i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX));
        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        let mut rows = rows
            .into_iter()
            .map(TaskEstimateSessionRow::try_from_row)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let next_cursor = if rows.len() > limit {
            rows.truncate(limit);
            rows.last()
                .map(|row| session_cursor(row.updated_at, row.root_thread_id, row.task_id.clone()))
        } else {
            None
        };
        Ok(TaskEstimateSessionPage { rows, next_cursor })
    }
}

impl StateRuntime {
    /// Prune terminal ETA history according to the configured cross-session retention window.
    pub async fn prune_task_estimate_history(
        &self,
        retention_days: u64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<u64> {
        self.task_estimates().prune_history(retention_days, now).await
    }

    /// Read a bounded task-first page spanning all persisted ETA roots.
    pub async fn list_task_estimate_sessions(
        &self,
        cursor: Option<&str>,
        limit: Option<usize>,
        include_nested: bool,
    ) -> anyhow::Result<TaskEstimateSessionPage> {
        self.task_estimates()
            .list_sessions(cursor, limit, include_nested)
            .await
    }
}

impl TaskEstimateSessionRow {
    fn try_from_row(row: SqliteRow) -> anyhow::Result<Self> {
        Ok(Self {
            task_id: row.try_get("task_id")?,
            root_thread_id: ThreadId::from_string(&row.try_get::<String, _>("root_thread_id")?)?,
            owner_thread_id: ThreadId::from_string(&row.try_get::<String, _>("owner_thread_id")?)?,
            parent_task_id: row.try_get("parent_task_id")?,
            title: row.try_get("title")?,
            status: TaskEstimateStatus::try_from(row.try_get::<String, _>("status")?.as_str())?,
            current_lower_seconds: row.try_get("current_lower_seconds")?,
            current_upper_seconds: row.try_get("current_upper_seconds")?,
            original_lower_seconds: row.try_get("original_lower_seconds")?,
            original_upper_seconds: row.try_get("original_upper_seconds")?,
            created_at: epoch_seconds_to_datetime(row.try_get("created_at")?)?,
            started_at: row
                .try_get::<Option<i64>, _>("started_at")?
                .map(epoch_seconds_to_datetime)
                .transpose()?,
            terminal_at: row
                .try_get::<Option<i64>, _>("terminal_at")?
                .map(epoch_seconds_to_datetime)
                .transpose()?,
            actual_elapsed_seconds: row.try_get("actual_elapsed_seconds")?,
            updated_at: epoch_seconds_to_datetime(row.try_get("updated_at")?)?,
            session_title: row.try_get("session_title")?,
            session_name: row.try_get("session_name")?,
            session_preview: row.try_get("session_preview")?,
            session_created_at: epoch_millis_to_datetime(row.try_get("session_created_at")?)?,
            session_updated_at: epoch_millis_to_datetime(row.try_get("session_updated_at")?)?,
            session_archived_at: row
                .try_get::<Option<i64>, _>("session_archived_at")?
                .map(epoch_seconds_to_datetime)
                .transpose()?,
            session_cwd: row.try_get::<String, _>("session_cwd")?.into(),
            nested_task_count: row.try_get("nested_task_count")?,
            active_nested_task_count: row.try_get("active_nested_task_count")?,
            nested_lower_seconds: row.try_get("nested_lower_seconds")?,
            nested_upper_seconds: row.try_get("nested_upper_seconds")?,
        })
    }
}

fn parse_session_cursor(cursor: &str) -> anyhow::Result<(i64, String, String)> {
    let mut parts = cursor.splitn(3, ':');
    let updated_at = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid ETA session cursor"))?
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("invalid ETA session cursor timestamp"))?;
    let root_thread_id = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid ETA session cursor"))?
        .to_string();
    ThreadId::from_string(&root_thread_id)
        .map_err(|_| anyhow::anyhow!("invalid ETA session cursor root thread"))?;
    let task_id = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid ETA session cursor"))?;
    validate_task_id(task_id)?;
    Ok((updated_at, root_thread_id, task_id.to_string()))
}

fn session_cursor(updated_at: DateTime<Utc>, root_thread_id: ThreadId, task_id: String) -> String {
    format!(
        "{}:{}:{task_id}",
        datetime_to_epoch_seconds(updated_at),
        root_thread_id
    )
}
