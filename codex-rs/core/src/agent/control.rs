use crate::TurnInputRequest;
use crate::TurnInputSubmission;
use crate::TurnStartOptions;
use crate::agent::AgentStatus;
use crate::agent::registry::AgentMetadata;
use crate::agent::registry::AgentRegistry;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::resolve_role_config;
use crate::agent::status::is_final;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use crate::config::RolloutBudgetConfig;
use crate::context::SubagentNotification;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::rollout_budget::RolloutBudget;
use crate::session::emit_subagent_session_started;
use crate::session::multi_agents::ResolvedMultiAgentV2UsageHints;
use crate::session_prefix::format_inter_agent_completion_message;
use crate::session_prefix::format_subagent_context_line;
use crate::thread_manager::ResumeThreadWithHistoryOptions;
use crate::thread_manager::ThreadIdGenerator;
use crate::thread_manager::ThreadManagerState;
use crate::thread_manager::default_thread_id_generator;
use crate::thread_rollout_truncation::truncate_rollout_to_last_n_fork_turns;
use crate::turn_timing::now_unix_timestamp_ms;
use arc_swap::ArcSwap;
use arc_swap::ArcSwapOption;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::ThreadUsagePolicy;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::turn_input::CyberAccessProgram;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ReadThreadParams;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::Weak;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::watch;
use tracing::warn;
use uuid::Uuid;

pub(crate) use self::execution::AgentExecutionGuard;
pub use self::handoff::HandoffAdmissionGuard;
pub use self::handoff::HandoffGuard;
use self::execution::AgentExecutionLimiter;
use self::residency::V2Residency;
pub(crate) use self::worker_limit::TeamWorkerLease;
use self::worker_limit::TeamWorkerLimiter;

mod activity;
mod execution;
mod handoff;
mod legacy;
mod residency;
mod service_tier;
mod spawn;
mod usage_policy;
mod user_authorization;
mod worker_handoff;
mod worker_limit;

const MAX_ENVIRONMENT_SUBAGENTS: usize = 8;
const MAX_ENVIRONMENT_SUBAGENT_BYTES: usize = 1_024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SpawnAgentForkMode {
    FullHistory,
    LastNTurns(usize),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SpawnAgentOptions {
    pub(crate) fork_parent_spawn_call_id: Option<String>,
    pub(crate) fork_mode: Option<SpawnAgentForkMode>,
    pub(crate) initial_collaboration_mode: Option<CollaborationMode>,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) parent_turn_id: Option<String>,
    pub(crate) root_turn_id: Option<String>,
    pub(crate) environments: Option<Vec<TurnEnvironmentSelection>>,
    pub(crate) multi_agent_v2_usage_hints: Option<ResolvedMultiAgentV2UsageHints>,
    pub(crate) cyber_access_program: Option<CyberAccessProgram>,
}

#[derive(Clone, Debug)]
pub(crate) struct LiveAgent {
    pub(crate) thread_id: ThreadId,
    pub(crate) metadata: AgentMetadata,
    pub(crate) status: AgentStatus,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ListedAgent {
    pub(crate) agent_name: String,
    pub(crate) agent_status: AgentStatus,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PruneIdleAgentsReport {
    pub(crate) closed: Vec<ThreadId>,
    pub(crate) failed: Vec<(ThreadId, String)>,
}

/// Control-plane handle for multi-agent operations.
/// `AgentControl` is held by each session (via `SessionServices`). It provides capability to
/// spawn new agents and the inter-agent communication layer.
/// An `AgentControl` instance is intended to be created at most once per root thread/session
/// tree. That same `AgentControl` is then shared with every sub-agent spawned from that root,
/// which keeps the registry scoped to that root thread rather than the entire `ThreadManager`.
#[derive(Clone)]
pub(crate) struct AgentControl {
    /// session_id is equal to the root thread's ID.
    session_id: SessionId,
    /// Weak handle back to the global thread registry/state.
    /// This is `Weak` to avoid reference cycles and shadow persistence of the form
    /// `ThreadManagerState -> CodexThread -> Session -> SessionServices -> ThreadManagerState`.
    manager: Weak<ThreadManagerState>,
    /// Captured at construction so delegates retain their manager's allocation policy.
    thread_id_generator: ThreadIdGenerator,
    state: Arc<AgentRegistry>,
    v2_residency: Arc<V2Residency>,
    agent_execution_limiter: Arc<AgentExecutionLimiter>,
    team_worker_limiter: Arc<TeamWorkerLimiter>,
    /// Session-scoped state shared by the root thread and every cloned sub-agent control handle.
    rollout_budget: Arc<RolloutBudget>,
    /// The user-selected root routing tier, shared by the entire agent tree.
    root_service_tier: Arc<ArcSwapOption<String>>,
    /// The complete root usage policy, shared by the entire agent tree.
    root_usage_policy: Arc<ArcSwap<ThreadUsagePolicy>>,
    /// Serializes root usage-toggle commits with descendant synchronization.
    root_usage_auto_resume_update: Arc<Mutex<()>>,
    /// Serializes settings events sent while the root usage toggle changes.
    root_usage_auto_resume_propagation: Arc<Mutex<()>>,
    /// Root-scoped process-local manual pause switch shared by every loaded descendant.
    root_activity_paused: Arc<std::sync::atomic::AtomicBool>,
    /// Root-scoped process-local fence that rejects new work during daemon handoff.
    pub(crate) handoff_admission_sealed: Arc<AtomicBool>,
    /// Number of admissions that passed the handoff fence before it sealed.
    pub(crate) handoff_admission_in_flight: Arc<AtomicU32>,
    /// Wakes the handoff coordinator after a pre-seal admission finishes.
    pub(crate) handoff_admission_notify: Arc<Notify>,
    /// Serializes manual pause publication with child startup reconciliation.
    root_activity_pause_update: Arc<Mutex<()>>,
    /// Serializes descendant activity state propagation and preserves toggle order.
    root_activity_pause_propagation: Arc<Mutex<()>>,
    /// Wakes retained turns when the root activity pause is released.
    root_activity_resume_notify: Arc<Notify>,
    /// Serializes root tier commits with descendant synchronization.
    root_service_tier_update: Arc<Mutex<()>>,
    /// Serializes settings events sent while a root routing tier changes, preserving toggle order.
    root_service_tier_propagation: Arc<Mutex<()>>,
}

impl Default for AgentControl {
    fn default() -> Self {
        Self::new(
            Weak::default(),
            default_thread_id_generator(),
            /*rollout_budget*/ None,
        )
    }
}

impl AgentControl {
    /// Construct a new `AgentControl` that can spawn/message agents via the given manager state.
    pub(crate) fn new(
        manager: Weak<ThreadManagerState>,
        thread_id_generator: ThreadIdGenerator,
        rollout_budget: Option<RolloutBudgetConfig>,
    ) -> Self {
        let control = Self {
            session_id: SessionId::default(),
            manager,
            thread_id_generator,
            state: Arc::default(),
            v2_residency: Arc::default(),
            agent_execution_limiter: Arc::default(),
            team_worker_limiter: Arc::default(),
            rollout_budget: Arc::default(),
            root_service_tier: Arc::new(ArcSwapOption::from(None)),
            root_usage_policy: Arc::new(ArcSwap::from_pointee(ThreadUsagePolicy::default())),
            root_usage_auto_resume_update: Arc::new(Mutex::new(())),
            root_usage_auto_resume_propagation: Arc::new(Mutex::new(())),
            root_activity_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            handoff_admission_sealed: Arc::new(AtomicBool::new(false)),
            handoff_admission_in_flight: Arc::new(AtomicU32::new(0)),
            handoff_admission_notify: Arc::new(Notify::new()),
            root_activity_pause_update: Arc::new(Mutex::new(())),
            root_activity_pause_propagation: Arc::new(Mutex::new(())),
            root_activity_resume_notify: Arc::new(Notify::new()),
            root_service_tier_update: Arc::new(Mutex::new(())),
            root_service_tier_propagation: Arc::new(Mutex::new(())),
        };
        if let Some(rollout_budget) = rollout_budget {
            control.rollout_budget.configure(rollout_budget);
        }
        control
    }

    pub(crate) fn with_session_id(
        mut self,
        session_id: SessionId,
        max_threads: usize,
        team_worker_max_concurrent: Option<usize>,
    ) -> Self {
        self.session_id = session_id;
        self.agent_execution_limiter.initialize(max_threads);
        self.team_worker_limiter
            .initialize(team_worker_max_concurrent);
        self
    }

    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn generate_thread_id(&self) -> ThreadId {
        (self.thread_id_generator)()
    }

    pub(crate) fn rollout_budget(&self) -> &RolloutBudget {
        self.rollout_budget.as_ref()
    }

    /// Send rich user input items to an existing agent thread.
    pub(crate) async fn send_input(
        &self,
        agent_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let thread = state.get_thread(agent_id).await?;
        let result = match thread
            .start_or_steer_turn(TurnInputRequest::user_input(input).on_start(start_options))
            .await
        {
            Ok(TurnInputSubmission::Started { turn_id }) => Ok(turn_id),
            Ok(TurnInputSubmission::Steered { .. }) => {
                // MAv1 exposes an opaque `submission_id` to the model. The legacy
                // `Op::UserInput` path returned a fresh ID for every steer, while the
                // turn-input API returns the active turn ID. Keep the tool-visible ID
                // unique without adding a submission receipt back to Core.
                Ok(Uuid::now_v7().to_string())
            }
            Ok(TurnInputSubmission::NotSubmitted { reason }) => Err(CodexErr::InvalidRequest(
                format!("turn input was not submitted: {reason:?}"),
            )),
            Err(err) => Err(err),
        };
        self.handle_thread_request_result(agent_id, &state, result)
            .await
    }

    pub(crate) async fn send_inter_agent_communication(
        &self,
        agent_id: ThreadId,
        communication: InterAgentCommunication,
        agent_communication_context: AgentCommunicationContext,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        self.send_inter_agent_communication_with_delivery_kind(
            agent_id,
            communication,
            agent_communication_context,
            start_options,
            /*team_lead_completion*/ false,
        )
        .await
    }

    /// Delivers a terminal Worker result that was admitted while its parent owned the Lead role.
    /// The process-local delivery kind lets the recipient discard a stale trigger if Team mode
    /// is disabled before the queued operation reaches its session handler.
    pub(crate) async fn send_team_lead_completion(
        &self,
        agent_id: ThreadId,
        communication: InterAgentCommunication,
        agent_communication_context: AgentCommunicationContext,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        self.send_inter_agent_communication_with_delivery_kind(
            agent_id,
            communication,
            agent_communication_context,
            start_options,
            /*team_lead_completion*/ true,
        )
        .await
    }

    async fn send_inter_agent_communication_with_delivery_kind(
        &self,
        agent_id: ThreadId,
        communication: InterAgentCommunication,
        agent_communication_context: AgentCommunicationContext,
        start_options: TurnStartOptions,
        team_lead_completion: bool,
    ) -> CodexResult<String> {
        // Queue-only mailbox mail is process-local too; admit its enqueue before a handoff seal
        // so a late completion cannot be silently lost when the writer is replaced.
        let _admission = match self.begin_handoff_admission() {
            Ok(admission) => admission,
            Err(err) => {
                if let Ok(state) = self.upgrade()
                    && let Ok(thread) = state.get_thread(agent_id).await
                {
                    if team_lead_completion {
                        thread
                            .session
                            .input_queue
                            .enqueue_team_lead_mailbox_communication(communication, start_options)
                            .await;
                    } else {
                        thread
                            .session
                            .input_queue
                            .enqueue_mailbox_communication(communication, start_options)
                            .await;
                    }
                    tracing::debug!(
                        agent_id = %agent_id,
                        "retained inter-agent message after handoff admission was sealed"
                    );
                }
                return Err(err);
            }
        };
        let state = self.upgrade()?;

        let _team_worker_lease = if communication.trigger_turn {
            let thread = state.get_thread(agent_id).await?;
            self.ensure_execution_capacity_for_turn_start(&thread)
                .await?;
            let config = thread.session.get_config().await;
            self.reserve_team_worker_turn(&config, &thread.session_source, agent_id)?
        } else {
            None
        };

        self.send_inter_agent_communication_after_capacity_check(
            agent_id,
            &state,
            communication,
            agent_communication_context,
            start_options,
            team_lead_completion,
        )
        .await
    }

    pub(crate) async fn emit_sub_agent_activity(
        &self,
        thread_id: ThreadId,
        turn_id: String,
        item: SubAgentActivityItem,
    ) -> CodexResult<()> {
        let _admission = self.begin_handoff_admission()?;
        let state = self.upgrade()?;
        let thread = state.get_thread(thread_id).await?;
        let started_at_ms = now_unix_timestamp_ms();
        let item = TurnItem::SubAgentActivity(item);
        thread
            .session
            .send_event_raw(Event {
                id: turn_id.clone(),
                msg: EventMsg::ItemStarted(ItemStartedEvent {
                    thread_id,
                    turn_id: turn_id.clone(),
                    item: item.clone(),
                    started_at_ms,
                }),
            })
            .await;
        let completed_at_ms = now_unix_timestamp_ms();
        let completed = ItemCompletedEvent {
            thread_id,
            turn_id: turn_id.clone(),
            item,
            started_at_ms: Some(started_at_ms),
            completed_at_ms,
        };
        thread
            .session
            .send_event_raw(Event {
                id: turn_id.clone(),
                msg: EventMsg::ItemCompleted(completed.clone()),
            })
            .await;
        for legacy in completed.as_legacy_events(/*show_raw_agent_reasoning*/ false) {
            thread
                .session
                .send_event_raw(Event {
                    id: turn_id.clone(),
                    msg: legacy,
                })
                .await;
        }
        Ok(())
    }

    async fn send_inter_agent_communication_after_capacity_check(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        start_options: TurnStartOptions,
        team_lead_completion: bool,
    ) -> CodexResult<String> {
        self.submit_inter_agent_communication(
            agent_id,
            state,
            communication,
            context,
            start_options,
            team_lead_completion,
        )
        .await
    }

    async fn submit_inter_agent_communication(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        start_options: TurnStartOptions,
        team_lead_completion: bool,
    ) -> CodexResult<String> {
        let communication_for_log =
            crate::agent_communication::logging_enabled().then(|| communication.clone());
        let (parent_turn_id, root_turn_id) = if communication.trigger_turn {
            (
                start_options.parent_turn_id.clone(),
                start_options.root_turn_id.clone(),
            )
        } else {
            (None, None)
        };
        let result = self
            .handle_thread_request_result(
                agent_id,
                state,
                state
                    .send_op(
                        agent_id,
                        if team_lead_completion {
                            Op::TeamLeadCompletion {
                                communication,
                                start_options,
                            }
                        } else {
                            Op::InterAgentCommunication {
                                communication,
                                start_options,
                            }
                        },
                        parent_turn_id,
                        root_turn_id,
                    )
                    .await,
            )
            .await;
        if let (Some(communication), Ok(communication_id)) =
            (communication_for_log, result.as_ref())
        {
            crate::agent_communication::emit_agent_communication_send(
                communication_id,
                &context,
                &communication,
                agent_id,
            );
        }
        result
    }

    /// Interrupt the current task for an existing agent thread.
    pub(crate) async fn interrupt_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        self.handle_thread_request_result(
            agent_id,
            &state,
            state
                .send_op(
                    agent_id,
                    Op::Interrupt,
                    /*parent_turn_id*/ None,
                    /*root_turn_id*/ None,
                )
                .await,
        )
        .await
    }

    async fn handle_thread_request_result(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        result: CodexResult<String>,
    ) -> CodexResult<String> {
        if result
            .as_ref()
            .is_err_and(|err| matches!(err.details(), CodexErrorDetails::InternalAgentDied))
        {
            let _ = state.remove_thread(&agent_id).await;
            self.forget_v2_residency(agent_id);
            self.state.release_spawned_thread(agent_id);
        }
        result
    }

    /// Fetch the last known status for `agent_id`, returning `NotFound` when unavailable.
    pub(crate) async fn get_status(&self, agent_id: ThreadId) -> AgentStatus {
        let Ok(state) = self.upgrade() else {
            // No agent available if upgrade fails.
            return AgentStatus::NotFound;
        };
        let Ok(thread) = state.get_thread(agent_id).await else {
            return AgentStatus::NotFound;
        };
        thread.agent_status().await
    }

    /// Counts direct Worker children that can still perform work for a parent session.
    /// Interrupted and terminal children do not keep a Lead parked.
    pub(crate) async fn active_direct_worker_count(&self, parent_thread_id: ThreadId) -> usize {
        let Ok(children) = self.open_thread_spawn_children(parent_thread_id).await else {
            return 0;
        };
        let mut active = 0;
        for (thread_id, _) in children {
            if matches!(
                self.get_status(thread_id).await,
                AgentStatus::PendingInit | AgentStatus::Running
            ) {
                active += 1;
            }
        }
        active
    }

    /// Subscribes to status changes for direct Workers that can still perform work. The boolean
    /// reports a child that was already terminal or disappeared while the watchers were built.
    pub(crate) async fn direct_worker_status_watchers(
        &self,
        parent_thread_id: ThreadId,
    ) -> (Vec<watch::Receiver<AgentStatus>>, bool) {
        let Ok(children) = self.open_thread_spawn_children(parent_thread_id).await else {
            return (Vec::new(), true);
        };
        let mut watchers = Vec::new();
        let mut status_changed = false;
        for (thread_id, _) in children {
            let status = self.get_status(thread_id).await;
            if is_final(&status) || matches!(status, AgentStatus::Interrupted) {
                status_changed = true;
                continue;
            }
            match self.subscribe_status(thread_id).await {
                Ok(receiver) => watchers.push(receiver),
                Err(_) => status_changed = true,
            }
        }
        (watchers, status_changed)
    }

    /// Returns whether a target thread currently has the Lead assignment. This is used by the
    /// communication path to classify root-directed Worker progress without changing non-team
    /// delivery semantics.
    pub(crate) async fn parent_is_team_lead(&self, thread_id: ThreadId) -> bool {
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let Ok(thread) = state.get_thread(thread_id).await else {
            return false;
        };
        let config = thread.session.get_config().await;
        let source = thread.session.session_source().await;
        config.team_mode == codex_protocol::protocol::TeamMode::LeadWorker
            && crate::session::team::effective_role_for_session_source(&config, &source)
                == Some(codex_config::TeamRole::Lead)
    }

    pub(crate) fn register_session_root(
        &self,
        current_thread_id: ThreadId,
        current_parent_thread_id: Option<ThreadId>,
    ) {
        if current_parent_thread_id.is_none() {
            self.state.register_root_thread(current_thread_id);
        }
    }

    pub(crate) fn get_agent_metadata(&self, agent_id: ThreadId) -> Option<AgentMetadata> {
        self.state.agent_metadata_for_thread(agent_id)
    }

    pub(crate) fn ensure_agent_known(&self, agent_id: ThreadId) -> CodexResult<AgentMetadata> {
        self.state
            .agent_metadata_for_thread(agent_id)
            .ok_or_else(|| CodexErr::ThreadNotFound(agent_id))
    }

    pub(crate) async fn list_live_agent_subtree_thread_ids(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        let mut thread_ids = vec![agent_id];
        thread_ids.extend(self.live_thread_spawn_descendants(agent_id).await?);
        Ok(thread_ids)
    }

    pub(crate) async fn get_agent_config_snapshot(
        &self,
        agent_id: ThreadId,
    ) -> Option<ThreadConfigSnapshot> {
        let Ok(state) = self.upgrade() else {
            return None;
        };
        let Ok(thread) = state.get_thread(agent_id).await else {
            return None;
        };
        Some(thread.config_snapshot().await)
    }

    pub(crate) async fn resolve_agent_reference(
        &self,
        _current_thread_id: ThreadId,
        current_session_source: &SessionSource,
        agent_reference: &str,
    ) -> CodexResult<ThreadId> {
        let current_agent_path = current_session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root);
        let agent_path = current_agent_path
            .resolve(agent_reference)
            .map_err(CodexErr::UnsupportedOperation)?;
        if let Some(thread_id) = self.state.agent_id_for_path(&agent_path) {
            return Ok(thread_id);
        }
        Err(CodexErr::UnsupportedOperation(format!(
            "live agent path `{}` not found",
            agent_path.as_str()
        )))
    }

    /// Subscribe to status updates for `agent_id`, yielding the latest value and changes.
    pub(crate) async fn subscribe_status(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<watch::Receiver<AgentStatus>> {
        let state = self.upgrade()?;
        let thread = state.get_thread(agent_id).await?;
        Ok(thread.subscribe_status())
    }

    pub(crate) async fn format_environment_context_subagents(
        &self,
        parent_thread_id: ThreadId,
        multi_agent_version: MultiAgentVersion,
    ) -> String {
        if multi_agent_version != MultiAgentVersion::V2 {
            let Ok(agents) = self.open_thread_spawn_children(parent_thread_id).await else {
                return String::new();
            };
            return agents
                .into_iter()
                .map(|(thread_id, metadata)| {
                    let reference = metadata
                        .agent_path
                        .as_ref()
                        .map(|path| path.name().to_string())
                        .unwrap_or_else(|| thread_id.to_string());
                    format_subagent_context_line(&reference, metadata.agent_nickname.as_deref())
                })
                .collect::<Vec<_>>()
                .join("\n");
        }

        let Some(parent_path) = self
            .state
            .agent_metadata_for_thread(parent_thread_id)
            .and_then(|metadata| metadata.agent_path)
        else {
            return String::new();
        };
        let parent_prefix = format!("{parent_path}/");
        let mut agent_paths = self
            .state
            .live_agents()
            .into_iter()
            .filter_map(|metadata| metadata.agent_path)
            .filter(|path| {
                path.as_str()
                    .strip_prefix(&parent_prefix)
                    .is_some_and(|name| !name.contains('/'))
            })
            .collect::<Vec<_>>();
        let loaded_paths = self
            .open_thread_spawn_children(parent_thread_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, metadata)| metadata.agent_path)
            .collect::<HashSet<_>>();
        agent_paths.sort();
        // Stable sorting preserves alphabetical order within each group.
        agent_paths.sort_by_key(|path| !loaded_paths.contains(path));

        let mut lines = Vec::with_capacity(agent_paths.len().min(MAX_ENVIRONMENT_SUBAGENTS));
        let mut rendered_bytes = "  <subagents>\n  </subagents>\n".len();
        for agent_path in agent_paths {
            if lines.len() == MAX_ENVIRONMENT_SUBAGENTS {
                break;
            }
            let line = format!(r#"<agent name="{agent_path}" />"#);
            let line_bytes = "    \n".len() + line.len();
            if rendered_bytes + line_bytes <= MAX_ENVIRONMENT_SUBAGENT_BYTES {
                rendered_bytes += line_bytes;
                lines.push(line);
            }
        }
        lines.join("\n")
    }

    pub(crate) async fn list_agents(
        &self,
        current_session_source: &SessionSource,
        path_prefix: Option<&str>,
    ) -> CodexResult<Vec<ListedAgent>> {
        let state = self.upgrade()?;
        let resolved_prefix = path_prefix
            .map(|prefix| {
                current_session_source
                    .get_agent_path()
                    .unwrap_or_else(AgentPath::root)
                    .resolve(prefix)
                    .map_err(CodexErr::UnsupportedOperation)
            })
            .transpose()?;

        let mut live_agents = self.state.live_agents();
        live_agents.sort_by(|left, right| {
            left.agent_path
                .as_deref()
                .unwrap_or_default()
                .cmp(right.agent_path.as_deref().unwrap_or_default())
                .then_with(|| {
                    left.agent_id
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                        .cmp(&right.agent_id.map(|id| id.to_string()).unwrap_or_default())
                })
        });

        let root_path = AgentPath::root();
        let mut agents = Vec::with_capacity(live_agents.len().saturating_add(1));
        if resolved_prefix
            .as_ref()
            .is_none_or(|prefix| agent_matches_prefix(Some(&root_path), prefix))
            && let Some(root_thread_id) = self.state.agent_id_for_path(&root_path)
            && let Ok(root_thread) = state.get_thread(root_thread_id).await
        {
            agents.push(ListedAgent {
                agent_name: root_path.to_string(),
                agent_status: root_thread.agent_status().await,
            });
        }

        for metadata in live_agents {
            let Some(thread_id) = metadata.agent_id else {
                continue;
            };
            if resolved_prefix
                .as_ref()
                .is_some_and(|prefix| !agent_matches_prefix(metadata.agent_path.as_ref(), prefix))
            {
                continue;
            }

            let Ok(thread) = state.get_thread(thread_id).await else {
                continue;
            };
            let agent_name = metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| thread_id.to_string());
            agents.push(ListedAgent {
                agent_name,
                agent_status: thread.agent_status().await,
            });
        }

        Ok(agents)
    }

    pub(crate) async fn prune_idle_agents(
        &self,
        current_thread_id: ThreadId,
    ) -> CodexResult<PruneIdleAgentsReport> {
        let _admission = self.begin_handoff_admission()?;
        let mut children_by_parent = self.live_thread_spawn_children().await?;
        for children in children_by_parent.values_mut() {
            children.sort_by_key(|left| left.0.to_string());
        }
        let mut parent_by_child = HashMap::new();
        for (parent_thread_id, children) in &children_by_parent {
            for (child_thread_id, _) in children {
                parent_by_child.insert(*child_thread_id, *parent_thread_id);
            }
        }

        let mut session_root_thread_id = current_thread_id;
        let mut visited_ancestors = HashSet::new();
        while let Some(parent_thread_id) = parent_by_child.get(&session_root_thread_id).copied() {
            if !visited_ancestors.insert(parent_thread_id) {
                break;
            }
            session_root_thread_id = parent_thread_id;
        }

        let mut session_thread_ids = HashSet::from([session_root_thread_id]);
        session_thread_ids.extend(collect_descendants(
            session_root_thread_id,
            &children_by_parent,
        ));

        let mut thread_spawn_depths = HashMap::new();
        let mut depth_queue = VecDeque::from([(session_root_thread_id, 0usize)]);
        while let Some((thread_id, depth)) = depth_queue.pop_front() {
            if thread_spawn_depths.insert(thread_id, depth).is_some() {
                continue;
            }
            if let Some(children) = children_by_parent.get(&thread_id) {
                for (child_thread_id, _) in children {
                    depth_queue.push_back((*child_thread_id, depth.saturating_add(1)));
                }
            }
        }

        let mut live_agents = self.state.live_agents();
        let mut live_agent_ids = HashSet::new();
        live_agents.retain(|metadata| {
            metadata
                .agent_id
                .is_some_and(|thread_id| live_agent_ids.insert(thread_id))
        });
        for children in children_by_parent.values() {
            for (thread_id, metadata) in children {
                if !session_thread_ids.contains(thread_id) || !live_agent_ids.insert(*thread_id) {
                    continue;
                }
                let mut metadata = metadata.clone();
                metadata.agent_id = Some(*thread_id);
                live_agents.push(metadata);
            }
        }
        live_agents.sort_by(|left, right| {
            let left_depth = left
                .agent_id
                .and_then(|thread_id| thread_spawn_depths.get(&thread_id).copied())
                .unwrap_or(usize::MAX);
            let right_depth = right
                .agent_id
                .and_then(|thread_id| thread_spawn_depths.get(&thread_id).copied())
                .unwrap_or(usize::MAX);
            left_depth
                .cmp(&right_depth)
                .then_with(|| {
                    left.agent_path
                        .as_deref()
                        .unwrap_or_default()
                        .cmp(right.agent_path.as_deref().unwrap_or_default())
                })
                .then_with(|| {
                    left.agent_id
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                        .cmp(&right.agent_id.map(|id| id.to_string()).unwrap_or_default())
                })
        });

        let protected = HashSet::from([current_thread_id]);
        let mut handled = HashSet::new();
        let mut report = PruneIdleAgentsReport::default();

        for metadata in live_agents {
            let Some(thread_id) = metadata.agent_id else {
                continue;
            };
            if handled.contains(&thread_id) {
                continue;
            }

            let descendants = collect_descendants(thread_id, &children_by_parent);
            if std::iter::once(thread_id)
                .chain(descendants.iter().copied())
                .any(|candidate| protected.contains(&candidate))
            {
                continue;
            }

            let mut subtree_contains_active_work = false;
            for candidate in std::iter::once(thread_id).chain(descendants.iter().copied()) {
                if matches!(
                    self.get_status(candidate).await,
                    AgentStatus::PendingInit | AgentStatus::Running
                ) {
                    subtree_contains_active_work = true;
                    break;
                }
            }
            if subtree_contains_active_work {
                continue;
            }

            let mut subtree = vec![thread_id];
            subtree.extend(descendants);
            let unhandled_subtree = subtree
                .into_iter()
                .filter(|candidate| !handled.contains(candidate))
                .collect::<Vec<_>>();
            match self.close_agent(thread_id).await {
                Ok(_) => {
                    handled.extend(unhandled_subtree.iter().copied());
                    report.closed.extend(unhandled_subtree);
                }
                Err(err) => {
                    if matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) {
                        handled.extend(unhandled_subtree.iter().copied());
                        report.closed.extend(unhandled_subtree);
                    } else {
                        handled.insert(thread_id);
                        report.failed.push((thread_id, err.to_string()));
                    }
                }
            }
        }

        report.closed.sort_by_key(std::string::ToString::to_string);
        report
            .failed
            .sort_by_key(|(thread_id, _)| thread_id.to_string());
        Ok(report)
    }

    /// Starts a detached watcher for sub-agents spawned from another thread.
    ///
    /// This is only enabled for `SubAgentSource::ThreadSpawn`, where a parent thread exists and
    /// can receive completion notifications.
    fn maybe_start_completion_watcher(
        &self,
        child_thread_id: ThreadId,
        session_source: Option<SessionSource>,
        child_reference: String,
        child_agent_path: Option<AgentPath>,
    ) {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return;
        };
        let control = self.clone();
        tokio::spawn(async move {
            let status = match control.subscribe_status(child_thread_id).await {
                Ok(mut status_rx) => {
                    let mut status = status_rx.borrow().clone();
                    while !is_final(&status) {
                        if status_rx.changed().await.is_err() {
                            status = control.get_status(child_thread_id).await;
                            break;
                        }
                        status = status_rx.borrow().clone();
                    }
                    status
                }
                Err(_) => control.get_status(child_thread_id).await,
            };
            if !is_final(&status) {
                return;
            }

            let Ok(state) = control.upgrade() else {
                return;
            };
            let child_thread = state.get_thread(child_thread_id).await.ok();
            let child_uses_multi_agent_v2 = match child_thread.as_ref() {
                Some(child_thread) => {
                    child_thread.multi_agent_version() == Some(MultiAgentVersion::V2)
                }
                None => true,
            };
            if child_agent_path.is_some() && child_uses_multi_agent_v2 {
                let Some(child_agent_path) = child_agent_path.clone() else {
                    return;
                };
                let Some(parent_agent_path) = child_agent_path
                    .as_str()
                    .rsplit_once('/')
                    .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
                else {
                    return;
                };
                let Some(message) = format_inter_agent_completion_message(
                    parent_agent_path.clone(),
                    child_agent_path.clone(),
                    &status,
                ) else {
                    return;
                };
                let trigger_turn = control.parent_is_team_lead(parent_thread_id).await;
                let communication = InterAgentCommunication::new(
                    child_agent_path,
                    parent_agent_path,
                    Vec::new(),
                    message,
                    trigger_turn,
                );
                let context =
                    AgentCommunicationContext::new(AgentCommunicationKind::Result, child_thread_id);
                let _ = if trigger_turn {
                    control
                        .send_team_lead_completion(
                            parent_thread_id,
                            communication,
                            context,
                            TurnStartOptions::default(),
                        )
                        .await
                } else {
                    control
                        .send_inter_agent_communication(
                            parent_thread_id,
                            communication,
                            context,
                            TurnStartOptions::default(),
                        )
                        .await
                };
                return;
            }
            let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
                return;
            };
            if control.parent_is_team_lead(parent_thread_id).await {
                // Legacy V1 workers report completion through a context fragment rather than an
                // InterAgentCommunication. A parked Team Lead still needs an actionable wake for
                // that terminal result, so route it through the same bounded wake path used by
                // V2 completion and deadline events.
                if !parent_thread.session.is_team_lead().await {
                    return;
                }
                // The legacy completion path mutates the parent's rollout before waking its
                // Lead. Keep that fragment, cancellation, and wake as one admitted handoff
                // operation so a sealing coordinator cannot close the writer between them.
                let Ok(_handoff_admission) = parent_thread
                    .session
                    .services
                    .agent_control
                    .begin_handoff_admission()
                else {
                    Self::retain_legacy_completion_after_handoff(
                        &parent_thread,
                        &child_reference,
                        child_agent_path.as_ref(),
                        &status,
                        /*trigger_turn*/ true,
                    )
                    .await;
                    return;
                };
                parent_thread
                    .inject_fragment_without_turn(SubagentNotification::new(
                        child_reference.as_str(),
                        status.clone(),
                    ))
                    .await;
                parent_thread.session.cancel_lead_oversight().await;
                parent_thread
                    .session
                    .enqueue_lead_wakeup(&format!(
                        "Worker {child_reference} completed with status {status:?}; review the result."
                    ))
                    .await;
                parent_thread
                    .session
                    .maybe_start_turn_for_pending_work()
                    .await;
                return;
            }
            let Ok(_handoff_admission) = parent_thread
                .session
                .services
                .agent_control
                .begin_handoff_admission()
            else {
                Self::retain_legacy_completion_after_handoff(
                    &parent_thread,
                    &child_reference,
                    child_agent_path.as_ref(),
                    &status,
                    /*trigger_turn*/ false,
                )
                .await;
                return;
            };
            parent_thread
                .inject_fragment_without_turn(SubagentNotification::new(
                    child_reference.as_str(),
                    status,
                ))
                .await;
        });
    }

    /// Retains a V1 completion as mailbox mail when the parent handoff fence already won.
    ///
    /// V1 normally writes a context fragment directly to the parent rollout. That fragment has no
    /// durable callback identity, so a sealed owner cannot safely append it. Queue a bounded
    /// completion message instead; the old owner can deliver it after an aborted handoff and the
    /// replacement coordinator will see the mailbox as a blocker.
    async fn retain_legacy_completion_after_handoff(
        parent_thread: &Arc<CodexThread>,
        child_reference: &str,
        child_agent_path: Option<&AgentPath>,
        status: &AgentStatus,
        trigger_turn: bool,
    ) {
        let author = child_agent_path
            .cloned()
            .or_else(|| AgentPath::try_from(child_reference).ok())
            .unwrap_or_else(AgentPath::root);
        let recipient = author
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
            .unwrap_or_else(AgentPath::root);
        let communication = InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            format!("Worker {child_reference} completed with status {status:?}; review the result."),
            trigger_turn,
        );
        if trigger_turn {
            parent_thread
                .session
                .input_queue
                .enqueue_team_lead_mailbox_communication(
                    communication,
                    TurnStartOptions::default(),
                )
                .await;
        } else {
            parent_thread
                .session
                .input_queue
                .enqueue_mailbox_communication(communication, TurnStartOptions::default())
                .await;
        }
    }

    fn prepare_agent_metadata(
        &self,
        reservation: &mut crate::agent::registry::SpawnReservation,
        config: &Config,
        agent_path: Option<AgentPath>,
        agent_role: Option<String>,
        preferred_agent_nickname: Option<String>,
    ) -> CodexResult<AgentMetadata> {
        if let Some(agent_path) = agent_path.as_ref() {
            reservation.reserve_agent_path(agent_path)?;
        }
        let candidate_names = spawn::agent_nickname_candidates(config, agent_role.as_deref());
        let candidate_name_refs: Vec<&str> = candidate_names.iter().map(String::as_str).collect();
        let agent_nickname = Some(reservation.reserve_agent_nickname_with_preference(
            &candidate_name_refs,
            preferred_agent_nickname.as_deref(),
        )?);
        Ok(AgentMetadata {
            agent_id: None,
            agent_path,
            agent_nickname,
            agent_role,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_thread_spawn(
        &self,
        reservation: &mut crate::agent::registry::SpawnReservation,
        config: &Config,
        parent_thread_id: ThreadId,
        depth: i32,
        agent_path: Option<AgentPath>,
        agent_role: Option<String>,
        preferred_agent_nickname: Option<String>,
    ) -> CodexResult<(SessionSource, AgentMetadata)> {
        if depth == 1 {
            self.state.register_root_thread(parent_thread_id);
        }
        let agent_metadata = self.prepare_agent_metadata(
            reservation,
            config,
            agent_path,
            agent_role,
            preferred_agent_nickname,
        )?;
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth,
            agent_path: agent_metadata.agent_path.clone(),
            agent_nickname: agent_metadata.agent_nickname.clone(),
            agent_role: agent_metadata.agent_role.clone(),
        });
        Ok((session_source, agent_metadata))
    }

    fn upgrade(&self) -> CodexResult<Arc<ThreadManagerState>> {
        self.manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))
    }

    async fn inherited_environments_for_source(
        &self,
        state: &Arc<ThreadManagerState>,
        session_source: Option<&SessionSource>,
    ) -> Option<TurnEnvironmentSnapshot> {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return None;
        };

        let parent_thread = state.get_thread(*parent_thread_id).await.ok()?;
        Some(
            parent_thread
                .session
                .services
                .turn_environments
                .snapshot()
                .await,
        )
    }

    async fn inherited_exec_policy_for_source(
        &self,
        state: &Arc<ThreadManagerState>,
        session_source: Option<&SessionSource>,
        child_config: &Config,
    ) -> Option<Arc<crate::exec_policy::ExecPolicyManager>> {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return None;
        };

        let parent_thread = state.get_thread(*parent_thread_id).await.ok()?;
        let parent_config = parent_thread.session.get_config().await;
        if !crate::exec_policy::child_uses_parent_exec_policy(&parent_config, child_config) {
            return None;
        }

        Some(Arc::clone(&parent_thread.session.services.exec_policy))
    }

    async fn open_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
    ) -> CodexResult<Vec<(ThreadId, AgentMetadata)>> {
        let mut children_by_parent = self.live_thread_spawn_children().await?;
        Ok(children_by_parent
            .remove(&parent_thread_id)
            .unwrap_or_default())
    }

    async fn live_thread_spawn_children(
        &self,
    ) -> CodexResult<HashMap<ThreadId, Vec<(ThreadId, AgentMetadata)>>> {
        let state = self.upgrade()?;
        let mut children_by_parent = HashMap::<ThreadId, Vec<(ThreadId, AgentMetadata)>>::new();

        for (parent_thread_id, child_thread_id) in state.list_live_thread_spawn_edges().await {
            children_by_parent
                .entry(parent_thread_id)
                .or_default()
                .push((
                    child_thread_id,
                    self.state
                        .agent_metadata_for_thread(child_thread_id)
                        .unwrap_or(AgentMetadata {
                            agent_id: Some(child_thread_id),
                            ..Default::default()
                        }),
                ));
        }

        for children in children_by_parent.values_mut() {
            children.sort_by(|left, right| {
                left.1
                    .agent_path
                    .as_deref()
                    .unwrap_or_default()
                    .cmp(right.1.agent_path.as_deref().unwrap_or_default())
                    .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
            });
        }

        Ok(children_by_parent)
    }

    async fn persist_thread_spawn_edge_for_source(
        &self,
        child_thread: &crate::CodexThread,
        child_thread_id: ThreadId,
        session_source: Option<&SessionSource>,
    ) {
        let Some(parent_thread_id) = session_source.and_then(SessionSource::parent_thread_id)
        else {
            return;
        };
        if child_thread.config_snapshot().await.ephemeral {
            return;
        }
        let Ok(state) = self.upgrade() else {
            return;
        };
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return;
        };
        if let Err(err) = agent_graph_store
            .upsert_thread_spawn_edge(
                parent_thread_id,
                child_thread_id,
                codex_agent_graph_store::ThreadSpawnEdgeStatus::Open,
            )
            .await
        {
            warn!("failed to persist thread-spawn edge: {err}");
        }
    }

    async fn live_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        let mut children_by_parent = self.live_thread_spawn_children().await?;
        let mut descendants = Vec::new();
        let mut stack = children_by_parent
            .remove(&root_thread_id)
            .unwrap_or_default()
            .into_iter()
            .map(|(child_thread_id, _)| child_thread_id)
            .rev()
            .collect::<Vec<_>>();

        while let Some(thread_id) = stack.pop() {
            descendants.push(thread_id);
            if let Some(children) = children_by_parent.remove(&thread_id) {
                for (child_thread_id, _) in children.into_iter().rev() {
                    stack.push(child_thread_id);
                }
            }
        }

        Ok(descendants)
    }
}

fn agent_matches_prefix(agent_path: Option<&AgentPath>, prefix: &AgentPath) -> bool {
    if prefix.is_root() {
        return true;
    }

    agent_path.is_some_and(|agent_path| {
        agent_path == prefix
            || agent_path
                .as_str()
                .strip_prefix(prefix.as_str())
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn collect_descendants(
    root_thread_id: ThreadId,
    children_by_parent: &HashMap<ThreadId, Vec<(ThreadId, AgentMetadata)>>,
) -> Vec<ThreadId> {
    let mut descendants = Vec::new();
    let mut stack = children_by_parent
        .get(&root_thread_id)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|(child_thread_id, _)| child_thread_id)
        .rev()
        .collect::<Vec<_>>();

    while let Some(thread_id) = stack.pop() {
        descendants.push(thread_id);
        if let Some(children) = children_by_parent.get(&thread_id) {
            for (child_thread_id, _) in children.iter().rev() {
                stack.push(*child_thread_id);
            }
        }
    }

    descendants
}

pub(crate) fn render_input_preview(input: &[UserInput]) -> String {
    input
        .iter()
        .map(|item| match item {
            UserInput::Text { text, .. } => text.clone(),
            UserInput::Image { .. } => "[image]".to_string(),
            UserInput::LocalImage { path, .. } => {
                format!("[local_image:{}]", path.display())
            }
            UserInput::Audio { .. } => "[audio]".to_string(),
            UserInput::LocalAudio { path } => {
                format!("[local_audio:{}]", path.display())
            }
            UserInput::Skill { name, path, .. } => {
                format!("[skill:${name}]({})", path.display())
            }
            UserInput::Mention { name, path, .. } => format!("[mention:${name}]({path})"),
            _ => "[input]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn thread_spawn_depth(session_source: &SessionSource) -> Option<i32> {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => Some(*depth),
        _ => None,
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
