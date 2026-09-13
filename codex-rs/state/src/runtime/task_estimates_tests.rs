use super::*;
use crate::DirectionalThreadSpawnEdgeStatus;
use crate::SqliteConfig;
use crate::TaskEstimateRange;
use crate::runtime::test_support::unique_temp_dir;
use chrono::Duration;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(seconds, 0).expect("timestamp")
}

async fn runtime() -> (Arc<StateRuntime>, ThreadId) {
    let home = unique_temp_dir();
    tokio::fs::create_dir_all(&home)
        .await
        .expect("test state directory");
    let runtime = StateRuntime::init(
        SqliteConfig::new_for_testing(home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("state runtime");
    (runtime, ThreadId::new())
}

fn create(task_id: &str, title: &str, estimate: Option<(i64, i64)>) -> TaskEstimateMutation {
    TaskEstimateMutation {
        action: TaskEstimateAction::Create,
        task_id: Some(task_id.to_string()),
        title: Some(title.to_string()),
        parent_task_id: None,
        depends_on_task_ids: None,
        estimate: estimate.map(|(lower_seconds, upper_seconds)| TaskEstimateRange {
            lower_seconds: Some(lower_seconds),
            upper_seconds: Some(upper_seconds),
        }),
        reason: None,
    }
}

fn transition(action: TaskEstimateAction, task_id: &str) -> TaskEstimateMutation {
    TaskEstimateMutation {
        action,
        task_id: Some(task_id.to_string()),
        title: None,
        parent_task_id: None,
        depends_on_task_ids: None,
        estimate: None,
        reason: None,
    }
}

#[tokio::test]
async fn task_completion_freezes_harness_elapsed_and_history() {
    let (runtime, root) = runtime().await;
    let started_at = at(1_700_000_000);
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[create("task", "Task", Some((10, 20)))],
            started_at,
        )
        .await
        .expect("create task");
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Start, "task")],
            started_at,
        )
        .await
        .expect("start task");

    let overdue = runtime
        .read_task_estimate_snapshot(root, started_at + Duration::seconds(30), None, None)
        .await
        .expect("overdue snapshot");
    assert_eq!(overdue.overall, TaskEstimateOverall::unknown("task `task` estimate has elapsed"));
    assert_eq!(overdue.active.len(), 1);
    assert_eq!(overdue.active[0].remaining_range(started_at + Duration::seconds(30)), TaskEstimateRange {
        lower_seconds: Some(0),
        upper_seconds: Some(0),
    });

    let completed_at = started_at + Duration::seconds(40);
    let completion = runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Complete, "task")],
            completed_at,
        )
        .await
        .expect("complete task");
    assert_eq!(completion.changed_tasks.len(), 1);
    assert_eq!(completion.changed_tasks[0].actual_elapsed_seconds, Some(40));
    assert_eq!(completion.changed_tasks[0].original_range(), TaskEstimateRange {
        lower_seconds: Some(10),
        upper_seconds: Some(20),
    });

    let repeated = runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Complete, "task")],
            completed_at + Duration::seconds(10),
        )
        .await
        .expect("idempotent completion");
    assert_eq!(repeated.sequence, completion.sequence);
    assert!(repeated.changed_tasks.is_empty());

    let snapshot = runtime
        .read_task_estimate_snapshot(root, completed_at + Duration::seconds(10), None, None)
        .await
        .expect("history snapshot");
    assert!(snapshot.active.is_empty());
    assert_eq!(snapshot.history.len(), 1);
    assert_eq!(snapshot.history[0].terminal_at, Some(completed_at));
    assert_eq!(snapshot.overall.remaining_upper_seconds, Some(0));
    runtime.close().await;
}

#[tokio::test]
async fn stale_unrelated_work_keeps_update_aggregate_unknown() {
    let (runtime, root) = runtime().await;
    let started_at = at(1_700_000_000);
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[create("old", "Old task", Some((10, 20)))],
            started_at,
        )
        .await
        .expect("create old task");
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Start, "old")],
            started_at,
        )
        .await
        .expect("start old task");

    let update = runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[create("new", "New task", Some((1, 2)))],
            started_at + Duration::seconds(15 * 60 + 1),
        )
        .await
        .expect("create new task");
    assert_eq!(update.overall, TaskEstimateOverall::unknown("stale task update"));
    runtime.close().await;
}

#[tokio::test]
async fn repeated_start_does_not_rewrite_original_baseline() {
    let (runtime, root) = runtime().await;
    let now = at(1_700_000_000);
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[create("task", "Task", None)],
            now,
        )
        .await
        .expect("create task");
    let first_start = TaskEstimateMutation {
        action: TaskEstimateAction::Start,
        task_id: Some("task".to_string()),
        title: None,
        parent_task_id: None,
        depends_on_task_ids: None,
        estimate: Some(TaskEstimateRange {
            lower_seconds: Some(10),
            upper_seconds: Some(20),
        }),
        reason: Some("initial estimate".to_string()),
    };
    runtime
        .apply_task_estimate_mutations(root, root, &[first_start], now)
        .await
        .expect("start task");
    let repeated_start = TaskEstimateMutation {
        action: TaskEstimateAction::Start,
        task_id: Some("task".to_string()),
        title: None,
        parent_task_id: None,
        depends_on_task_ids: None,
        estimate: Some(TaskEstimateRange {
            lower_seconds: Some(30),
            upper_seconds: Some(40),
        }),
        reason: Some("late estimate".to_string()),
    };
    let update = runtime
        .apply_task_estimate_mutations(root, root, &[repeated_start], now + Duration::seconds(1))
        .await
        .expect("repeat start task");
    assert_eq!(
        update.changed_tasks[0].original_range(),
        TaskEstimateRange {
            lower_seconds: Some(10),
            upper_seconds: Some(20),
        }
    );
    runtime.close().await;
}

#[tokio::test]
async fn grouping_parent_is_not_double_counted_and_requires_explicit_completion() {
    let (runtime, root) = runtime().await;
    let now = at(1_700_000_000);
    let mut child = create("child", "Child", Some((5, 5)));
    child.parent_task_id = Some("group".to_string());
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[
                create("group", "Group", Some((100, 100))),
                child,
                create("independent", "Independent", Some((8, 8))),
                transition(TaskEstimateAction::Start, "group"),
                transition(TaskEstimateAction::Start, "child"),
                transition(TaskEstimateAction::Start, "independent"),
            ],
            now,
        )
        .await
        .expect("create and start grouped tasks");

    let snapshot = runtime
        .read_task_estimate_snapshot(root, now, None, None)
        .await
        .expect("group snapshot");
    assert_eq!(
        (
            snapshot.overall.remaining_lower_seconds,
            snapshot.overall.remaining_upper_seconds,
            snapshot.overall.unknown_reason,
        ),
        (Some(8), Some(8), None),
    );

    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[
                transition(TaskEstimateAction::Complete, "child"),
                transition(TaskEstimateAction::Complete, "independent"),
            ],
            now + Duration::seconds(8),
        )
        .await
        .expect("complete children");
    let awaiting_group = runtime
        .read_task_estimate_snapshot(root, now + Duration::seconds(8), None, None)
        .await
        .expect("awaiting group snapshot");
    assert_eq!(
        awaiting_group.overall,
        TaskEstimateOverall::unknown("task group awaits explicit completion")
    );

    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Complete, "group")],
            now + Duration::seconds(8),
        )
        .await
        .expect("complete group");
    let complete = runtime
        .read_task_estimate_snapshot(root, now + Duration::seconds(8), None, None)
        .await
        .expect("complete snapshot");
    assert_eq!(complete.overall.remaining_upper_seconds, Some(0));
    runtime.close().await;
}

#[tokio::test]
async fn grouping_dependencies_are_serialized_before_parallel_children() {
    let (runtime, root) = runtime().await;
    let now = at(1_700_000_000);
    let mut group = create("group", "Group", Some((100, 100)));
    group.depends_on_task_ids = Some(vec!["dependency".to_string()]);
    let mut child = create("child", "Child", Some((5, 5)));
    child.parent_task_id = Some("group".to_string());
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[
                create("dependency", "Dependency", Some((7, 7))),
                group,
                child,
                transition(TaskEstimateAction::Start, "group"),
                transition(TaskEstimateAction::Start, "child"),
                transition(TaskEstimateAction::Start, "dependency"),
            ],
            now,
        )
        .await
        .expect("create and start grouped dependency tasks");

    let snapshot = runtime
        .read_task_estimate_snapshot(root, now, None, None)
        .await
        .expect("group dependency snapshot");
    assert_eq!(
        (
            snapshot.overall.remaining_lower_seconds,
            snapshot.overall.remaining_upper_seconds,
            snapshot.overall.unknown_reason,
        ),
        (Some(12), Some(12), None),
    );
    runtime.close().await;
}

#[tokio::test]
async fn grouping_dependency_paths_do_not_double_count_shared_transitive_work() {
    let (runtime, root) = runtime().await;
    let now = at(1_700_000_000);
    let mut first = create("first", "First", Some((5, 5)));
    first.depends_on_task_ids = Some(vec!["shared".to_string()]);
    let mut second = create("second", "Second", Some((7, 7)));
    second.depends_on_task_ids = Some(vec!["shared".to_string()]);
    let mut group = create("group", "Group", Some((100, 100)));
    group.depends_on_task_ids = Some(vec!["first".to_string(), "second".to_string()]);
    let mut child = create("child", "Child", Some((3, 3)));
    child.parent_task_id = Some("group".to_string());
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[
                create("shared", "Shared", Some((2, 2))),
                first,
                second,
                group,
                child,
                transition(TaskEstimateAction::Start, "shared"),
                transition(TaskEstimateAction::Start, "first"),
                transition(TaskEstimateAction::Start, "second"),
                transition(TaskEstimateAction::Start, "group"),
                transition(TaskEstimateAction::Start, "child"),
            ],
            now,
        )
        .await
        .expect("create and start shared grouping dependencies");
    let snapshot = runtime
        .read_task_estimate_snapshot(root, now, None, None)
        .await
        .expect("shared grouping dependency snapshot");
    assert_eq!(
        (
            snapshot.overall.remaining_lower_seconds,
            snapshot.overall.remaining_upper_seconds,
            snapshot.overall.unknown_reason,
        ),
        (Some(12), Some(12), None),
    );
    runtime.close().await;
}

#[tokio::test]
async fn grouping_dependency_on_descendant_is_unknown_instead_of_silent() {
    let (runtime, root) = runtime().await;
    let now = at(1_700_000_000);
    let mut child = create("child", "Child", Some((5, 5)));
    child.parent_task_id = Some("group".to_string());
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[
                create("group", "Group", Some((1, 1))),
                child,
                transition(TaskEstimateAction::Start, "group"),
                transition(TaskEstimateAction::Start, "child"),
                TaskEstimateMutation {
                    action: TaskEstimateAction::Revise,
                    task_id: Some("group".to_string()),
                    title: None,
                    parent_task_id: None,
                    depends_on_task_ids: Some(vec!["child".to_string()]),
                    estimate: Some(TaskEstimateRange {
                        lower_seconds: Some(1),
                        upper_seconds: Some(1),
                    }),
                    reason: Some("invalid grouping dependency".to_string()),
                },
            ],
            now,
        )
        .await
        .expect("grouping dependency is stored for explicit unknown aggregate");

    let snapshot = runtime
        .read_task_estimate_snapshot(root, now, None, None)
        .await
        .expect("grouping cycle snapshot");
    assert_eq!(
        snapshot.overall,
        TaskEstimateOverall::unknown("task grouping and dependency graphs contain a cycle")
    );
    runtime.close().await;
}

#[tokio::test]
async fn revisions_are_bounded_and_history_is_cursor_paginated() {
    let (runtime, root) = runtime().await;
    let now = at(1_700_000_000);
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[create("task", "Task", Some((1, 2)))],
            now,
        )
        .await
        .expect("create task");
    for seconds in 3..36 {
        runtime
            .apply_task_estimate_mutations(
                root,
                root,
                &[TaskEstimateMutation {
                    action: TaskEstimateAction::Revise,
                    task_id: Some("task".to_string()),
                    title: None,
                    parent_task_id: None,
                    depends_on_task_ids: None,
                    estimate: Some(TaskEstimateRange {
                        lower_seconds: Some(seconds),
                        upper_seconds: Some(seconds + 1),
                    }),
                    reason: Some("scope update".to_string()),
                }],
                now,
            )
            .await
            .expect("revise task");
    }
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Complete, "task")],
            now,
        )
        .await
        .expect("complete task");
    let snapshot = runtime
        .read_task_estimate_snapshot(root, now, None, Some(1))
        .await
        .expect("history page");
    assert_eq!(snapshot.history.len(), 1);
    assert!(snapshot.next_cursor.is_none());
    assert_eq!(snapshot.history[0].revisions.len(), 32);
    runtime.close().await;
}

#[tokio::test]
async fn task_ids_are_namespaced_by_root_session() {
    let (runtime, root_one) = runtime().await;
    let root_two = ThreadId::new();
    let now = at(1_700_000_000);
    runtime
        .apply_task_estimate_mutations(
            root_one,
            root_one,
            &[create("shared", "First root", Some((1, 1)))],
            now,
        )
        .await
        .expect("first root task");
    runtime
        .apply_task_estimate_mutations(
            root_two,
            root_two,
            &[create("shared", "Second root", Some((2, 2)))],
            now,
        )
        .await
        .expect("second root task with same id");

    let first = runtime
        .read_task_estimate_snapshot(root_one, now, None, None)
        .await
        .expect("first root snapshot");
    let second = runtime
        .read_task_estimate_snapshot(root_two, now, None, None)
        .await
        .expect("second root snapshot");
    assert_eq!(first.active[0].title, "First root");
    assert_eq!(second.active[0].title, "Second root");
    runtime.close().await;
}

#[tokio::test]
async fn worker_updates_are_root_scoped_and_owner_checked() {
    let (runtime, root) = runtime().await;
    let child = ThreadId::new();
    let sibling = ThreadId::new();
    runtime
        .upsert_thread_spawn_edge(root, child, DirectionalThreadSpawnEdgeStatus::Open)
        .await
        .expect("spawn edge");
    assert_eq!(runtime.root_thread_id(child).await.expect("child root"), root);

    let now = at(1_700_000_000);
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[create("root-task", "Root task", Some((1, 1)))],
            now,
        )
        .await
        .expect("root task");
    runtime
        .apply_task_estimate_mutations(
            root,
            child,
            &[create("child-task", "Child task", Some((2, 2)))],
            now,
        )
        .await
        .expect("child task");

    let child_cannot_edit_root = runtime
        .apply_task_estimate_mutations(
            root,
            child,
            &[transition(TaskEstimateAction::Complete, "root-task")],
            now,
        )
        .await;
    assert!(child_cannot_edit_root.is_err());

    let sibling_cannot_join_root = runtime
        .apply_task_estimate_mutations(
            root,
            sibling,
            &[transition(TaskEstimateAction::Complete, "child-task")],
            now,
        )
        .await;
    assert!(sibling_cannot_join_root.is_err());

    runtime
        .set_thread_spawn_edge_status(child, DirectionalThreadSpawnEdgeStatus::Closed)
        .await
        .expect("close spawn edge");
    assert_eq!(runtime.root_thread_id(child).await.expect("closed child root"), root);
    runtime.close().await;
}
