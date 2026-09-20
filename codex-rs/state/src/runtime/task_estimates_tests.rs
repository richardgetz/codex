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

#[tokio::test]
async fn all_session_eta_list_cursor_returns_every_task_across_pages() {
    let (runtime, root) = runtime().await;
    let root_metadata = crate::runtime::test_support::test_thread_metadata(
        runtime.sqlite.home(),
        root,
        runtime.sqlite.home().to_path_buf(),
    );
    runtime
        .upsert_thread(&root_metadata)
        .await
        .expect("persist root metadata");
    let now = at(1_700_000_000);
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[create("parent", "Parent", Some((10, 20)))],
            now,
        )
        .await
        .expect("create parent");
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[TaskEstimateMutation {
                action: TaskEstimateAction::Create,
                task_id: Some("child".to_string()),
                title: Some("Child".to_string()),
                parent_task_id: Some("parent".to_string()),
                depends_on_task_ids: None,
                estimate: Some(TaskEstimateRange {
                    lower_seconds: Some(3),
                    upper_seconds: Some(5),
                }),
                reason: None,
                owner_thread_id: None,
            }],
            now + Duration::seconds(1),
        )
        .await
        .expect("create child");

    let top_level = runtime
        .list_task_estimate_sessions(None, Some(1), false)
        .await
        .expect("list top-level ETA tasks");
    assert_eq!(top_level.rows.len(), 1);
    assert_eq!(top_level.rows[0].task_id, "parent");
    assert_eq!(top_level.rows[0].nested_task_count, 1);
    assert_eq!(top_level.rows[0].active_nested_task_count, 1);
    assert_eq!(top_level.rows[0].nested_lower_seconds, Some(3));
    assert_eq!(top_level.rows[0].nested_upper_seconds, Some(5));
    assert!(top_level.next_cursor.is_none());

    let expanded = runtime
        .list_task_estimate_sessions(None, Some(1), true)
        .await
        .expect("list nested ETA tasks");
    assert_eq!(expanded.rows.len(), 1);
    assert_eq!(expanded.rows[0].task_id, "child");
    let cursor = expanded.next_cursor.as_deref();
    assert!(cursor.is_some());
    let second_page = runtime
        .list_task_estimate_sessions(cursor, Some(1), true)
        .await
        .expect("list second ETA page");
    assert_eq!(second_page.rows.len(), 1);
    assert_eq!(second_page.rows[0].task_id, "parent");
    assert!(second_page.next_cursor.is_none());
    runtime.close().await;
}

#[tokio::test]
async fn all_session_eta_list_projects_ranges_with_root_freshness() {
    let (runtime, root) = runtime().await;
    let root_metadata = crate::runtime::test_support::test_thread_metadata(
        runtime.sqlite.home(),
        root,
        runtime.sqlite.home().to_path_buf(),
    );
    runtime
        .upsert_thread(&root_metadata)
        .await
        .expect("persist root metadata");
    runtime
        .initialize_eta_freshness_minimum_seconds(root, 60)
        .await
        .expect("persist root freshness policy");
    let started_at = at(1_700_000_000);
    let mut child = create("child", "Child", Some((20, 40)));
    child.parent_task_id = Some("parent".to_string());
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[
                create("parent", "Parent", Some((100, 100))),
                child,
                transition(TaskEstimateAction::Start, "parent"),
                transition(TaskEstimateAction::Start, "child"),
            ],
            started_at,
        )
        .await
        .expect("create and start ETA tasks");

    let fresh = runtime
        .list_task_estimate_sessions_at(None, Some(10), true, started_at + Duration::seconds(30))
        .await
        .expect("read fresh projected ETA rows");
    let fresh_child = fresh
        .rows
        .iter()
        .find(|row| row.task_id == "child")
        .expect("fresh child row");
    assert_eq!(
        (
            fresh_child.current_lower_seconds,
            fresh_child.current_upper_seconds,
            fresh_child.is_stale,
        ),
        (Some(0), Some(10), false)
    );
    let fresh_parent = fresh
        .rows
        .iter()
        .find(|row| row.task_id == "parent")
        .expect("fresh parent row");
    assert_eq!(
        (
            fresh_parent.nested_lower_seconds,
            fresh_parent.nested_upper_seconds,
        ),
        (Some(0), Some(10))
    );

    let stale = runtime
        .list_task_estimate_sessions_at(None, Some(10), true, started_at + Duration::seconds(61))
        .await
        .expect("read stale projected ETA rows");
    let stale_child = stale
        .rows
        .iter()
        .find(|row| row.task_id == "child")
        .expect("stale child row");
    assert_eq!(
        (
            stale_child.current_lower_seconds,
            stale_child.current_upper_seconds,
            stale_child.is_stale,
        ),
        (Some(20), Some(40), true)
    );
    let stale_parent = stale
        .rows
        .iter()
        .find(|row| row.task_id == "parent")
        .expect("stale parent row");
    assert_eq!(
        (
            stale_parent.nested_lower_seconds,
            stale_parent.nested_upper_seconds,
        ),
        (Some(20), Some(40))
    );

    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[
                transition(TaskEstimateAction::Complete, "child"),
                transition(TaskEstimateAction::Complete, "parent"),
            ],
            started_at + Duration::seconds(62),
        )
        .await
        .expect("complete ETA tasks");
    let terminal = runtime
        .list_task_estimate_sessions_at(None, Some(10), true, started_at + Duration::seconds(120))
        .await
        .expect("read terminal projected ETA rows");
    for task_id in ["child", "parent"] {
        let row = terminal
            .rows
            .iter()
            .find(|row| row.task_id == task_id)
            .expect("terminal ETA row");
        assert!(!row.is_stale);
        let expected_range = if task_id == "child" {
            (Some(20), Some(40))
        } else {
            (Some(100), Some(100))
        };
        assert_eq!(
            (row.current_lower_seconds, row.current_upper_seconds),
            expected_range
        );
    }
    runtime.close().await;
}

#[tokio::test]
async fn eta_history_pruning_removes_old_terminal_rows_only() {
    let (runtime, root) = runtime().await;
    let now = at(1_700_000_000);
    runtime
        .apply_task_estimate_mutations(root, root, &[create("old", "Old", Some((1, 2)))], now)
        .await
        .expect("create old task");
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Start, "old")],
            now,
        )
        .await
        .expect("start old task");
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Complete, "old")],
            now + Duration::seconds(1),
        )
        .await
        .expect("complete old task");
    runtime
        .apply_task_estimate_mutations(root, root, &[create("active", "Active", Some((1, 2)))], now)
        .await
        .expect("create active task");
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Start, "active")],
            now,
        )
        .await
        .expect("start active task");

    let removed = runtime
        .prune_task_estimate_history(30, now + Duration::days(31))
        .await
        .expect("prune ETA history");
    assert_eq!(removed, 1);
    let snapshot = runtime
        .read_task_estimate_snapshot(root, now + Duration::days(31), None, None)
        .await
        .expect("read retained ETA tasks");
    assert_eq!(
        snapshot
            .active
            .iter()
            .map(|task| task.task_id.as_str())
            .collect::<Vec<_>>(),
        vec!["active"]
    );
    assert!(snapshot.history.is_empty());
    runtime.close().await;
}

#[tokio::test]
async fn eta_freshness_minimum_is_root_owned_and_persistent() {
    let (runtime, root) = runtime().await;
    assert_eq!(
        runtime
            .eta_freshness_minimum_seconds(root)
            .await
            .expect("read missing ETA root policy"),
        None
    );
    assert_eq!(
        runtime
            .initialize_eta_freshness_minimum_seconds(root, 45 * 60)
            .await
            .expect("initialize ETA root policy"),
        45 * 60
    );
    assert_eq!(
        runtime
            .initialize_eta_freshness_minimum_seconds(root, 30 * 60)
            .await
            .expect("preserve initialized ETA root policy"),
        45 * 60
    );
    assert_eq!(
        runtime
            .eta_freshness_minimum_seconds(root)
            .await
            .expect("read ETA root policy"),
        Some(45 * 60)
    );
    runtime
        .set_eta_freshness_minimum_seconds(root, 30 * 60)
        .await
        .expect("update ETA root policy");
    assert_eq!(
        runtime
            .eta_freshness_minimum_seconds(root)
            .await
            .expect("read updated ETA root policy"),
        Some(30 * 60)
    );
    runtime.close().await;
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
        owner_thread_id: None,
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
        owner_thread_id: None,
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
    assert_eq!(
        overdue.overall,
        TaskEstimateOverall::unknown("task `task` estimate has elapsed")
    );
    assert_eq!(overdue.active.len(), 1);
    assert_eq!(
        overdue.active[0].remaining_range(started_at + Duration::seconds(30)),
        TaskEstimateRange {
            lower_seconds: Some(0),
            upper_seconds: Some(0),
        }
    );

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
    assert_eq!(completion.changed_tasks[0].started_at, Some(started_at));
    assert_eq!(completion.changed_tasks[0].terminal_at, Some(completed_at));
    assert_eq!(
        completion.changed_tasks[0].original_range(),
        TaskEstimateRange {
            lower_seconds: Some(10),
            upper_seconds: Some(20),
        }
    );

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
    assert_eq!(snapshot.history[0].started_at, Some(started_at));
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
    assert_eq!(
        update.overall,
        TaskEstimateOverall::unknown("stale task update")
    );
    runtime.close().await;
}

#[tokio::test]
async fn long_estimate_extends_freshness_to_one_quarter() {
    let (runtime, root) = runtime().await;
    let started_at = at(1_700_000_000);
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[create("long", "Long task", Some((60, 2 * 60 * 60)))],
            started_at,
        )
        .await
        .expect("create long task");
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Start, "long")],
            started_at,
        )
        .await
        .expect("start long task");

    let before_quarter = runtime
        .read_task_estimate_snapshot(root, started_at + Duration::minutes(29), None, None)
        .await
        .expect("fresh long snapshot");
    assert_eq!(before_quarter.overall.unknown_reason, None);

    let at_quarter = runtime
        .read_task_estimate_snapshot(root, started_at + Duration::minutes(30), None, None)
        .await
        .expect("stale long snapshot");
    assert_eq!(
        at_quarter.overall,
        TaskEstimateOverall::unknown("stale task update")
    );
    runtime.close().await;
}

#[tokio::test]
async fn repeated_start_does_not_rewrite_original_baseline() {
    let (runtime, root) = runtime().await;
    let now = at(1_700_000_000);
    runtime
        .apply_task_estimate_mutations(root, root, &[create("task", "Task", None)], now)
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
        owner_thread_id: None,
    };
    let first_update = runtime
        .apply_task_estimate_mutations(root, root, &[first_start], now)
        .await
        .expect("start task");
    assert_eq!(first_update.changed_tasks[0].started_at, Some(now));
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
        owner_thread_id: None,
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
    assert_eq!(update.changed_tasks[0].started_at, Some(now));
    runtime.close().await;
}

#[tokio::test]
async fn grouping_parent_is_not_double_counted_after_children_finish() {
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
        (
            awaiting_group.overall.remaining_lower_seconds,
            awaiting_group.overall.remaining_upper_seconds,
            awaiting_group.overall.unknown_reason,
        ),
        (Some(92), Some(92), None),
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
async fn bounded_parallel_tasks_keep_overall_after_nested_child_finishes() {
    let (runtime, root) = runtime().await;
    let now = at(1_700_000_000);
    let mut child = create("nested", "Nested worker", Some((5, 9)));
    child.parent_task_id = Some("group".to_string());
    let mut operations = vec![create("group", "Team details", Some((20, 40))), child];
    for index in 0..6 {
        operations.push(create(
            &format!("independent-{index}"),
            "Independent work",
            Some((5 + index, 10 + index)),
        ));
    }
    operations.extend([
        transition(TaskEstimateAction::Start, "group"),
        transition(TaskEstimateAction::Start, "nested"),
    ]);
    for index in 0..6 {
        operations.push(transition(
            TaskEstimateAction::Start,
            &format!("independent-{index}"),
        ));
    }
    runtime
        .apply_task_estimate_mutations(root, root, &operations, now)
        .await
        .expect("create and start bounded parallel tasks");

    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Complete, "nested")],
            now + Duration::seconds(1),
        )
        .await
        .expect("finish nested worker while group remains active");
    let snapshot = runtime
        .read_task_estimate_snapshot(root, now + Duration::seconds(1), None, None)
        .await
        .expect("read bounded parallel aggregate");
    assert_eq!(
        (
            snapshot.overall.remaining_lower_seconds,
            snapshot.overall.remaining_upper_seconds,
            snapshot.overall.unknown_reason,
        ),
        (Some(19), Some(39), None),
    );
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
                    owner_thread_id: None,
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
        .apply_task_estimate_mutations(root, root, &[create("task", "Task", Some((1, 2)))], now)
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
                    owner_thread_id: None,
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
    assert_eq!(
        runtime.root_thread_id(child).await.expect("child root"),
        root
    );

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
    assert_eq!(
        runtime
            .root_thread_id(child)
            .await
            .expect("closed child root"),
        root
    );
    runtime.close().await;
}

#[tokio::test]
async fn root_can_assign_persisted_worker_and_reassign_without_resetting_estimate() {
    let (runtime, root) = runtime().await;
    let worker = ThreadId::new();
    let unrelated = ThreadId::new();
    runtime
        .upsert_thread_spawn_edge(root, worker, DirectionalThreadSpawnEdgeStatus::Open)
        .await
        .expect("spawn edge");
    let now = at(1_700_000_000);
    let mut create_task = create("owned", "Owned task", Some((10, 20)));
    create_task.owner_thread_id = Some(worker);
    runtime
        .apply_task_estimate_mutations(root, root, &[create_task], now)
        .await
        .expect("root assigns worker");
    let snapshot = runtime
        .read_task_estimate_snapshot(root, now, None, None)
        .await
        .expect("snapshot");
    assert_eq!(snapshot.active[0].owner_thread_id, worker);

    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[transition(TaskEstimateAction::Start, "owned")],
            now,
        )
        .await
        .expect("start assigned task");
    let mut reassign = transition(TaskEstimateAction::Revise, "owned");
    reassign.owner_thread_id = Some(root);
    reassign.reason = Some("Lead resumed ownership".to_string());
    runtime
        .apply_task_estimate_mutations(root, root, &[reassign], now + Duration::seconds(60))
        .await
        .expect("root reassigns task");
    let snapshot = runtime
        .read_task_estimate_snapshot(root, now + Duration::seconds(60), None, None)
        .await
        .expect("snapshot after reassignment");
    assert_eq!(snapshot.active[0].owner_thread_id, root);
    assert_eq!(snapshot.active[0].updated_at, now);
    assert_eq!(snapshot.active[0].started_at, Some(now));
    assert_eq!(
        snapshot.active[0].current_range(),
        TaskEstimateRange {
            lower_seconds: Some(10),
            upper_seconds: Some(20),
        }
    );

    runtime
        .set_thread_spawn_edge_status(worker, DirectionalThreadSpawnEdgeStatus::Closed)
        .await
        .expect("close worker edge");
    let mut closed_owner = transition(TaskEstimateAction::Revise, "owned");
    closed_owner.owner_thread_id = Some(worker);
    let error = runtime
        .apply_task_estimate_mutations(root, root, &[closed_owner], now)
        .await
        .expect_err("closed owner is rejected");
    assert!(error.to_string().contains("not part of the requested root"));

    let mut invalid = transition(TaskEstimateAction::Revise, "owned");
    invalid.owner_thread_id = Some(unrelated);
    let error = runtime
        .apply_task_estimate_mutations(root, root, &[invalid], now)
        .await
        .expect_err("unrelated owner is rejected");
    assert!(error.to_string().contains("not part of the requested root"));
    runtime.close().await;
}

#[tokio::test]
async fn revision_range_is_measured_from_revision_time_and_refreshes_group_aggregate() {
    let (runtime, root) = runtime().await;
    let now = at(1_700_000_000);
    let mut child = create("child", "Child", Some((10, 30)));
    child.parent_task_id = Some("group".to_string());
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[create("group", "Group", Some((1, 1))), child],
            now,
        )
        .await
        .expect("create group and child");
    runtime
        .apply_task_estimate_mutations(
            root,
            root,
            &[
                transition(TaskEstimateAction::Start, "group"),
                transition(TaskEstimateAction::Start, "child"),
            ],
            now,
        )
        .await
        .expect("start group and child");

    let mut revision = transition(TaskEstimateAction::Revise, "child");
    revision.estimate = Some(TaskEstimateRange {
        lower_seconds: Some(4),
        upper_seconds: Some(8),
    });
    revision.reason = Some("remaining work measured from reminder".to_string());
    let revision_at = now + Duration::seconds(10);
    let update = runtime
        .apply_task_estimate_mutations(root, root, &[revision], revision_at)
        .await
        .expect("revise child");
    assert_eq!(update.overall.remaining_lower_seconds, Some(4));
    assert_eq!(update.overall.remaining_upper_seconds, Some(8));

    let snapshot = runtime
        .read_task_estimate_snapshot(root, revision_at + Duration::seconds(2), None, None)
        .await
        .expect("snapshot after revision");
    let child = snapshot
        .active
        .iter()
        .find(|task| task.task_id == "child")
        .expect("child snapshot");
    assert_eq!(
        child.remaining_range(revision_at + Duration::seconds(2)),
        TaskEstimateRange {
            lower_seconds: Some(2),
            upper_seconds: Some(6),
        }
    );
    assert_eq!(snapshot.overall.remaining_lower_seconds, Some(2));
    assert_eq!(snapshot.overall.remaining_upper_seconds, Some(6));
    runtime.close().await;
}
