use super::*;
use codex_app_server_protocol::ThreadActivityUpdatedNotification;
use codex_app_server_protocol::ThreadActivityWaitReason;
use pretty_assertions::assert_eq;

fn notification(
    thread_id: ThreadId,
    root_thread_id: ThreadId,
    activity: ThreadActivity,
    pause_state: ThreadPauseState,
    in_flight_operations: u32,
) -> ThreadActivityUpdatedNotification {
    ThreadActivityUpdatedNotification {
        thread_id: thread_id.to_string(),
        root_thread_id: root_thread_id.to_string(),
        activity,
        pause_state,
        wait_reason: None,
        in_flight_operations,
    }
}

#[test]
fn projection_counts_nested_workers_and_ignores_other_roots() {
    let root = ThreadId::new();
    let worker = ThreadId::new();
    let nested_worker = ThreadId::new();
    let unrelated_root = ThreadId::new();
    let unrelated_worker = ThreadId::new();
    let mut projection = TeamActivityProjection::default();

    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    projection.observe_thread_parent(worker, Some(root));
    projection.observe_thread_parent(nested_worker, Some(worker));
    projection.observe(&notification(
        worker,
        root,
        ThreadActivity::Working,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 1,
    ));
    projection.observe(&notification(
        nested_worker,
        root,
        ThreadActivity::Waiting,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    projection.observe(&notification(
        unrelated_root,
        unrelated_root,
        ThreadActivity::Idle,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    projection.observe(&notification(
        unrelated_worker,
        unrelated_root,
        ThreadActivity::Working,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 1,
    ));

    let status = projection
        .status_for_root(root, None)
        .expect("active worker projection");
    assert_eq!(
        status,
        TeamActivityStatus {
            lead: TeamRoleActivity::Idle,
            workers_working: 1,
            workers_waiting: 1,
            direct_workers: 1,
            subagents: 1,
            worker_max_concurrent: None,
            pause_state: UiPauseState::Running,
            in_flight_operations: 1,
        }
    );
    assert_eq!(status.header(), "Lead: idle · Team: 1 working, 1 waiting");
    assert!(status.is_animated());

    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Paused,
        /*in_flight_operations*/ 0,
    ));
    projection.observe(&notification(
        worker,
        root,
        ThreadActivity::Working,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    let status = projection
        .status_for_root(root, None)
        .expect("root activity");
    assert_eq!(status.pause_state, UiPauseState::Paused);
    assert_eq!(
        status.header(),
        "Paused · Lead + 2 workers · /continue to resume"
    );
}

#[test]
fn unknown_activity_waits_for_parent_metadata_admission() {
    let root = ThreadId::new();
    let worker = ThreadId::new();
    let nested_worker = ThreadId::new();
    let mut projection = TeamActivityProjection::default();

    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    projection.observe(&notification(
        worker,
        root,
        ThreadActivity::Working,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    projection.observe(&notification(
        nested_worker,
        root,
        ThreadActivity::Waiting,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));

    assert!(!projection.entries.contains_key(&worker));
    assert!(!projection.entries.contains_key(&nested_worker));

    projection.observe_thread_parent(worker, Some(root));
    projection.observe_thread_parent(nested_worker, Some(worker));
    projection.observe(&notification(
        worker,
        root,
        ThreadActivity::Working,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    projection.observe(&notification(
        nested_worker,
        root,
        ThreadActivity::Waiting,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    let after_metadata = projection
        .status_for_root(root, None)
        .expect("activity after metadata");
    assert_eq!(after_metadata.workers_working, 1);
    assert_eq!(after_metadata.workers_waiting, 1);
    assert_eq!(after_metadata.direct_workers, 1);
    assert_eq!(after_metadata.subagents, 1);
}

#[test]
fn projection_renders_pause_transition_and_clears_when_idle() {
    let root = ThreadId::new();
    let worker = ThreadId::new();
    let mut projection = TeamActivityProjection::default();
    projection.observe_thread_parent(worker, Some(root));

    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Working,
        ThreadPauseState::Pausing,
        /*in_flight_operations*/ 2,
    ));
    projection.observe(&notification(
        worker,
        root,
        ThreadActivity::Working,
        ThreadPauseState::Pausing,
        /*in_flight_operations*/ 1,
    ));
    let pausing = projection
        .status_for_root(root, None)
        .expect("pausing projection");
    assert_eq!(
        pausing.header(),
        "Pausing — finishing 3 in-flight operations"
    );
    assert!(pausing.is_animated());

    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Paused,
        /*in_flight_operations*/ 0,
    ));
    let still_pausing = projection
        .status_for_root(root, None)
        .expect("draining projection");
    assert_eq!(
        still_pausing.header(),
        "Pausing — finishing 1 in-flight operation"
    );
    assert!(still_pausing.is_animated());

    projection.observe(&notification(
        worker,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Paused,
        /*in_flight_operations*/ 0,
    ));
    let paused = projection
        .status_for_root(root, None)
        .expect("paused projection");
    assert_eq!(paused.header(), "Paused — /continue to resume");
    assert!(!paused.is_animated());

    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    assert_eq!(projection.status_for_root(root, None), None);
}

#[test]
fn ordinary_waiting_grace_is_display_only_and_does_not_extend() {
    let root = ThreadId::new();
    let worker = ThreadId::new();
    let start = Instant::now();
    let mut projection = TeamActivityProjection::default();
    projection.observe_thread_parent(worker, Some(root));
    projection.observe_at(
        &notification(
            root,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start,
    );
    projection.observe_at(
        &notification(
            worker,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 1,
        ),
        start,
    );
    projection.observe_at(
        &notification(
            worker,
            root,
            ThreadActivity::Waiting,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start + Duration::from_secs(1),
    );

    let grace = projection
        .status_for_root_at(root, None, start + Duration::from_secs(29))
        .expect("grace projection");
    assert_eq!(grace.workers_working, 1);
    assert_eq!(grace.workers_waiting, 0);

    // A repeated waiting event preserves the original deadline rather than extending it.
    projection.observe_at(
        &notification(
            worker,
            root,
            ThreadActivity::Waiting,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start + Duration::from_secs(20),
    );
    let expired = projection
        .status_for_root_at(root, None, start + Duration::from_secs(31))
        .expect("expired projection");
    assert_eq!(expired.workers_working, 0);
    assert_eq!(expired.workers_waiting, 1);

    // Waiting for user action bypasses the grace period immediately.
    projection.observe_at(
        &notification(
            worker,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 1,
        ),
        start + Duration::from_secs(32),
    );
    projection.observe_at(
        &ThreadActivityUpdatedNotification {
            wait_reason: Some(ThreadActivityWaitReason::Approval),
            ..notification(
                worker,
                root,
                ThreadActivity::Waiting,
                ThreadPauseState::Running,
                /*in_flight_operations*/ 0,
            )
        },
        start + Duration::from_secs(33),
    );
    let actionable = projection
        .status_for_root_at(root, None, start + Duration::from_secs(33))
        .expect("actionable projection");
    assert_eq!(actionable.workers_working, 0);
    assert_eq!(actionable.workers_waiting, 1);
}

#[test]
fn first_seen_and_paused_waiting_states_are_immediate() {
    let root = ThreadId::new();
    let worker = ThreadId::new();
    let start = Instant::now();
    let mut projection = TeamActivityProjection::default();
    projection.observe_thread_parent(worker, Some(root));
    projection.observe_at(
        &notification(
            root,
            root,
            ThreadActivity::Idle,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start,
    );
    projection.observe_at(
        &notification(
            worker,
            root,
            ThreadActivity::Waiting,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start,
    );
    let first_seen = projection
        .status_for_root_at(root, None, start)
        .expect("first-seen waiting projection");
    assert_eq!(first_seen.workers_working, 0);
    assert_eq!(first_seen.workers_waiting, 1);

    projection.observe_at(
        &notification(
            worker,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start + Duration::from_secs(1),
    );
    projection.observe_at(
        &notification(
            worker,
            root,
            ThreadActivity::Waiting,
            ThreadPauseState::Paused,
            /*in_flight_operations*/ 0,
        ),
        start + Duration::from_secs(2),
    );
    let paused = projection
        .status_for_root_at(root, None, start + Duration::from_secs(2))
        .expect("paused waiting projection");
    assert_eq!(paused.workers_working, 0);
    assert_eq!(paused.workers_waiting, 1);
}

#[test]
fn terminal_activity_ignores_late_updates_until_next_turn_starts() {
    let root = ThreadId::new();
    let worker = ThreadId::new();
    let start = Instant::now();
    let mut projection = TeamActivityProjection::default();
    projection.observe_thread_parent(worker, Some(root));
    projection.observe_at(
        &notification(
            root,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start,
    );
    projection.observe_at(
        &notification(
            worker,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 1,
        ),
        start,
    );

    projection.finish_thread(worker);
    projection.observe_at(
        &notification(
            worker,
            root,
            ThreadActivity::Waiting,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 1,
        ),
        start + Duration::from_secs(1),
    );
    let terminal = projection
        .status_for_root(root, None)
        .expect("root remains available after worker completion");
    assert_eq!(terminal.workers_working, 0);
    assert_eq!(terminal.workers_waiting, 0);

    projection.start_thread(worker);
    projection.observe_at(
        &notification(
            worker,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 1,
        ),
        start + Duration::from_secs(2),
    );
    let resumed = projection
        .status_for_root(root, None)
        .expect("new turn activity resumes normally");
    assert_eq!(resumed.workers_working, 1);
}

#[test]
fn terminal_tombstone_preserves_pause_updates() {
    let root = ThreadId::new();
    let start = Instant::now();
    let mut projection = TeamActivityProjection::default();
    projection.observe_at(
        &notification(
            root,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 1,
        ),
        start,
    );

    projection.finish_thread(root);
    projection.observe_at(
        &notification(
            root,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Paused,
            /*in_flight_operations*/ 4,
        ),
        start + Duration::from_secs(1),
    );
    projection.observe_at(
        &notification(
            root,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 4,
        ),
        start + Duration::from_secs(2),
    );
    let paused = projection
        .status_for_root(root, None)
        .expect("pause update remains visible after terminal activity");
    assert_eq!(paused.lead, TeamRoleActivity::Idle);
    assert_eq!(paused.pause_state, UiPauseState::Paused);
    assert_eq!(paused.in_flight_operations, 0);
}

#[test]
fn removed_tree_ignores_late_activity_until_each_turn_starts() {
    let root = ThreadId::new();
    let child = ThreadId::new();
    let start = Instant::now();
    let mut projection = TeamActivityProjection::default();
    projection.replace_thread_metadata(Some(root), [(root, None), (child, Some(root))]);
    projection.observe_at(
        &notification(
            root,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start,
    );
    projection.observe_at(
        &notification(
            child,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start,
    );

    projection.remove_thread(root);
    // A metadata refresh can still contain closed rows. The removal barrier must survive it.
    projection.replace_thread_metadata(Some(root), [(root, None), (child, Some(root))]);
    projection.observe_at(
        &notification(
            root,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 1,
        ),
        start + Duration::from_secs(1),
    );
    projection.observe_at(
        &notification(
            child,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 1,
        ),
        start + Duration::from_secs(1),
    );
    assert_eq!(projection.entries[&root].activity, ThreadActivity::Idle);
    assert_eq!(projection.entries[&child].activity, ThreadActivity::Idle);
    assert!(projection.status_for_root(root, None).is_none());

    projection.start_thread(root);
    projection.observe_at(
        &notification(
            root,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start + Duration::from_secs(2),
    );
    assert_eq!(
        projection
            .status_for_root(root, None)
            .unwrap()
            .workers_working,
        0
    );

    projection.start_thread(child);
    projection.observe_at(
        &notification(
            child,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start + Duration::from_secs(3),
    );
    assert_eq!(
        projection
            .status_for_root(root, None)
            .unwrap()
            .workers_working,
        1
    );
}

#[test]
fn uncached_removals_keep_selected_root_barriers() {
    let root = ThreadId::new();
    let child = ThreadId::new();
    let start = Instant::now();
    let mut projection = TeamActivityProjection::default();
    projection.replace_thread_metadata(Some(root), []);

    // Closing a selected root before any activity or thread metadata arrives still blocks a
    // delayed update after the overview refreshes the closed row.
    projection.remove_thread(root);
    projection.replace_thread_metadata(Some(root), [(root, None)]);
    projection.observe_at(
        &notification(
            root,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 1,
        ),
        start,
    );
    assert_eq!(projection.entries[&root].activity, ThreadActivity::Idle);

    // An uncached child is tied to the selected root for the same lifecycle barrier.
    projection.remove_thread(child);
    projection.replace_thread_metadata(Some(root), [(root, None)]);
    projection.observe_at(
        &notification(
            child,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 1,
        ),
        start + Duration::from_secs(1),
    );
    assert!(!projection.entries.contains_key(&child));

    projection.start_thread(root);
    projection.start_thread(child);
    projection.observe_thread_parent(child, Some(root));
    projection.observe_at(
        &notification(
            root,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start + Duration::from_secs(2),
    );
    projection.observe_at(
        &notification(
            child,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ),
        start + Duration::from_secs(2),
    );
    assert_eq!(
        projection
            .status_for_root(root, None)
            .unwrap()
            .workers_working,
        1
    );
}

#[test]
fn removed_worker_admission_does_not_accumulate_after_churn() {
    let root = ThreadId::new();
    let mut projection = TeamActivityProjection::default();
    projection.replace_thread_metadata(Some(root), [(root, None)]);

    for _ in 0..64 {
        let worker = ThreadId::new();
        projection.replace_thread_metadata(Some(root), [(root, None), (worker, Some(root))]);
        projection.observe(&notification(
            worker,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 0,
        ));
        projection.remove_thread(worker);
        // Closed rows are omitted from the admitted metadata on the next refresh.
        projection.replace_thread_metadata(Some(root), [(root, None)]);
        projection.observe(&notification(
            worker,
            root,
            ThreadActivity::Working,
            ThreadPauseState::Running,
            /*in_flight_operations*/ 1,
        ));
    }

    assert!(projection.entries.is_empty());
    assert!(projection.terminal_threads.is_empty());
}

#[test]
fn metadata_only_root_removal_prunes_descendants() {
    let root = ThreadId::new();
    let child = ThreadId::new();
    let grandchild = ThreadId::new();
    let mut projection = TeamActivityProjection::default();
    projection.observe_thread_parent(root, None);
    projection.observe_thread_parent(child, Some(root));
    projection.observe_thread_parent(grandchild, Some(child));

    projection.remove_thread(root);

    assert!(projection.parent_thread_ids.is_empty());
    assert!(projection.entries.is_empty());
    assert_eq!(projection.terminal_threads.len(), 3);
}

#[test]
fn replacing_selected_metadata_drops_the_previous_tree() {
    let root = ThreadId::new();
    let child = ThreadId::new();
    let other_root = ThreadId::new();
    let other_child = ThreadId::new();
    let mut projection = TeamActivityProjection::default();
    projection.replace_thread_metadata(Some(root), [(root, None), (child, Some(root))]);
    projection.observe(&notification(
        root,
        root,
        ThreadActivity::Idle,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    projection.observe(&notification(
        child,
        root,
        ThreadActivity::Working,
        ThreadPauseState::Running,
        /*in_flight_operations*/ 0,
    ));
    assert_eq!(projection.parent_thread_ids.len(), 2);

    projection.replace_thread_metadata(
        Some(other_root),
        [(other_root, None), (other_child, Some(other_root))],
    );

    assert_eq!(projection.parent_thread_ids.len(), 2);
    assert!(projection.parent_thread_ids.contains_key(&other_root));
    assert!(projection.parent_thread_ids.contains_key(&other_child));
    assert!(!projection.parent_thread_ids.contains_key(&root));
    assert!(!projection.parent_thread_ids.contains_key(&child));
    assert!(projection.entries.is_empty());
}
