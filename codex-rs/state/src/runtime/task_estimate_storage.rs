use super::task_estimates::{MAX_REVISIONS_PER_TASK, MAX_TASKS_PER_ROOT};
use crate::TaskEstimate;
use crate::model::TaskEstimateRow;
use crate::model::datetime_to_epoch_seconds;
use crate::model::task_estimate_from_row;
use crate::model::task_estimate_revision_from_row;
use crate::model::validate_task_id;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::Transaction;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub(super) async fn task_from_row_with_revisions(
    tx: &mut Transaction<'_, Sqlite>,
    row: sqlx::sqlite::SqliteRow,
) -> anyhow::Result<TaskEstimate> {
    let task = task_estimate_from_row(TaskEstimateRow::try_from_row(&row)?)?;
    let revisions = load_revisions(tx, task.root_thread_id, &task.task_id).await?;
    Ok(TaskEstimate { revisions, ..task })
}

pub(super) async fn actor_is_descendant(
    tx: &mut Transaction<'_, Sqlite>,
    root_thread_id: ThreadId,
    actor_thread_id: ThreadId,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"
SELECT EXISTS(
    WITH RECURSIVE subtree(thread_id) AS (
        SELECT child_thread_id FROM thread_spawn_edges WHERE parent_thread_id = ?
        UNION
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

pub(super) async fn ensure_related_tasks_exist(
    tx: &mut Transaction<'_, Sqlite>,
    root_thread_id: ThreadId,
    parent_task_id: Option<&str>,
    depends_on_task_ids: &[String],
) -> anyhow::Result<()> {
    let mut related = depends_on_task_ids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
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
            return Err(anyhow::anyhow!(
                "task relationship references unknown task `{task_id}`"
            ));
        }
    }
    Ok(())
}

pub(super) async fn insert_revision(
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

pub(super) async fn trim_revisions(
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

pub(super) async fn load_task(
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

pub(super) async fn load_relevant_task_map(
    tx: &mut Transaction<'_, Sqlite>,
    root_thread_id: ThreadId,
    seed_task_ids: &BTreeSet<String>,
) -> anyhow::Result<BTreeMap<String, TaskEstimate>> {
    let rows = sqlx::query(
        r#"
SELECT task_id, root_thread_id, owner_thread_id, parent_task_id, depends_on_task_ids, title,
       status, current_lower_seconds, current_upper_seconds, original_lower_seconds,
       original_upper_seconds, created_at, started_at, terminal_at, actual_elapsed_seconds,
       updated_at, updated_sequence
FROM eta_tasks
WHERE root_thread_id = ? AND status IN ('pending', 'active', 'blocked')
        "#,
    )
    .bind(root_thread_id.to_string())
    .fetch_all(&mut **tx)
    .await?;
    let mut tasks = rows
        .into_iter()
        .map(|row| {
            let task = task_estimate_from_row(TaskEstimateRow::try_from_row(&row)?)?;
            Ok((task.task_id.clone(), task))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let mut pending = tasks
        .values()
        .flat_map(|task| {
            task.parent_task_id
                .iter()
                .cloned()
                .chain(task.depends_on_task_ids.iter().cloned())
        })
        .chain(seed_task_ids.iter().cloned())
        .collect::<BTreeSet<_>>();
    // Retain one terminal child for every unfinished group that has no active child. The
    // aggregate uses this bounded marker to keep an explicitly active grouping task in the
    // unknown/awaiting-completion state without scanning its entire terminal history.
    let terminal_group_ids = tasks
        .values()
        .filter(|task| !task.status.is_terminal())
        .filter(|task| {
            !tasks.iter().any(|(_, child)| {
                child.parent_task_id.as_deref() == Some(task.task_id.as_str())
                    && !child.status.is_terminal()
            })
        })
        .map(|task| task.task_id.clone())
        .collect::<Vec<_>>();
    for parent_task_id in terminal_group_ids {
        let terminal_child_id = sqlx::query_scalar::<_, String>(
            "SELECT task_id FROM eta_tasks WHERE root_thread_id = ? AND parent_task_id = ? AND status IN ('completed', 'cancelled') ORDER BY terminal_at DESC, task_id DESC LIMIT 1",
        )
        .bind(root_thread_id.to_string())
        .bind(&parent_task_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(terminal_child_id) = terminal_child_id else {
            continue;
        };
        let Some(task) = load_task(tx, root_thread_id, &terminal_child_id).await? else {
            return Err(anyhow::anyhow!(
                "task relationship references unknown task `{terminal_child_id}`"
            ));
        };
        pending.extend(
            task.parent_task_id
                .iter()
                .cloned()
                .chain(task.depends_on_task_ids.iter().cloned()),
        );
        tasks.insert(task.task_id.clone(), task);
    }
    const MAX_RELEVANT_TASKS: usize = MAX_TASKS_PER_ROOT * 64;
    while let Some(task_id) = pending.pop_first() {
        if tasks.contains_key(&task_id) {
            continue;
        }
        let Some(task) = load_task(tx, root_thread_id, &task_id).await? else {
            return Err(anyhow::anyhow!(
                "task relationship references unknown task `{task_id}`"
            ));
        };
        pending.extend(
            task.parent_task_id
                .iter()
                .cloned()
                .chain(task.depends_on_task_ids.iter().cloned()),
        );
        tasks.insert(task_id, task);
        if tasks.len() > MAX_RELEVANT_TASKS {
            return Err(anyhow::anyhow!(
                "ETA dependency graph exceeds the bounded aggregate read"
            ));
        }
    }
    if tasks.is_empty() {
        // A root with only terminal History still has a completed aggregate. Read one terminal
        // row as a sentinel instead of scanning the unbounded lifetime History.
        let row = sqlx::query(
            r#"
SELECT task_id, root_thread_id, owner_thread_id, parent_task_id, depends_on_task_ids, title,
       status, current_lower_seconds, current_upper_seconds, original_lower_seconds,
       original_upper_seconds, created_at, started_at, terminal_at, actual_elapsed_seconds,
       updated_at, updated_sequence
FROM eta_tasks
WHERE root_thread_id = ? AND status IN ('completed', 'cancelled')
ORDER BY terminal_at DESC, task_id DESC
LIMIT 1
            "#,
        )
        .bind(root_thread_id.to_string())
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(row) = row {
            let task = task_estimate_from_row(TaskEstimateRow::try_from_row(&row)?)?;
            tasks.insert(task.task_id.clone(), task);
        }
    }
    Ok(tasks)
}

pub(super) async fn load_revisions(
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
    .map(task_estimate_revision_from_row)
    .collect()
}

pub(super) fn parse_history_cursor(cursor: &str) -> anyhow::Result<(i64, String)> {
    let (timestamp, task_id) = cursor
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid ETA history cursor"))?;
    let timestamp = timestamp
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("invalid ETA history cursor timestamp"))?;
    validate_task_id(task_id)?;
    Ok((timestamp, task_id.to_string()))
}

pub(super) fn history_cursor(task: &TaskEstimate) -> String {
    format!(
        "{}:{}",
        task.terminal_at
            .map(datetime_to_epoch_seconds)
            .unwrap_or_default(),
        task.task_id
    )
}

pub(super) fn validate_parent_graph(
    tasks: &BTreeMap<String, TaskEstimate>,
) -> anyhow::Result<()> {
    for task in tasks.values() {
        let mut current = task.parent_task_id.as_deref();
        let mut seen = BTreeSet::new();
        while let Some(parent_id) = current {
            if !seen.insert(parent_id) {
                return Err(anyhow::anyhow!("task parent graph contains a cycle"));
            }
            let parent = tasks.get(parent_id).ok_or_else(|| {
                anyhow::anyhow!("task parent references unknown task `{parent_id}`")
            })?;
            current = parent.parent_task_id.as_deref();
        }
    }
    Ok(())
}
