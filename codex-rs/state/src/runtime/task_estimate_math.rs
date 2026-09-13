use crate::TaskEstimate;
use crate::TaskEstimateOverall;
use crate::TaskEstimateStatus;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

const STALE_AFTER_SECONDS: i64 = 15 * 60;

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
    if active_tasks.iter().any(|task| {
        now.timestamp()
            .saturating_sub(task.updated_at.timestamp())
            > STALE_AFTER_SECONDS
    }) {
        return TaskEstimateOverall::unknown("stale task update");
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
        let (mut path_lower, mut path_upper) = match estimate_path(
            task_id,
            tasks,
            now,
            &mut memo,
            &mut BTreeSet::new(),
        ) {
            Ok(path) => path,
            Err(reason) => return TaskEstimateOverall::unknown(reason),
        };
        let mut existing_dependencies = BTreeSet::new();
        if let Err(reason) = collect_dependency_ids(
            task_id,
            tasks,
            &mut existing_dependencies,
            &mut BTreeSet::new(),
        ) {
            return TaskEstimateOverall::unknown(reason);
        }
        let inherited_dependencies = match inherited_group_dependencies(task_id, tasks) {
            Ok(dependencies) => dependencies,
            Err(reason) => return TaskEstimateOverall::unknown(reason),
        };
        for dependency_id in inherited_dependencies {
            if existing_dependencies.contains(&dependency_id) {
                continue;
            }
            let (dependency_lower, dependency_upper) = match estimate_path(
                &dependency_id,
                tasks,
                now,
                &mut memo,
                &mut BTreeSet::new(),
            ) {
                Ok(path) => path,
                Err(reason) => return TaskEstimateOverall::unknown(reason),
            };
            path_lower = path_lower.saturating_add(dependency_lower);
            path_upper = path_upper.saturating_add(dependency_upper);
        }
        lower = lower.max(path_lower);
        upper = upper.max(path_upper);
    }
    let Some(finish_at) = now.checked_add_signed(Duration::seconds(upper)) else {
        return TaskEstimateOverall::unknown("estimate exceeds supported ETA horizon");
    };
    TaskEstimateOverall {
        finish_at: Some(finish_at),
        remaining_lower_seconds: Some(lower),
        remaining_upper_seconds: Some(upper),
        unknown_reason: None,
    }
}

fn collect_dependency_ids(
    task_id: &str,
    tasks: &[TaskEstimate],
    dependencies: &mut BTreeSet<String>,
    visiting: &mut BTreeSet<String>,
) -> Result<(), String> {
    if !visiting.insert(task_id.to_string()) {
        return Err("task dependency graph contains a cycle".to_string());
    }
    let task = tasks
        .iter()
        .find(|task| task.task_id == task_id)
        .ok_or_else(|| format!("unknown dependency task `{task_id}`"))?;
    for dependency_id in &task.depends_on_task_ids {
        dependencies.insert(dependency_id.clone());
        collect_dependency_ids(dependency_id, tasks, dependencies, visiting)?;
    }
    visiting.remove(task_id);
    Ok(())
}

fn inherited_group_dependencies(
    task_id: &str,
    tasks: &[TaskEstimate],
) -> Result<Vec<String>, String> {
    let mut dependencies = BTreeSet::new();
    let mut seen_parents = BTreeSet::new();
    let mut parent_id = tasks
        .iter()
        .find(|task| task.task_id == task_id)
        .ok_or_else(|| format!("unknown dependency task `{task_id}`"))?
        .parent_task_id
        .clone();
    while let Some(current_parent_id) = parent_id {
        if !seen_parents.insert(current_parent_id.clone()) {
            return Err("task parent graph contains a cycle".to_string());
        }
        let parent = tasks
            .iter()
            .find(|task| task.task_id == current_parent_id)
            .ok_or_else(|| format!("unknown parent task `{current_parent_id}`"))?;
        if !parent.status.is_terminal() {
            for dependency_id in &parent.depends_on_task_ids {
                if is_parent_descendant(dependency_id, &current_parent_id, tasks)? {
                    return Err("task grouping and dependency graphs contain a cycle".to_string());
                }
                dependencies.insert(dependency_id.clone());
            }
        }
        parent_id = parent.parent_task_id.clone();
    }
    Ok(dependencies.into_iter().collect())
}

fn is_parent_descendant(
    task_id: &str,
    ancestor_id: &str,
    tasks: &[TaskEstimate],
) -> Result<bool, String> {
    let mut current = Some(task_id.to_string());
    let mut seen = BTreeSet::new();
    while let Some(current_id) = current {
        if !seen.insert(current_id.clone()) {
            return Err("task parent graph contains a cycle".to_string());
        }
        if current_id == ancestor_id {
            return Ok(true);
        }
        current = tasks
            .iter()
            .find(|task| task.task_id == current_id)
            .ok_or_else(|| format!("unknown dependency task `{current_id}`"))?
            .parent_task_id
            .clone();
    }
    Ok(false)
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
