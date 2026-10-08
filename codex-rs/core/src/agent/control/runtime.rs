//! Shared state and startup bindings for one local agent tree.
//! Registry identity is allocation identity; cloning this handle preserves ownership checks.

use super::LocalAgentControl;
use super::execution::AgentExecutionLimiter;
use super::residency::V2Residency;
use super::worker_limit::TeamWorkerLimiter;
use crate::agent::control::worker_question::WorkerQuestionRegistry;
use crate::agent::eta_reminders::EtaReminderController;

use crate::agent::api::AgentControl;
use crate::agent::registry::AgentRegistry;
use crate::config::RolloutBudgetConfig;
use crate::rollout_budget::RolloutBudget;
use crate::thread_manager::ThreadIdGenerator;
use crate::thread_manager::ThreadManagerState;
use arc_swap::ArcSwap;
use arc_swap::ArcSwapOption;
use codex_extension_api::ThreadInstructionsProvider;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::ThreadUsagePolicy;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

#[derive(Debug, Default)]
pub(crate) struct AgentTreeShutdownState {
    members: TaskTracker,
    failed: AtomicBool,
}

impl AgentTreeShutdownState {
    pub(crate) async fn wait(&self) -> CodexResult<()> {
        self.members.wait().await;
        if self.failed.load(Ordering::Acquire) {
            return Err(CodexErr::Fatal(
                "agent tree shutdown did not complete cleanly".to_owned(),
            ));
        }
        Ok(())
    }

    fn record_failure(&self) {
        self.failed.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
pub(crate) struct AgentTreeMembership {
    state: Arc<AgentTreeShutdownState>,
    _member: TaskTrackerToken,
}

impl AgentTreeMembership {
    pub(crate) fn into_teardown_guard(self) -> AgentTreeTeardownGuard {
        AgentTreeTeardownGuard {
            membership: self,
            completed: false,
        }
    }
}

/// Marks tree shutdown as failed if teardown work exits without completing.
pub(crate) struct AgentTreeTeardownGuard {
    membership: AgentTreeMembership,
    completed: bool,
}

impl AgentTreeTeardownGuard {
    pub(crate) fn clone_for_teardown(&self) -> Self {
        self.membership.clone().into_teardown_guard()
    }

    pub(crate) fn record_shutdown_failure(&self) {
        self.membership.state.record_failure();
    }

    pub(crate) fn complete(mut self) {
        self.completed = true;
    }
}

impl Drop for AgentTreeTeardownGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.record_shutdown_failure();
        }
    }
}

/// Local tree state, kept separate from the shared agent operation interface.
#[derive(Clone)]
pub(crate) struct LocalAgentRuntime {
    /// Weak handle back to the global thread registry/state.
    /// This is `Weak` to avoid reference cycles and shadow persistence of the form
    /// `ThreadManagerState -> CodexThread -> Session -> SessionServices -> ThreadManagerState`.
    pub(super) manager: Weak<ThreadManagerState>,
    /// Captured at construction so delegates retain their manager's allocation policy.
    pub(super) thread_id_generator: ThreadIdGenerator,
    pub(super) agent_execution_limiter: Arc<AgentExecutionLimiter>,
    /// Atomic admission shared by direct Workers and pending spawns.
    pub(super) team_worker_limiter: Arc<TeamWorkerLimiter>,
    /// Session-scoped state shared by the root thread and every cloned sub-agent control handle.
    pub(super) rollout_budget: Arc<RolloutBudget>,
    /// The user-selected root routing tier, shared by the entire agent tree.
    pub(super) root_service_tier: Arc<ArcSwapOption<String>>,
    /// Serializes root tier commits with descendant synchronization.
    pub(super) root_service_tier_update: Arc<Mutex<()>>,
    /// Serializes settings events sent while a root routing tier changes.
    pub(super) root_service_tier_propagation: Arc<Mutex<()>>,
    /// The complete root usage policy, shared by the entire agent tree.
    pub(super) root_usage_policy: Arc<ArcSwap<ThreadUsagePolicy>>,
    /// Serializes root usage-policy commits with descendant synchronization.
    pub(super) root_usage_auto_resume_update: Arc<Mutex<()>>,
    /// Advances whenever the root automatic-resume setting changes.
    pub(super) root_usage_auto_resume_generation: Arc<AtomicU64>,
    /// Serializes settings events sent while the root usage policy changes.
    pub(super) root_usage_auto_resume_propagation: Arc<Mutex<()>>,
    /// Root-scoped process-local manual pause switch shared by loaded descendants.
    pub(super) root_activity_paused: Arc<AtomicBool>,
    /// Serializes durable Team activity transitions across the loaded root tree.
    pub(super) root_activity_transition: Arc<Mutex<()>>,
    /// Serializes manual pause publication with child startup reconciliation.
    pub(super) root_activity_pause_update: Arc<Mutex<()>>,
    /// Serializes descendant activity propagation and preserves toggle order.
    pub(super) root_activity_pause_propagation: Arc<Mutex<()>>,
    /// Wakes retained turns when the root activity pause is released.
    pub(super) root_activity_resume_notify: Arc<Notify>,
    /// Root-scoped process-local fence that rejects new work during daemon handoff.
    pub(crate) handoff_admission_sealed: Arc<AtomicBool>,
    /// Number of admissions that passed the handoff fence before it sealed.
    pub(crate) handoff_admission_in_flight: Arc<AtomicU32>,
    /// Number of terminal deliveries and watcher registrations in flight.
    pub(crate) handoff_delivery_state: Arc<AtomicU64>,
    /// Set when a sealed completion could not be persisted durably.
    pub(crate) handoff_delivery_failed: Arc<AtomicBool>,
    /// Set when a durable inbound payload is incompatible or malformed.
    pub(crate) handoff_inbound_unsupported: Arc<AtomicBool>,
    /// Threads intentionally stopped by handoff, keyed by thread id.
    pub(crate) handoff_suspended_threads: Arc<StdMutex<HashSet<ThreadId>>>,
    /// Wakes the handoff coordinator after admission and terminal delivery transitions.
    pub(crate) handoff_admission_notify: Arc<Notify>,
    /// Retains the root's opt-in instruction provider even when the root is unloaded.
    pub(super) shared_thread_instructions_provider:
        Arc<OnceLock<Arc<dyn ThreadInstructionsProvider>>>,
    pub(super) registry: Arc<AgentRegistry>,
    /// One outstanding explicit question per Worker, shared by the local tree.
    pub(super) worker_questions: Arc<WorkerQuestionRegistry>,
    pub(super) residency: Arc<V2Residency>,
    /// One-shot ETA reminders shared by the root and all descendants.
    pub(super) eta_reminders: Arc<EtaReminderController>,
    /// Shared by every session in this tree, including private delegates.
    pub(crate) shutdown: CancellationToken,
    shutdown_state: Arc<AgentTreeShutdownState>,
}

impl LocalAgentRuntime {
    pub(super) fn new(
        manager: Weak<ThreadManagerState>,
        thread_id_generator: ThreadIdGenerator,
        rollout_budget: Option<RolloutBudgetConfig>,
    ) -> Self {
        let runtime = Self {
            manager,
            thread_id_generator,
            registry: Arc::default(),
            worker_questions: Arc::default(),
            residency: Arc::default(),
            shutdown: CancellationToken::new(),
            shutdown_state: Arc::default(),
            agent_execution_limiter: Arc::default(),
            team_worker_limiter: Arc::default(),
            rollout_budget: Arc::default(),
            root_service_tier: Arc::new(ArcSwapOption::from(None)),
            root_service_tier_update: Arc::new(Mutex::new(())),
            root_service_tier_propagation: Arc::new(Mutex::new(())),
            root_usage_policy: Arc::new(ArcSwap::from_pointee(ThreadUsagePolicy::default())),
            root_usage_auto_resume_update: Arc::new(Mutex::new(())),
            root_usage_auto_resume_generation: Arc::new(AtomicU64::new(0)),
            root_usage_auto_resume_propagation: Arc::new(Mutex::new(())),
            root_activity_paused: Arc::new(AtomicBool::new(false)),
            root_activity_transition: Arc::new(Mutex::new(())),
            root_activity_pause_update: Arc::new(Mutex::new(())),
            root_activity_pause_propagation: Arc::new(Mutex::new(())),
            root_activity_resume_notify: Arc::new(Notify::new()),
            handoff_admission_sealed: Arc::new(AtomicBool::new(false)),
            handoff_admission_in_flight: Arc::new(AtomicU32::new(0)),
            handoff_delivery_state: Arc::new(AtomicU64::new(0)),
            handoff_delivery_failed: Arc::new(AtomicBool::new(false)),
            handoff_inbound_unsupported: Arc::new(AtomicBool::new(false)),
            handoff_suspended_threads: Arc::new(StdMutex::new(HashSet::new())),
            handoff_admission_notify: Arc::new(Notify::new()),
            shared_thread_instructions_provider: Arc::default(),
            eta_reminders: Arc::new(EtaReminderController::default()),
        };
        if let Some(rollout_budget) = rollout_budget {
            runtime.rollout_budget.configure(rollout_budget);
        }
        runtime
    }

    /// Bind local startup to the same tree state with this session's identity.
    pub(crate) fn control(&self, session_id: SessionId) -> LocalAgentControl {
        LocalAgentControl {
            session_id,
            runtime: self.clone(),
        }
    }

    pub(crate) fn set_root_service_tier(&self, service_tier: Option<String>) {
        self.root_service_tier.store(service_tier.map(Arc::new));
    }

    pub(crate) fn set_root_usage_policy(&self, policy: ThreadUsagePolicy) {
        if self.root_usage_policy.load().auto_resume != policy.auto_resume {
            self.root_usage_auto_resume_generation
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |generation| {
                    Some(generation.saturating_add(1))
                })
                .expect("generation update always succeeds");
        }
        self.root_usage_policy.store(Arc::new(policy));
    }

    pub(crate) fn initialize_limits(
        &self,
        max_threads: usize,
        team_worker_max_concurrent: Option<usize>,
    ) {
        self.agent_execution_limiter.initialize(max_threads);
        self.team_worker_limiter
            .initialize(team_worker_max_concurrent);
    }
}

impl std::ops::Deref for LocalAgentControl {
    type Target = LocalAgentRuntime;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

/// Local construction binds identity after reading history. Hosts and internal children
/// provide an already-bound controller without selecting a backend again.
#[derive(Clone)]
pub(crate) enum AgentControlInit {
    Local(LocalAgentControl),
    Provided {
        control: Arc<dyn AgentControl>,
        runtime: LocalAgentRuntime,
    },
}

impl From<LocalAgentControl> for AgentControlInit {
    fn from(control: LocalAgentControl) -> Self {
        Self::Local(control)
    }
}

impl AgentControlInit {
    pub(crate) fn runtime(&self) -> &LocalAgentRuntime {
        match self {
            Self::Local(control) => &control.runtime,
            Self::Provided { runtime, .. } => runtime,
        }
    }

    pub(crate) fn control(&self) -> &dyn AgentControl {
        match self {
            Self::Local(control) => control,
            Self::Provided { control, .. } => control.as_ref(),
        }
    }
}

impl LocalAgentRuntime {
    pub(crate) fn generate_thread_id(&self) -> ThreadId {
        (self.thread_id_generator)()
    }

    pub(crate) fn root_thread_instructions_provider(
        &self,
        root_thread_id: ThreadId,
        provider: Option<Arc<dyn ThreadInstructionsProvider>>,
    ) -> Option<Arc<dyn ThreadInstructionsProvider>> {
        let provider = match self.manager.upgrade() {
            Some(manager) => manager.shared_thread_instructions_provider(root_thread_id, provider),
            None => provider,
        };
        if let Some(provider) = provider
            .as_ref()
            .filter(|provider| provider.share_with_subagents())
        {
            let _ = self
                .shared_thread_instructions_provider
                .set(Arc::clone(provider));
        }
        provider
    }
}

impl LocalAgentRuntime {
    pub(crate) fn admit_start(&self) -> CodexResult<AgentTreeMembership> {
        if self.shutdown_state.members.is_closed() {
            return Err(CodexErr::InvalidRequest(
                "agent runtime is shutting down".to_owned(),
            ));
        }
        let membership = AgentTreeMembership {
            state: Arc::clone(&self.shutdown_state),
            _member: self.shutdown_state.members.token(),
        };
        // Closing a TaskTracker does not reject new tokens. Recheck so a start racing with
        // shutdown is either admitted before the fence or rejected after it.
        if self.shutdown_state.members.is_closed() {
            return Err(CodexErr::InvalidRequest(
                "agent runtime is shutting down".to_owned(),
            ));
        }
        Ok(membership)
    }

    pub(crate) fn request_shutdown(&self) -> Arc<AgentTreeShutdownState> {
        self.shutdown_state.members.close();
        self.shutdown.cancel();
        Arc::clone(&self.shutdown_state)
    }

    pub(crate) fn record_shutdown_failure(&self) {
        self.shutdown_state.record_failure();
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
