use super::AgentControl;
use crate::config::Config;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TeamMode;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use tokio::sync::Notify;

/// Shared admission state for a Lead's direct Worker turns.
///
/// The state covers both threads that have been created and spawn operations that have not yet
/// received a thread ID. Holding the mutex across the capacity check and state update makes each
/// admission atomic with respect to concurrent spawn and turn requests.
#[derive(Default)]
pub(super) struct TeamWorkerLimiter {
    max_concurrent: OnceLock<Option<usize>>,
    state: Mutex<TeamWorkerLimiterState>,
    capacity_available: Notify,
}

#[derive(Default)]
struct TeamWorkerLimiterState {
    active_workers: HashMap<ThreadId, ActiveWorkerAdmission>,
    pending_spawns: usize,
    next_generation: u64,
}

/// One active-thread slot can have multiple callers racing to submit the same turn. Borrowers
/// keep the admission alive until each submission reports its outcome. A task holder is retained
/// by the actual running task, so a dropped submission cannot release a live Worker admission.
struct ActiveWorkerAdmission {
    generation: u64,
    borrowers: usize,
    task_holders: usize,
}

pub(crate) struct TeamWorkerLease {
    limiter: Arc<TeamWorkerLimiter>,
    state: TeamWorkerLeaseState,
}

enum TeamWorkerLeaseState {
    PendingSpawn,
    Active {
        thread_id: ThreadId,
        generation: u64,
        task_owned: bool,
    },
}

impl TeamWorkerLimiter {
    pub(super) fn initialize(&self, max_concurrent: Option<usize>) {
        self.max_concurrent.get_or_init(|| max_concurrent);
    }

    pub(super) fn reserve_pending_spawn(self: &Arc<Self>) -> CodexResult<Option<TeamWorkerLease>> {
        let Some(max_concurrent) = self.max_concurrent() else {
            return Ok(None);
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .active_workers
            .len()
            .saturating_add(state.pending_spawns)
            >= max_concurrent
        {
            return Err(limit_reached(max_concurrent));
        }
        state.pending_spawns += 1;
        Ok(Some(TeamWorkerLease {
            limiter: Arc::clone(self),
            state: TeamWorkerLeaseState::PendingSpawn,
        }))
    }

    pub(super) fn reserve_active(
        self: &Arc<Self>,
        thread_id: ThreadId,
    ) -> CodexResult<Option<TeamWorkerLease>> {
        let Some(max_concurrent) = self.max_concurrent() else {
            return Ok(None);
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(admission) = state.active_workers.get_mut(&thread_id) {
            admission.borrowers += 1;
            return Ok(Some(TeamWorkerLease {
                limiter: Arc::clone(self),
                state: TeamWorkerLeaseState::Active {
                    thread_id,
                    generation: admission.generation,
                    task_owned: false,
                },
            }));
        }
        if state
            .active_workers
            .len()
            .saturating_add(state.pending_spawns)
            >= max_concurrent
        {
            return Err(limit_reached(max_concurrent));
        }
        let generation = next_generation(&mut state);
        state.active_workers.insert(
            thread_id,
            ActiveWorkerAdmission {
                generation,
                borrowers: 1,
                task_holders: 0,
            },
        );
        Ok(Some(TeamWorkerLease {
            limiter: Arc::clone(self),
            state: TeamWorkerLeaseState::Active {
                thread_id,
                generation,
                task_owned: false,
            },
        }))
    }

    fn mark_task_started(&self, thread_id: ThreadId, generation: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let admission = state
            .active_workers
            .get_mut(&thread_id)
            .filter(|admission| admission.generation == generation)
            .expect("team Worker task must retain its admission");
        admission.task_holders += 1;
    }

    fn release_borrower(&self, thread_id: ThreadId, generation: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let should_remove = if let Some(admission) = state.active_workers.get_mut(&thread_id)
            && admission.generation == generation
        {
            debug_assert!(admission.borrowers > 0);
            admission.borrowers = admission.borrowers.saturating_sub(1);
            admission.borrowers == 0 && admission.task_holders == 0
        } else {
            false
        };
        if should_remove {
            state.active_workers.remove(&thread_id);
            self.capacity_available.notify_waiters();
        }
    }

    fn release_task(&self, thread_id: ThreadId, generation: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let should_remove = if let Some(admission) = state.active_workers.get_mut(&thread_id)
            && admission.generation == generation
        {
            debug_assert!(admission.borrowers > 0);
            debug_assert!(admission.task_holders > 0);
            admission.borrowers = admission.borrowers.saturating_sub(1);
            admission.task_holders = admission.task_holders.saturating_sub(1);
            admission.borrowers == 0 && admission.task_holders == 0
        } else {
            false
        };
        if should_remove {
            state.active_workers.remove(&thread_id);
            self.capacity_available.notify_waiters();
        }
    }

    fn release_pending_spawn(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(state.pending_spawns > 0);
        state.pending_spawns = state.pending_spawns.saturating_sub(1);
        self.capacity_available.notify_waiters();
    }

    fn commit_pending_spawn(&self, thread_id: ThreadId) -> u64 {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(state.pending_spawns > 0);
        state.pending_spawns = state.pending_spawns.saturating_sub(1);
        let generation = next_generation(&mut state);
        assert!(
            state
                .active_workers
                .insert(
                    thread_id,
                    ActiveWorkerAdmission {
                        generation,
                        borrowers: 1,
                        task_holders: 0,
                    },
                )
                .is_none(),
            "a team Worker thread must be admitted only once"
        );
        generation
    }

    fn max_concurrent(&self) -> Option<usize> {
        self.max_concurrent.get().copied().flatten()
    }

    async fn wait_for_capacity(&self) {
        loop {
            let notified = self.capacity_available.notified();
            let has_capacity = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                self.max_concurrent().is_none_or(|max_concurrent| {
                    state
                        .active_workers
                        .len()
                        .saturating_add(state.pending_spawns)
                        < max_concurrent
                })
            };
            if has_capacity {
                return;
            }
            notified.await;
        }
    }
}

impl TeamWorkerLease {
    pub(super) fn commit_pending_spawn(&mut self, thread_id: ThreadId) {
        if !matches!(&self.state, TeamWorkerLeaseState::PendingSpawn) {
            return;
        }
        let generation = self.limiter.commit_pending_spawn(thread_id);
        self.state = TeamWorkerLeaseState::Active {
            thread_id,
            generation,
            task_owned: false,
        };
    }

    /// Promotes this reservation to ownership retained by the actual running task.
    pub(crate) fn mark_task_started(&mut self) {
        let TeamWorkerLeaseState::Active {
            thread_id,
            generation,
            task_owned,
        } = &mut self.state
        else {
            return;
        };
        if !*task_owned {
            self.limiter.mark_task_started(*thread_id, *generation);
            *task_owned = true;
        }
    }
}

impl AgentControl {
    pub(crate) fn reserve_team_worker_spawn(
        &self,
        config: &Config,
        session_source: Option<&SessionSource>,
    ) -> CodexResult<Option<TeamWorkerLease>> {
        if !self.is_direct_team_worker(config, session_source) {
            return Ok(None);
        }
        self.team_worker_limiter.reserve_pending_spawn()
    }

    pub(crate) fn reserve_team_worker_turn(
        &self,
        config: &Config,
        session_source: &SessionSource,
        thread_id: ThreadId,
    ) -> CodexResult<Option<TeamWorkerLease>> {
        if !self.is_direct_team_worker(config, Some(session_source)) {
            return Ok(None);
        }
        self.team_worker_limiter.reserve_active(thread_id)
    }

    pub(crate) async fn wait_for_team_worker_capacity(&self) {
        self.team_worker_limiter.wait_for_capacity().await;
    }

    fn is_direct_team_worker(
        &self,
        config: &Config,
        session_source: Option<&SessionSource>,
    ) -> bool {
        config.team_mode == TeamMode::LeadWorker
            && matches!(
                session_source,
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    ..
                })) if *parent_thread_id == ThreadId::from(self.session_id)
            )
    }
}

impl Drop for TeamWorkerLease {
    fn drop(&mut self) {
        match &self.state {
            TeamWorkerLeaseState::PendingSpawn => self.limiter.release_pending_spawn(),
            TeamWorkerLeaseState::Active {
                thread_id,
                generation,
                task_owned,
            } => {
                if *task_owned {
                    self.limiter.release_task(*thread_id, *generation);
                } else {
                    self.limiter.release_borrower(*thread_id, *generation);
                }
            }
        }
    }
}

fn next_generation(state: &mut TeamWorkerLimiterState) -> u64 {
    let generation = state.next_generation;
    state.next_generation = state
        .next_generation
        .checked_add(1)
        .expect("team Worker admission generation exhausted");
    generation
}

fn limit_reached(max_concurrent: usize) -> CodexErr {
    CodexErr::new(CodexErrorDetails::AgentLimitReached {
        max_threads: max_concurrent,
    })
}

#[cfg(test)]
#[path = "worker_limit_tests.rs"]
mod tests;
