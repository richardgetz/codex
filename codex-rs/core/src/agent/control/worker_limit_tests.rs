use super::*;
use crate::config::ConfigBuilder;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

fn limiter(max_concurrent: Option<usize>) -> Arc<TeamWorkerLimiter> {
    let limiter = Arc::new(TeamWorkerLimiter::default());
    limiter.initialize(max_concurrent);
    limiter
}

#[test]
fn omitted_ceiling_preserves_unbounded_admission() {
    let limiter = limiter(None);

    assert!(
        limiter
            .reserve_pending_spawn()
            .expect("unbounded limiter should not reject a spawn")
            .is_none()
    );
    assert!(
        limiter
            .reserve_active(ThreadId::new())
            .expect("unbounded limiter should not reject a turn")
            .is_none()
    );
}

#[tokio::test]
async fn only_root_direct_workers_use_the_team_admission() {
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .build()
        .await
        .expect("test config should load");
    config.team_mode = TeamMode::LeadWorker;
    let root_thread_id = ThreadId::new();
    let control =
        AgentControl::default().with_session_id(root_thread_id.into(), usize::MAX, Some(1));
    let direct_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let grandchild_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 2,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });

    assert!(control.is_direct_team_worker(&config, Some(&direct_source)));
    assert!(!control.is_direct_team_worker(&config, Some(&grandchild_source)));
    assert!(!control.is_direct_team_worker(&config, Some(&SessionSource::Cli)));
}

#[test]
fn pending_spawns_and_active_workers_share_one_ceiling() {
    let limiter = limiter(Some(2));
    let first = limiter
        .reserve_pending_spawn()
        .expect("first pending spawn should fit")
        .expect("configured limiter should return a lease");
    let second = limiter
        .reserve_active(ThreadId::new())
        .expect("first active Worker should fit")
        .expect("configured limiter should return a lease");

    let error = match limiter.reserve_pending_spawn() {
        Ok(_) => panic!("pending spawn should not exceed the shared ceiling"),
        Err(error) => error,
    };
    let CodexErrorDetails::AgentLimitReached { max_threads } = error.details() else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*max_threads, 2);

    drop(first);
    drop(second);
    assert!(
        limiter
            .reserve_pending_spawn()
            .expect("released leases should restore capacity")
            .is_some()
    );
}

#[test]
fn committed_spawn_transfers_pending_capacity_to_thread_lifecycle() {
    let limiter = limiter(Some(1));
    let thread_id = ThreadId::new();
    let mut pending = limiter
        .reserve_pending_spawn()
        .expect("pending spawn should fit")
        .expect("configured limiter should return a lease");

    pending.commit_pending_spawn(thread_id);
    let mut task = limiter
        .reserve_active(thread_id)
        .expect("the admitted thread can start its first task")
        .expect("configured limiter should return a lease");
    task.mark_task_started();
    drop(pending);

    assert!(limiter.reserve_active(ThreadId::new()).is_err());
    drop(task);
    assert!(
        limiter
            .reserve_active(ThreadId::new())
            .expect("terminal release should restore capacity")
            .is_some()
    );
}

#[test]
fn active_lease_is_released_and_same_thread_shares_an_admission() {
    let limiter = limiter(Some(1));
    let thread_id = ThreadId::new();
    let mut lease = limiter
        .reserve_active(thread_id)
        .expect("first turn should fit")
        .expect("configured limiter should return a lease");
    let duplicate = limiter
        .reserve_active(thread_id)
        .expect("same active thread should share its admission")
        .expect("configured limiter should return a lease");

    assert!(limiter.reserve_active(ThreadId::new()).is_err());

    lease.mark_task_started();
    drop(duplicate);
    assert!(limiter.reserve_active(ThreadId::new()).is_err());

    drop(lease);
    assert!(
        limiter
            .reserve_active(ThreadId::new())
            .expect("terminal release should restore capacity")
            .is_some()
    );
}

#[test]
fn all_failed_same_thread_admissions_release_the_slot() {
    let limiter = limiter(Some(1));
    let thread_id = ThreadId::new();
    let first = limiter
        .reserve_active(thread_id)
        .expect("first turn should fit")
        .expect("configured limiter should return a lease");
    let second = limiter
        .reserve_active(thread_id)
        .expect("duplicate turn should share its admission")
        .expect("configured limiter should return a lease");

    drop(first);
    assert!(limiter.reserve_active(ThreadId::new()).is_err());
    drop(second);
    assert!(
        limiter
            .reserve_active(ThreadId::new())
            .expect("failed admissions should restore capacity")
            .is_some()
    );
}

#[test]
fn losing_same_thread_admission_cannot_release_started_worker() {
    let limiter = limiter(Some(1));
    let thread_id = ThreadId::new();
    let mut winner = limiter
        .reserve_active(thread_id)
        .expect("first turn should fit")
        .expect("configured limiter should return a lease");
    let loser = limiter
        .reserve_active(thread_id)
        .expect("competing turn should share its admission")
        .expect("configured limiter should return a lease");

    winner.mark_task_started();
    drop(loser);

    assert!(limiter.reserve_active(ThreadId::new()).is_err());
    drop(winner);
    assert!(
        limiter
            .reserve_active(ThreadId::new())
            .expect("terminal release should restore capacity")
            .is_some()
    );
}

#[test]
fn concurrent_admission_never_exceeds_ceiling() {
    const WORKERS: usize = 8;
    const MAX_CONCURRENT: usize = 2;
    let limiter = limiter(Some(MAX_CONCURRENT));
    let start = Arc::new(Barrier::new(WORKERS));
    let held = Arc::new(Barrier::new(WORKERS));
    let admitted = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            let limiter = Arc::clone(&limiter);
            let start = Arc::clone(&start);
            let held = Arc::clone(&held);
            let admitted = Arc::clone(&admitted);
            scope.spawn(move || {
                start.wait();
                let lease = limiter.reserve_active(ThreadId::new()).ok().flatten();
                if lease.is_some() {
                    admitted.fetch_add(1, Ordering::Relaxed);
                }
                held.wait();
                drop(lease);
            });
        }
    });

    assert_eq!(admitted.load(Ordering::Relaxed), MAX_CONCURRENT);
}

#[test]
fn overlapping_task_leases_release_only_after_the_last_holder_drops() {
    let limiter = limiter(Some(1));
    let thread_id = ThreadId::new();
    let mut first_task = limiter
        .reserve_active(thread_id)
        .expect("first turn should fit")
        .expect("configured limiter should return a lease");
    first_task.mark_task_started();
    let mut second_task = limiter
        .reserve_active(thread_id)
        .expect("overlapping same-thread turn should share its admission")
        .expect("configured limiter should return a lease");
    second_task.mark_task_started();
    drop(first_task);

    assert!(limiter.reserve_active(ThreadId::new()).is_err());
    drop(second_task);
    assert!(
        limiter
            .reserve_active(ThreadId::new())
            .expect("current lease should release normally")
            .is_some()
    );
}

#[test]
fn dropped_submission_lease_cannot_release_a_live_task() {
    let limiter = limiter(Some(1));
    let thread_id = ThreadId::new();
    let mut task = limiter
        .reserve_active(thread_id)
        .expect("task should fit")
        .expect("configured limiter should return a lease");
    task.mark_task_started();

    // A queued request may outlive its caller. Its temporary borrower must not clear the
    // task-owned slot when the request waiter is dropped.
    let submission = limiter
        .reserve_active(thread_id)
        .expect("same-thread submission should share the admission")
        .expect("configured limiter should return a lease");
    drop(submission);

    assert!(limiter.reserve_active(ThreadId::new()).is_err());
    drop(task);
    assert!(
        limiter
            .reserve_active(ThreadId::new())
            .expect("task completion should restore capacity")
            .is_some()
    );
}

#[test]
fn stale_task_release_cannot_clear_a_reused_thread_admission() {
    let limiter = limiter(Some(1));
    let thread_id = ThreadId::new();
    let mut old_task = limiter
        .reserve_active(thread_id)
        .expect("old task should fit")
        .expect("configured limiter should return a lease");
    old_task.mark_task_started();
    let old_generation = match &old_task.state {
        TeamWorkerLeaseState::Active { generation, .. } => *generation,
        TeamWorkerLeaseState::PendingSpawn => panic!("active reservation should have a generation"),
    };

    // Model the old task's holder being released before a stale submission lease is dropped.
    // A later admission must get a fresh generation, and the stale drop must not clear it.
    limiter.release_task(thread_id, old_generation);
    let new_task = limiter
        .reserve_active(thread_id)
        .expect("replacement task should fit")
        .expect("configured limiter should return a lease");
    drop(old_task);

    assert!(limiter.reserve_active(ThreadId::new()).is_err());
    drop(new_task);
    assert!(
        limiter
            .reserve_active(ThreadId::new())
            .expect("replacement completion should restore capacity")
            .is_some()
    );
}

#[tokio::test]
async fn releasing_a_lease_wakes_capacity_waiters() {
    let limiter = limiter(Some(1));
    let lease = limiter
        .reserve_active(ThreadId::new())
        .expect("turn should fit")
        .expect("configured limiter should return a lease");
    let waiter_limiter = Arc::clone(&limiter);
    let waiter = tokio::spawn(async move { waiter_limiter.wait_for_capacity().await });

    tokio::task::yield_now().await;
    drop(lease);
    tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .expect("capacity waiter should be woken")
        .expect("capacity waiter should finish");
}
