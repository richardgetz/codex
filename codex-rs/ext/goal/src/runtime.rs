use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_core::CodexThread;
use codex_core::StartIfIdleSubmission;
use codex_core::ThreadManager;
use codex_core::TurnInput;
use codex_core::TurnInputRequest;
use codex_core::TurnStartOptions;
use codex_extension_api::ExtensionData;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadGoal;

use crate::accounting::BudgetLimitedGoalDisposition;
use crate::accounting::GoalAccountingState;
use crate::analytics::GoalAnalytics;
use crate::analytics::GoalEventAttribution;
use crate::events::GoalEventEmitter;
use crate::metrics::GoalMetrics;
use crate::steering::continuation_steering_item;
use crate::steering::objective_updated_steering_item;
use crate::tool::protocol_goal_from_state;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tokio::sync::watch;

use codex_tools::ToolWaitHandle;

#[derive(Clone)]
pub struct GoalRuntimeHandle {
    inner: Arc<GoalRuntimeInner>,
}

pub(crate) struct GoalRuntimeConfig {
    pub(crate) analytics: GoalAnalytics,
    pub(crate) enabled: bool,
    pub(crate) tools_available_for_thread: bool,
    pub(crate) root_accounting_state: Option<Arc<GoalAccountingState>>,
}

pub(crate) enum ActiveGoalStopReason {
    TurnError,
    UsageLimit,
    ExecutionUnavailable { expected_goal_id: String },
    EmptyResponse,
}

struct GoalRuntimeInner {
    thread_id: ThreadId,
    state_dbs: Arc<codex_state::StateRuntime>,
    analytics: GoalAnalytics,
    event_emitter: GoalEventEmitter,
    metrics: GoalMetrics,
    thread_manager: Weak<ThreadManager>,
    accounting_state: Arc<GoalAccountingState>,
    root_accounting_state: Option<Arc<GoalAccountingState>>,
    enabled: AtomicBool,
    tools_available_for_thread: bool,
    goal_state_lock: Semaphore,
    background_wait: Mutex<GoalBackgroundWait>,
}

/// Tracks background work emitted by the active goal turn. The generation and
/// cancellation channel invalidate watchers when a new turn or goal state
/// wins the race with an exit notification.
struct GoalBackgroundWait {
    generation: u64,
    turn_id: Option<String>,
    process_ids: BTreeSet<i32>,
    watcher_started: bool,
    cancellation: watch::Sender<bool>,
}

/// Captures the background-wait generation at tool admission. A tool that
/// started before an external goal mutation cannot attach its late output to
/// the replacement goal, while tools admitted afterward can still wait.
#[derive(Debug, Default)]
pub(crate) struct GoalToolWaitScopes {
    values: std::sync::Mutex<HashMap<String, u64>>,
}

impl GoalToolWaitScopes {
    fn record(&self, call_id: &str, generation: u64) {
        self.values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(call_id.to_string(), generation);
    }

    fn take(&self, call_id: &str) -> Option<u64> {
        self.values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(call_id)
    }
}

impl Default for GoalBackgroundWait {
    fn default() -> Self {
        Self {
            generation: 0,
            turn_id: None,
            process_ids: BTreeSet::new(),
            watcher_started: false,
            cancellation: watch::channel(false).0,
        }
    }
}

struct GoalBackgroundWaitSnapshot {
    generation: u64,
    process_ids: Vec<i32>,
    cancellation: watch::Receiver<bool>,
}

enum GoalBackgroundWaitClaim {
    Start(GoalBackgroundWaitSnapshot),
    AlreadyWaiting,
}

async fn wait_for_background_wait_cancellation(mut cancellation: watch::Receiver<bool>) {
    while !*cancellation.borrow() {
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}

pub(crate) struct AccountedGoalProgress {
    pub(crate) goal: ThreadGoal,
    pub(crate) goal_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviousGoalSnapshot {
    pub goal_id: String,
    pub status: codex_state::ThreadGoalStatus,
    pub objective: String,
}

impl From<&codex_state::ThreadGoal> for PreviousGoalSnapshot {
    fn from(goal: &codex_state::ThreadGoal) -> Self {
        Self {
            goal_id: goal.goal_id.clone(),
            status: goal.status,
            objective: goal.objective.clone(),
        }
    }
}

impl std::fmt::Debug for GoalRuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoalRuntimeHandle").finish_non_exhaustive()
    }
}

impl GoalRuntimeHandle {
    pub(crate) fn new(
        thread_id: ThreadId,
        state_dbs: Arc<codex_state::StateRuntime>,
        event_emitter: GoalEventEmitter,
        metrics: GoalMetrics,
        thread_manager: Weak<ThreadManager>,
        accounting_state: Arc<GoalAccountingState>,
        config: GoalRuntimeConfig,
    ) -> Self {
        Self {
            inner: Arc::new(GoalRuntimeInner {
                thread_id,
                state_dbs,
                analytics: config.analytics,
                event_emitter,
                metrics,
                thread_manager,
                accounting_state,
                root_accounting_state: config.root_accounting_state,
                enabled: AtomicBool::new(config.enabled),
                tools_available_for_thread: config.tools_available_for_thread,
                goal_state_lock: Semaphore::new(/*permits*/ 1),
                background_wait: Mutex::new(GoalBackgroundWait::default()),
            }),
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
    }

    pub(crate) fn tools_visible(&self) -> bool {
        self.is_enabled() && self.inner.tools_available_for_thread
    }

    pub(crate) fn thread_id(&self) -> ThreadId {
        self.inner.thread_id
    }

    pub(crate) fn accounting_state(&self) -> Arc<GoalAccountingState> {
        Arc::clone(&self.inner.accounting_state)
    }

    pub(crate) fn root_accounting_state(&self) -> Option<Arc<GoalAccountingState>> {
        self.inner.root_accounting_state.clone()
    }

    pub(crate) async fn clear_pending_turn_start_options(&self) {
        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            return;
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            return;
        };
        thread.thread_extension_data().remove::<TurnStartOptions>();
    }

    pub(crate) async fn goal_state_permit(&self) -> Result<SemaphorePermit<'_>, String> {
        self.inner
            .goal_state_lock
            .acquire()
            .await
            .map_err(|err| err.to_string())
    }

    /// Records a process emitted by the active Goal turn as external work that
    /// must finish before an automatic continuation is admitted.
    pub(crate) async fn register_background_wait(
        &self,
        turn_id: &str,
        turn_store: &ExtensionData,
        call_id: &str,
        wait_handle: &ToolWaitHandle,
    ) {
        let Some(process_id) = wait_handle.id().parse::<i32>().ok() else {
            return;
        };
        let Some(tool_generation) = turn_store
            .get::<GoalToolWaitScopes>()
            .and_then(|scopes| scopes.take(call_id))
        else {
            return;
        };
        let mut wait = self.inner.background_wait.lock().await;
        if wait.generation != tool_generation
            || self
                .inner
                .accounting_state
                .current_active_goal_id_for_turn(turn_id)
                .is_none()
        {
            return;
        }
        if wait.turn_id.as_deref() != Some(turn_id) {
            wait.generation = wait.generation.wrapping_add(1);
            wait.turn_id = Some(turn_id.to_string());
            wait.process_ids.clear();
            wait.watcher_started = false;
            let _ = wait.cancellation.send(true);
            wait.cancellation = watch::channel(false).0;
        }
        wait.process_ids.insert(process_id);
    }

    /// Records the wait-generation for a tool before its handler runs.
    pub(crate) async fn capture_tool_wait_scope(&self, turn_store: &ExtensionData, call_id: &str) {
        // External goal writes hold this permit through their prepare/write
        // window. Taking it here makes the generation snapshot an admission
        // boundary: a tool that starts before a mutation keeps the old
        // generation, while a tool that starts afterward sees the new one.
        let Ok(_goal_state_permit) = self.goal_state_permit().await else {
            return;
        };
        let wait = self.inner.background_wait.lock().await;
        let generation = wait.generation;
        drop(wait);
        turn_store
            .get_or_init(GoalToolWaitScopes::default)
            .record(call_id, generation);
    }

    /// Cancels any watcher and advances the wait generation.
    pub(crate) async fn invalidate_background_wait(&self) {
        let Ok(_goal_state_permit) = self.goal_state_permit().await else {
            return;
        };
        self.invalidate_background_wait_locked().await;
    }

    /// Invalidates a wait while the caller already owns the goal-state permit.
    pub(crate) async fn invalidate_background_wait_locked(&self) {
        let mut wait = self.inner.background_wait.lock().await;
        wait.generation = wait.generation.wrapping_add(1);
        wait.turn_id = None;
        wait.process_ids.clear();
        wait.watcher_started = false;
        let _ = wait.cancellation.send(true);
        wait.cancellation = watch::channel(false).0;
    }

    /// Starts a fresh turn's wait scope and cancels any stale watcher.
    pub(crate) async fn begin_background_wait_turn(&self, turn_id: &str) {
        let mut wait = self.inner.background_wait.lock().await;
        wait.generation = wait.generation.wrapping_add(1);
        wait.turn_id = None;
        wait.process_ids.clear();
        wait.watcher_started = false;
        let _ = wait.cancellation.send(true);
        wait.cancellation = watch::channel(false).0;
        wait.turn_id = Some(turn_id.to_string());
    }

    pub(crate) async fn cancel_background_wait(&self) {
        self.invalidate_background_wait().await;
    }

    async fn claim_background_wait(&self) -> Option<GoalBackgroundWaitClaim> {
        let mut wait = self.inner.background_wait.lock().await;
        if wait.process_ids.is_empty() {
            return None;
        }
        if wait.watcher_started {
            return Some(GoalBackgroundWaitClaim::AlreadyWaiting);
        }
        wait.watcher_started = true;
        Some(GoalBackgroundWaitClaim::Start(GoalBackgroundWaitSnapshot {
            generation: wait.generation,
            process_ids: wait.process_ids.iter().copied().collect(),
            cancellation: wait.cancellation.subscribe(),
        }))
    }

    async fn current_background_wait_processes(&self, generation: u64) -> Option<Vec<i32>> {
        let wait = self.inner.background_wait.lock().await;
        (wait.generation == generation && wait.watcher_started)
            .then(|| wait.process_ids.iter().copied().collect())
    }

    async fn complete_background_wait(&self, generation: u64, process_ids: &[i32]) -> bool {
        let mut wait = self.inner.background_wait.lock().await;
        if wait.generation != generation
            || !wait.watcher_started
            || wait.process_ids.iter().copied().collect::<Vec<_>>() != process_ids
        {
            return false;
        }
        wait.generation = wait.generation.wrapping_add(1);
        wait.turn_id = None;
        wait.process_ids.clear();
        wait.watcher_started = false;
        wait.cancellation = watch::channel(false).0;
        true
    }

    fn spawn_background_wait_watcher(
        &self,
        thread: Arc<CodexThread>,
        snapshot: GoalBackgroundWaitSnapshot,
    ) {
        let runtime = self.clone();
        tokio::spawn(async move {
            let mut process_ids = snapshot.process_ids;
            loop {
                for process_id in &process_ids {
                    tokio::select! {
                        _ = wait_for_background_wait_cancellation(snapshot.cancellation.clone()) => return,
                        _ = thread.wait_for_background_terminal(*process_id) => {}
                    }
                }
                if *snapshot.cancellation.borrow() {
                    return;
                }
                let Some(current_process_ids) = runtime
                    .current_background_wait_processes(snapshot.generation)
                    .await
                else {
                    return;
                };
                if current_process_ids == process_ids {
                    break;
                }
                process_ids = current_process_ids;
            }

            if runtime
                .complete_background_wait(snapshot.generation, &process_ids)
                .await
                && let Err(err) = runtime.continue_if_idle().await
            {
                tracing::debug!(%err, "failed to resume Goal after background work exited");
            }
        });
    }

    pub async fn prepare_external_goal_mutation(&self) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        self.inner.accounting_state.reset_empty_responses();
        if let Some(turn_id) = self.inner.accounting_state.current_turn_id() {
            self.account_active_goal_progress(
                turn_id.as_str(),
                &format!("{turn_id}:external-goal-mutation"),
                codex_state::GoalAccountingMode::ActiveOnly,
                BudgetLimitedGoalDisposition::ClearActive,
            )
            .await?;
            return Ok(());
        }

        self.account_idle_goal_progress(
            &format!("{}:external-goal-mutation", self.inner.thread_id),
            codex_state::GoalAccountingMode::ActiveOnly,
            BudgetLimitedGoalDisposition::ClearActive,
        )
        .await?;
        Ok(())
    }

    pub async fn apply_external_goal_set(
        &self,
        goal: codex_state::ThreadGoal,
        previous_goal: Option<PreviousGoalSnapshot>,
    ) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        self.inner.accounting_state.reset_empty_responses();
        let replaced_existing_goal = previous_goal
            .as_ref()
            .is_some_and(|previous_goal| previous_goal.goal_id != goal.goal_id);
        if previous_goal.is_none() || replaced_existing_goal {
            self.inner.metrics.record_created();
            self.inner
                .analytics
                .created(&goal, GoalEventAttribution::NoTurn);
        }
        let previous_status = previous_goal
            .as_ref()
            .and_then(|previous_goal| (!replaced_existing_goal).then_some(previous_goal.status));
        self.inner
            .metrics
            .record_resumed_if_status_changed(previous_status, goal.status);
        self.inner
            .metrics
            .record_terminal_if_status_changed(previous_status, &goal);
        self.inner
            .analytics
            .status_changed(&goal, previous_status, GoalEventAttribution::NoTurn);
        let objective_changed = previous_goal.as_ref().is_some_and(|previous_goal| {
            !replaced_existing_goal && previous_goal.objective != goal.objective
        });
        match goal.status {
            codex_state::ThreadGoalStatus::Active => {
                if self.inner.accounting_state.current_turn_id().is_some() {
                    let _ = self
                        .inner
                        .accounting_state
                        .mark_current_turn_goal_active(goal.goal_id.clone());
                } else {
                    self.inner
                        .accounting_state
                        .mark_idle_goal_active(goal.goal_id.clone());
                }
                if objective_changed {
                    let item = objective_updated_steering_item(&protocol_goal_from_state(goal));
                    self.inject_active_turn_steering(item).await;
                }
                self.continue_if_idle().await?;
            }
            codex_state::ThreadGoalStatus::BudgetLimited => {
                if self.inner.accounting_state.current_turn_id().is_none() {
                    self.cancel_background_wait().await;
                    self.inner.accounting_state.clear_active_goal();
                }
            }
            codex_state::ThreadGoalStatus::Paused
            | codex_state::ThreadGoalStatus::Blocked
            | codex_state::ThreadGoalStatus::UsageLimited
            | codex_state::ThreadGoalStatus::Complete => {
                self.invalidate_background_wait().await;
                self.inner.accounting_state.clear_active_goal();
            }
        }
        Ok(())
    }

    pub async fn apply_external_goal_clear(
        &self,
        goal: codex_state::ThreadGoal,
    ) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        self.inner.analytics.cleared(&goal);
        self.invalidate_background_wait().await;
        self.inner.accounting_state.clear_active_goal();
        Ok(())
    }

    pub async fn usage_limit_active_goal_for_turn(&self, turn_id: &str) -> Result<(), String> {
        self.stop_active_goal_for_turn(turn_id, ActiveGoalStopReason::UsageLimit)
            .await
    }

    /// Accounts the ending turn and stops its active goal after a terminal error.
    pub(crate) async fn stop_active_goal_for_turn(
        &self,
        turn_id: &str,
        reason: ActiveGoalStopReason,
    ) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        // Hold this through accounting and the status update so external goal
        // mutations and idle continuation cannot interleave between them.
        let _goal_state_permit = self.goal_state_permit().await?;
        let is_empty_response = matches!(&reason, ActiveGoalStopReason::EmptyResponse);
        if !is_empty_response {
            self.invalidate_background_wait_locked().await;
        }
        let Some(accounting_goal_id) = self
            .inner
            .accounting_state
            .current_active_goal_id_for_turn(turn_id)
        else {
            return Ok(());
        };
        if let ActiveGoalStopReason::ExecutionUnavailable { expected_goal_id } = &reason
            && accounting_goal_id != *expected_goal_id
        {
            return Ok(());
        }

        let (event_name, status, expected_goal_id) = match reason {
            ActiveGoalStopReason::TurnError => {
                ("turn-error", codex_state::ThreadGoalStatus::Blocked, None)
            }
            ActiveGoalStopReason::UsageLimit => (
                "usage-limit",
                codex_state::ThreadGoalStatus::UsageLimited,
                None,
            ),
            ActiveGoalStopReason::EmptyResponse => {
                let Some(expected_goal_id) =
                    self.inner.accounting_state.empty_response_goal(turn_id)
                else {
                    return Ok(());
                };
                if accounting_goal_id != expected_goal_id {
                    return Ok(());
                }
                (
                    "empty-response",
                    codex_state::ThreadGoalStatus::Blocked,
                    Some(expected_goal_id),
                )
            }
            ActiveGoalStopReason::ExecutionUnavailable { expected_goal_id } => (
                "execution-unavailable",
                codex_state::ThreadGoalStatus::Blocked,
                Some(expected_goal_id),
            ),
        };
        self.account_active_goal_progress(
            turn_id,
            &format!("{turn_id}:{event_name}-progress"),
            codex_state::GoalAccountingMode::ActiveOnly,
            BudgetLimitedGoalDisposition::ClearActive,
        )
        .await?;

        let Some(active_goal) = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        else {
            self.invalidate_background_wait_locked().await;
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        };
        if expected_goal_id
            .as_ref()
            .is_some_and(|expected_goal_id| active_goal.goal_id != *expected_goal_id)
        {
            return Ok(());
        }
        let can_stop = active_goal.status == codex_state::ThreadGoalStatus::Active
            || (active_goal.status == codex_state::ThreadGoalStatus::BudgetLimited
                && status == codex_state::ThreadGoalStatus::UsageLimited);
        if !can_stop {
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }
        if is_empty_response {
            self.invalidate_background_wait_locked().await;
        }
        let previous_status = Some(active_goal.status);
        let Some(goal) = self
            .inner
            .state_dbs
            .thread_goals()
            .update_thread_goal(
                self.thread_id(),
                codex_state::GoalUpdate {
                    objective: None,
                    status: Some(status),
                    token_budget: None,
                    expected_goal_id: Some(active_goal.goal_id),
                },
            )
            .await
            .map_err(|err| err.to_string())?
        else {
            return Ok(());
        };
        self.inner
            .metrics
            .record_terminal_if_status_changed(previous_status, &goal);
        self.inner.analytics.status_changed(
            &goal,
            previous_status,
            GoalEventAttribution::Turn(turn_id),
        );
        self.inner.accounting_state.clear_active_goal();
        let goal = protocol_goal_from_state(goal);
        self.inner.event_emitter.thread_goal_updated(
            format!("{turn_id}:{event_name}"),
            Some(turn_id.to_string()),
            goal,
        );
        Ok(())
    }

    pub async fn restore_after_resume(&self) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        let goal = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?;
        match goal {
            Some(goal) if goal.status == codex_state::ThreadGoalStatus::Active => {
                self.inner
                    .accounting_state
                    .mark_idle_goal_active(goal.goal_id);
                self.inner.metrics.record_resumed();
            }
            Some(_) | None => self.inner.accounting_state.clear_active_goal(),
        }
        Ok(())
    }

    pub(crate) async fn continue_if_idle(&self) -> Result<(), String> {
        if !self.tools_visible() {
            self.cancel_background_wait().await;
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }
        // Hold this through the read/start window so external set/clear cannot
        // change the goal after we read it but before the continuation launches.
        let _goal_state_permit = self.goal_state_permit().await?;

        if self
            .inner
            .state_dbs
            .thread_goals()
            .has_thread_goal_continuation_deferral(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        {
            return Ok(());
        }

        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            tracing::debug!("skipping goal continuation because thread manager is unavailable");
            return Ok(());
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            tracing::debug!("skipping goal continuation because live thread is unavailable");
            return Ok(());
        };

        let Some(goal) = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        else {
            self.invalidate_background_wait_locked().await;
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        };
        if goal.status != codex_state::ThreadGoalStatus::Active {
            self.invalidate_background_wait_locked().await;
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }
        if let Some(wait) = self.claim_background_wait().await {
            match wait {
                GoalBackgroundWaitClaim::Start(wait) => {
                    self.spawn_background_wait_watcher(thread, wait);
                }
                GoalBackgroundWaitClaim::AlreadyWaiting => {}
            }
            return Ok(());
        }
        let start_options = thread
            .thread_extension_data()
            .get::<TurnStartOptions>()
            .map(|options| options.as_ref().clone())
            .unwrap_or_default();
        let item = continuation_steering_item(
            &protocol_goal_from_state(goal),
            thread.config().await.update_plan_enabled,
        );

        let submission = thread
            .start_turn_if_idle(
                TurnInputRequest::new(TurnInput::ResponseItem(item)).on_start(TurnStartOptions {
                    turn_trigger: Some("goal".to_string()),
                    ..start_options
                }),
            )
            .await;
        let started = match submission {
            Ok(StartIfIdleSubmission::Started { turn_id }) => {
                // Turn-stop evaluation takes the same permit, so even a fast response
                // cannot finish before this host-admitted continuation is identified.
                self.inner.accounting_state.mark_goal_continuation(turn_id);
                true
            }
            Ok(StartIfIdleSubmission::NotSubmitted { reason }) => {
                tracing::debug!(
                    ?reason,
                    "skipping goal continuation because automatic idle work was rejected"
                );
                false
            }
            Err(error) => {
                tracing::debug!(
                    %error,
                    "skipping goal continuation because turn input submission failed"
                );
                false
            }
        };

        // A Team Lead may be parked behind a direct Worker. Keep the active
        // Goal accounting state intact when admission declines this automatic
        // continuation; the Worker completion path will call us again.
        if !started {
            return Ok(());
        }

        let current_turn_is_goal_active = self
            .inner
            .accounting_state
            .current_turn_id()
            .is_some_and(|turn_id| {
                self.inner
                    .accounting_state
                    .current_active_goal_id_for_turn(turn_id.as_str())
                    .is_some()
            });
        if !current_turn_is_goal_active {
            self.inner
                .accounting_state
                .reset_idle_progress_baseline_and_clear_active_goal();
        }
        Ok(())
    }

    pub(crate) async fn inject_active_turn_steering(&self, item: ResponseItem) {
        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            tracing::debug!("skipping goal steering because thread manager is unavailable");
            return;
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            tracing::debug!("skipping goal steering because live thread is unavailable");
            return;
        };
        if thread.inject_if_running(vec![item]).await.is_err() {
            tracing::debug!("skipping goal steering because no turn is active");
        }
    }

    pub(crate) async fn account_active_goal_progress(
        &self,
        turn_id: &str,
        event_id: &str,
        mode: codex_state::GoalAccountingMode,
        budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
    ) -> Result<Option<AccountedGoalProgress>, String> {
        let accounting = self.accounting_state();
        let _accounting_permit = accounting
            .progress_accounting_permit()
            .await
            .map_err(|err| err.to_string())?;
        let Some(snapshot) = accounting.progress_snapshot(turn_id) else {
            return Ok(None);
        };
        let previous_status = self
            .current_goal_status_for_metrics(Some(snapshot.expected_goal_id.as_str()))
            .await?;
        let outcome = self
            .inner
            .state_dbs
            .thread_goals()
            .account_thread_goal_usage(
                self.thread_id(),
                snapshot.time_delta_seconds,
                snapshot.token_delta,
                mode,
                Some(snapshot.expected_goal_id.as_str()),
            )
            .await
            .map_err(|err| err.to_string())?;
        Ok(match outcome {
            codex_state::GoalAccountingOutcome::Updated(goal) => {
                let goal_id = goal.goal_id.clone();
                self.inner
                    .metrics
                    .record_terminal_if_status_changed(previous_status, &goal);
                self.inner
                    .analytics
                    .usage_accounted(&goal, GoalEventAttribution::Turn(turn_id));
                self.inner.analytics.status_changed(
                    &goal,
                    previous_status,
                    GoalEventAttribution::Turn(turn_id),
                );
                accounting.mark_progress_accounted_for_status(
                    turn_id,
                    &snapshot,
                    goal.status,
                    budget_limited_goal_disposition,
                );
                let goal = protocol_goal_from_state(goal);
                self.inner.event_emitter.thread_goal_updated(
                    event_id.to_string(),
                    Some(turn_id.to_string()),
                    goal.clone(),
                );
                Some(AccountedGoalProgress { goal, goal_id })
            }
            codex_state::GoalAccountingOutcome::Unchanged(_) => None,
        })
    }

    async fn account_idle_goal_progress(
        &self,
        event_id: &str,
        mode: codex_state::GoalAccountingMode,
        budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
    ) -> Result<Option<AccountedGoalProgress>, String> {
        let accounting = self.accounting_state();
        let _accounting_permit = accounting
            .progress_accounting_permit()
            .await
            .map_err(|err| err.to_string())?;
        let Some(snapshot) = accounting.idle_progress_snapshot() else {
            return Ok(None);
        };
        let previous_status = self
            .current_goal_status_for_metrics(Some(snapshot.expected_goal_id.as_str()))
            .await?;
        let outcome = self
            .inner
            .state_dbs
            .thread_goals()
            .account_thread_goal_usage(
                self.thread_id(),
                snapshot.time_delta_seconds,
                snapshot.token_delta,
                mode,
                Some(snapshot.expected_goal_id.as_str()),
            )
            .await
            .map_err(|err| err.to_string())?;
        Ok(match outcome {
            codex_state::GoalAccountingOutcome::Updated(goal) => {
                let goal_id = goal.goal_id.clone();
                self.inner
                    .metrics
                    .record_terminal_if_status_changed(previous_status, &goal);
                self.inner
                    .analytics
                    .usage_accounted(&goal, GoalEventAttribution::NoTurn);
                self.inner.analytics.status_changed(
                    &goal,
                    previous_status,
                    GoalEventAttribution::NoTurn,
                );
                accounting.mark_idle_progress_accounted_for_status(
                    &snapshot,
                    goal.status,
                    budget_limited_goal_disposition,
                );
                let goal = protocol_goal_from_state(goal);
                self.inner.event_emitter.thread_goal_updated(
                    event_id.to_string(),
                    /*turn_id*/ None,
                    goal.clone(),
                );
                Some(AccountedGoalProgress { goal, goal_id })
            }
            codex_state::GoalAccountingOutcome::Unchanged(_) => {
                accounting.reset_idle_progress_baseline_and_clear_active_goal();
                None
            }
        })
    }

    async fn current_goal_status_for_metrics(
        &self,
        expected_goal_id: Option<&str>,
    ) -> Result<Option<codex_state::ThreadGoalStatus>, String> {
        let goal = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?;
        Ok(goal.and_then(|goal| {
            expected_goal_id
                .is_none_or(|expected_goal_id| goal.goal_id == expected_goal_id)
                .then_some(goal.status)
        }))
    }
}
