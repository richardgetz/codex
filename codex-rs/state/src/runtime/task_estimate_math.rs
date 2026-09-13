use crate::TaskEstimate;
use crate::TaskEstimateOverall;
use crate::TaskEstimateStatus;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub(super) fn compute_overall(tasks: &[TaskEstimate], now: DateTime<Utc>) -> TaskEstimateOverall {
    if tasks.is_empty() {
        return TaskEstimateOverall::unknown("no task estimates");
    }
    let active_tasks = tasks
        .iter()
        .filter(|task| !task.status.is_terminal())
        .collect::<Vec<_>>();
    if active_tasks.is_empty() {
        return TaskEstimateOverall {
            finish_at: Some(now),
            remaining_lower_seconds: Some(0),
            remaining_upper_seconds: Some(0),
            unknown_reason: None,
        };
    }
    if active_tasks
        .iter()
        .any(|task| task.status == TaskEstimateStatus::Blocked)
    {
        return TaskEstimateOverall::unknown("blocked task");
    }
    let task_ids_with_children = tasks
        .iter()
        .filter_map(|task| task.parent_task_id.as_deref())
        .collect::<BTreeSet<_>>();
    let active_task_ids = active_tasks
        .iter()
        .map(|task| task.task_id.as_str())
        .collect::<BTreeSet<_>>();
    if active_tasks.iter().any(|task| {
        task_ids_with_children.contains(task.task_id.as_str())
            && !tasks.iter().any(|child| {
                child.parent_task_id.as_deref() == Some(task.task_id.as_str())
                    && active_task_ids.contains(child.task_id.as_str())
            })
    }) {
        return TaskEstimateOverall::unknown("task group awaits explicit completion");
    }
    let active_children = active_tasks
        .iter()
        .filter_map(|task| task.parent_task_id.as_deref())
        .collect::<BTreeSet<_>>();
    let mut memo = BTreeMap::new();
    let mut leaves = active_tasks
        .iter()
        .filter(|task| !active_children.contains(task.task_id.as_str()))
        .map(|task| task.task_id.as_str())
        .collect::<Vec<_>>();
    leaves.sort_unstable();
    if leaves.is_empty() {
        return TaskEstimateOverall::unknown("active task group has no executable leaf");
    }
    let mut lower = 0_i64;
    let mut upper = 0_i64;
    for task_id in leaves {
        let (path_lower, path_upper) = match estimate_path(
            task_id,
            tasks,
            now,
            &mut memo,
            &mut BTreeSet::new(),
        ) {
            Ok(path) => path,
            Err(reason) => return TaskEstimateOverall::unknown(reason),
        };
        lower = lower.max(path_lower);
        upper = upper.max(path_upper);
    }
    TaskEstimateOverall {
        finish_at: Some(now + Duration::seconds(upper)),
        remaining_lower_seconds: Some(lower),
        remaining_upper_seconds: Some(upper),
        unknown_reason: None,
    }
}

fn estimate_path(
    task_id: &str,
    tasks: &[TaskEstimate],
    now: DateTime<Utc>,
    memo: &mut BTreeMap<String, (i64, i64)>,
    visiting: &mut BTreeSet<String>,
) -> Result<(i64, i64), String> {
    if let Some(path) = memo.get(task_id) {
        return Ok(*path);
    }
    if !visiting.insert(task_id.to_string()) {
        return Err("task dependency graph contains a cycle".to_string());
    }
    let task = tasks
        .iter()
        .find(|task| task.task_id == task_id)
        .ok_or_else(|| format!("unknown dependency task `{task_id}`"))?;
    if task.status == TaskEstimateStatus::Blocked {
        return Err(format!("dependency `{task_id}` is blocked"));
    }
    if matches!(task.status, TaskEstimateStatus::Completed) {
        visiting.remove(task_id);
        return Ok((0, 0));
    }
    if task.status == TaskEstimateStatus::Cancelled {
        return Err(format!("dependency `{task_id}` was cancelled"));
    }

    // Parent task rows are grouping/finalization records while they have unfinished children;
    // count their executable leaves instead of adding a second estimate for the group itself.
    let active_children = tasks
        .iter()
        .filter(|child| {
            child.parent_task_id.as_deref() == Some(task_id) && !child.status.is_terminal()
        })
        .map(|child| child.task_id.as_str())
        .collect::<Vec<_>>();
    if !active_children.is_empty() {
        let mut child_lower = 0_i64;
        let mut child_upper = 0_i64;
        for child_id in active_children {
            let (lower, upper) = estimate_path(child_id, tasks, now, memo, visiting)?;
            child_lower = child_lower.max(lower);
            child_upper = child_upper.max(upper);
        }
        let mut dependency_lower = 0_i64;
        let mut dependency_upper = 0_i64;
        for dependency_id in &task.depends_on_task_ids {
            let (lower, upper) = estimate_path(dependency_id, tasks, now, memo, visiting)?;
            dependency_lower = dependency_lower.max(lower);
            dependency_upper = dependency_upper.max(upper);
        }
        let path = (
            child_lower.saturating_add(dependency_lower),
            child_upper.saturating_add(dependency_upper),
        );
        visiting.remove(task_id);
        memo.insert(task_id.to_string(), path);
        return Ok(path);
    }

    let remaining = task.remaining_range(now);
    if let (Some(started_at), Some(upper)) = (task.started_at, task.current_upper_seconds) {
        let elapsed = (now - started_at).num_seconds().max(0);
        if elapsed > upper {
            return Err(format!("task `{task_id}` estimate has elapsed"));
        }
    }
    let (own_lower, own_upper) = match (remaining.lower_seconds, remaining.upper_seconds) {
        (Some(lower), Some(upper)) => (lower, upper),
        _ => return Err(format!("task `{task_id}` has no estimate")),
    };
    let mut dependency_lower = 0_i64;
    let mut dependency_upper = 0_i64;
    for dependency_id in &task.depends_on_task_ids {
        let (lower, upper) = estimate_path(dependency_id, tasks, now, memo, visiting)?;
        dependency_lower = dependency_lower.max(lower);
        dependency_upper = dependency_upper.max(upper);
    }
    let path = (
        own_lower.saturating_add(dependency_lower),
        own_upper.saturating_add(dependency_upper),
    );
    visiting.remove(task_id);
    memo.insert(task_id.to_string(), path);
    Ok(path)
}
