use super::continuous_loopback::ScratchpadLoopbackLimiter;
use super::input_queue::InputQueue;
use super::mcp_refresh::McpRefresh;
use super::*;
use crate::agents_md_manager::AgentsMdManager;
use crate::environment_selection::ThreadEnvironments;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::hook_mcp_executor::CoreHookMcpExecutor;
use crate::shell_snapshot::ShellSnapshot;
use crate::state::ActiveTurn;
use codex_config::types::ScratchpadLoopbackConfig;
use codex_extension_api::ExtensionDataInit;
use codex_http_client::ClientRouteClass;
use codex_http_client::RouteAwareClientPool;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_memories_read::memory_root;
use codex_model_provider::SharedModelProvider;
use codex_protocol::SessionId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::MemoryAccessPolicy;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::config_types::UserPreferencesMemoryBucketPolicy;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_protocol::security_risk::SecurityRiskScore;
use codex_skills::SkillError;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::time::Instant;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;

pub(crate) const MEMORY_WRITE_GATE_PERMITS: u32 = 1024;

/// Context for an initialized model agent
///
/// A session has at most 1 running task at a time, and can be interrupted by user input.
pub(crate) struct Session {
    pub(crate) thread_id: ThreadId,
    pub(crate) installation_id: String,
    pub(super) tx_event: Sender<Event>,
    pub(super) agent_status: watch::Sender<AgentStatus>,
    pub(super) state: Mutex<SessionState>,
    pub(super) memory_write_gate: Semaphore,
    /// Serializes rebuild/apply cycles for the running proxy; each cycle
    /// rebuilds from the current SessionState while holding this lock.
    pub(super) managed_network_proxy_refresh_lock: Semaphore,
    /// The set of enabled features should be invariant for the lifetime of the
    /// session.
    pub(super) features: ManagedFeatures,
    pub(crate) windows_sandbox_proxy_settings_mode:
        codex_sandboxing::WindowsSandboxProxySettingsMode,
    pub(super) multi_agent_version: OnceLock<MultiAgentVersion>,
    /// Owns invalidation and serializes refreshes without blocking captured calls.
    pub(super) mcp_refresh: McpRefresh,
    pub(super) mcp_elicitation_reviewer_handle: OnceLock<codex_mcp::ElicitationReviewerHandle>,
    pub(super) mcp_elicitation_lifecycle_handle: OnceLock<codex_mcp::ElicitationLifecycle>,
    pub(super) mcp_prewarm_tx: async_channel::Sender<()>,
    pub(super) mcp_prewarm_shutdown: CancellationToken,
    pub(super) mcp_prewarm_task: std::sync::Mutex<Option<JoinHandle<()>>>,
    pub(crate) conversation: Arc<RealtimeConversationManager>,
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>,
    /// Tracks recent automatic scratchpad loopbacks for this loaded thread.
    pub(crate) scratchpad_loopback_limiter: std::sync::Mutex<ScratchpadLoopbackLimiter>,
    pub(crate) pending_user_message_admissions:
        crate::user_message_admission::PendingUserMessageAdmissions,
    pub(crate) async_hook_results: async_channel::Receiver<HookCompletedEvent>,
    pub(crate) input_queue: InputQueue,
    pub(crate) guardian_review_session: GuardianReviewSessionManager,
    pub(crate) services: SessionServices,
    /// Owns this thread's managed temporary directory and root-session cleanup.
    pub(crate) session_tmp: Option<codex_session_tmp::SessionTmpManager>,
    pub(super) git_enrichment_policy: GitEnrichmentPolicy,
    pub(super) fork_persistence: ForkPersistence,
    pub(super) next_internal_sub_id: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct SessionConfiguration {
    /// Runtime provider and its provider-specific execution policy.
    pub(super) provider: SharedModelProvider,

    pub(super) collaboration_mode: CollaborationMode,
    pub(super) model_reasoning_summary: Option<ReasoningSummaryConfig>,
    pub(super) service_tier: Option<String>,

    /// Developer instructions that supplement the base instructions.
    pub(super) developer_instructions: Option<String>,
    pub(super) user_instructions: Option<String>,

    /// Personality preference for the model.
    pub(super) personality: Option<Personality>,

    /// Base instructions for the session.
    pub(super) base_instructions: String,

    /// When to escalate for approval for execution
    pub(super) approval_policy: Constrained<AskForApproval>,
    pub(super) approvals_reviewer: ApprovalsReviewer,
    /// Permission profile state for the session. Keep the constrained profile,
    /// active profile id, and profile-defined workspace roots in sync by using
    /// the methods below instead of mutating the fields independently.
    pub(super) permission_profile_state: PermissionProfileState,
    pub(super) allow_login_shell: bool,
    pub(super) shell_environment_policy: ShellEnvironmentPolicy,
    pub(super) windows_sandbox_level: WindowsSandboxLevel,

    /// Legacy thread cwd used when a turn does not select an environment.
    pub(super) legacy_fallback_cwd: AbsolutePathBuf,
    /// Directory containing all Codex state for this session.
    pub(super) codex_home: AbsolutePathBuf,
    /// Session-owned temporary root exposed to this thread's sandbox and
    /// child-process environment, when the opt-in feature is enabled.
    pub(super) session_tmp_agent_root: Option<AbsolutePathBuf>,
    /// Optional user-facing name for the thread, updated during the session.
    pub(super) thread_name: Option<String>,
    pub(super) memory_policy: MemoryAccessPolicy,
    pub(super) user_preferences_memory_policy: UserPreferencesMemoryBucketPolicy,

    // TODO(pakrym): Remove config from here
    pub(super) original_config_do_not_use: Arc<Config>,
    /// Optional service name tag for session metrics.
    pub(super) metrics_service_name: Option<String>,
    pub(super) app_server_client_name: Option<String>,
    pub(super) app_server_client_version: Option<String>,
    /// Guardian reviewer identity is trusted only when established during an in-memory spawn.
    pub(super) trusted_guardian_reviewer: bool,
    /// Source of the session (cli, vscode, exec, mcp, ...)
    pub(super) session_source: SessionSource,
    /// Persisted thread history contract selected when this thread was created.
    pub(super) history_mode: ThreadHistoryMode,
    /// Immediate history source copied into this thread, when this thread was forked.
    pub(super) forked_from_thread_id: Option<ThreadId>,
    /// Immediate control/spawn parent for this thread, when it has one.
    pub(super) parent_thread_id: Option<ThreadId>,
    /// Optional analytics source classification for this thread.
    pub(super) thread_source: Option<ThreadSource>,
    /// Effective originator used for this thread's Responses requests and analytics events.
    pub(super) originator: String,
    pub(super) dynamic_tools: Vec<DynamicToolSpec>,
    pub(super) user_shell_override: Option<shell::Shell>,
}

impl SessionConfiguration {
    pub(super) fn cwd(&self) -> &AbsolutePathBuf {
        &self.legacy_fallback_cwd
    }

    pub(crate) fn codex_home(&self) -> &AbsolutePathBuf {
        &self.codex_home
    }

    pub(super) fn inferred_environment_config(&self) -> EnvironmentConfig {
        EnvironmentConfig {
            allow_login_shell: self.allow_login_shell,
            // The environment attachment boundary adds the session temporary root only to
            // local environments. Keeping this inherited config rootless prevents a remote
            // executor from receiving a host-local writable path.
            permission_profile: self.permission_profile_state.snapshot(),
            shell_environment_policy: self.shell_environment_policy.clone(),
            exec_policy: None,
            mcp_policy: None,
            network_policy: None,
            selected_capability_roots: Vec::new(),
        }
    }

    pub(super) fn permission_profile(&self) -> PermissionProfile {
        self.permission_profile_state.permission_profile().clone()
    }

    fn materialized_permission_profile(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> PermissionProfile {
        let workspace_roots = ThreadEnvironments::primary_workspace_roots_for(environments);
        self.with_session_tmp_root(
            self.permission_profile()
                .materialize_project_roots_with_workspace_roots(&workspace_roots),
        )
    }

    fn with_session_tmp_root(&self, permission_profile: PermissionProfile) -> PermissionProfile {
        let Some(session_tmp_agent_root) = self.session_tmp_agent_root.as_ref() else {
            return permission_profile;
        };
        let file_system_sandbox_policy = permission_profile
            .file_system_sandbox_policy()
            .with_additional_writable_roots(
                self.cwd(),
                std::slice::from_ref(session_tmp_agent_root),
            );
        PermissionProfile::from_runtime_permissions_with_enforcement(
            permission_profile.enforcement(),
            &file_system_sandbox_policy,
            permission_profile.network_sandbox_policy(),
        )
    }

    fn permission_profile_snapshot(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> PermissionProfileSnapshot {
        let snapshot = self.permission_profile_state.snapshot();
        let permission_profile = if environments.is_empty() {
            self.with_session_tmp_root(snapshot.permission_profile().clone())
        } else {
            self.materialized_permission_profile(environments)
        };
        match snapshot.active_permission_profile() {
            Some(active_permission_profile) => {
                PermissionProfileSnapshot::active_with_profile_workspace_roots(
                    permission_profile,
                    active_permission_profile,
                    snapshot.profile_workspace_roots().to_vec(),
                )
            }
            None => PermissionProfileSnapshot::legacy(permission_profile),
        }
    }

    pub(super) fn active_permission_profile(&self) -> Option<ActivePermissionProfile> {
        self.permission_profile_state.active_permission_profile()
    }

    pub(super) fn apply_permission_profile_to_permissions(
        &self,
        permissions: &mut crate::config::Permissions,
    ) {
        permissions.set_permission_profile_state(self.permission_profile_state.clone());
    }

    #[cfg(test)]
    pub(super) fn set_permission_profile_for_tests(
        &mut self,
        permission_profile: PermissionProfile,
    ) -> ConstraintResult<()> {
        self.permission_profile_state
            .set_legacy_permission_profile(permission_profile)
    }

    pub(super) fn sandbox_policy(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> SandboxPolicy {
        let permission_profile = self.materialized_permission_profile(environments);
        codex_sandboxing::compatibility_sandbox_policy_for_permission_profile(
            &permission_profile,
            self.cwd(),
        )
    }

    pub(super) fn file_system_sandbox_policy(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> FileSystemSandboxPolicy {
        self.materialized_permission_profile(environments)
            .file_system_sandbox_policy()
    }

    pub(super) fn network_sandbox_policy(&self) -> NetworkSandboxPolicy {
        self.permission_profile_state
            .permission_profile()
            .network_sandbox_policy()
    }

    pub(super) fn thread_config_snapshot(
        &self,
        environment_selections: Vec<TurnEnvironmentSelection>,
    ) -> ThreadConfigSnapshot {
        let workspace_roots =
            ThreadEnvironments::primary_workspace_roots_for(&environment_selections);
        let permission_profile = ThreadEnvironments::primary_config_for(&environment_selections)
            .map(|config| config.permission_profile.clone())
            .unwrap_or_else(|| self.permission_profile_snapshot(&environment_selections));
        ThreadConfigSnapshot {
            model: self.collaboration_mode.model().to_string(),
            model_provider_id: self.original_config_do_not_use.model_provider_id.clone(),
            service_tier: self.service_tier.clone(),
            approval_policy: self.approval_policy.value(),
            approvals_reviewer: self.approvals_reviewer,
            permission_profile: permission_profile
                .permission_profile()
                .clone()
                .materialize_project_roots_with_workspace_roots(&workspace_roots),
            active_permission_profile: permission_profile.active_permission_profile(),
            environments: TurnEnvironmentSelections::new(
                self.legacy_fallback_cwd.clone(),
                environment_selections,
            ),
            workspace_roots,
            profile_workspace_roots: permission_profile.profile_workspace_roots().to_vec(),
            ephemeral: self.original_config_do_not_use.ephemeral,
            reasoning_effort: self.collaboration_mode.reasoning_effort(),
            reasoning_summary: self.model_reasoning_summary,
            personality: self.personality,
            collaboration_mode: self.collaboration_mode.clone(),
            multi_agent_mode: MultiAgentMode::ExplicitRequestOnly,
            session_source: self.session_source.clone(),
            history_mode: self.history_mode,
            forked_from_thread_id: self.forked_from_thread_id,
            parent_thread_id: self.parent_thread_id,
            thread_source: self.thread_source.clone(),
            memory_policy: self.memory_policy,
            user_preferences_memory_policy: self.user_preferences_memory_policy.clone(),
            originator: self.originator.clone(),
        }
    }

    /// Captures thread-owned settings for persistence and resume.
    pub(super) fn thread_settings_snapshot(
        &self,
        environment_selections: &[TurnEnvironmentSelection],
    ) -> ThreadSettingsSnapshot {
        ThreadSettingsSnapshot {
            model: self.collaboration_mode.model().to_string(),
            model_provider_id: self.original_config_do_not_use.model_provider_id.clone(),
            service_tier: self.service_tier.clone(),
            approval_policy: self.approval_policy.value(),
            approvals_reviewer: self.approvals_reviewer,
            permission_profile: self.materialized_permission_profile(environment_selections),
            active_permission_profile: self.active_permission_profile(),
            cwd: self.legacy_fallback_cwd.clone(),
            reasoning_effort: self.collaboration_mode.reasoning_effort(),
            reasoning_summary: self.model_reasoning_summary,
            personality: self.personality,
            collaboration_mode: self.collaboration_mode.clone(),
            memory_policy: self.memory_policy,
            user_preferences_memory_policy: self.user_preferences_memory_policy.clone(),
        }
    }

    /// Captures thread-owned settings and their separately owned environments.
    pub(super) fn restorable_thread_settings(
        &self,
        environment_selections: Vec<TurnEnvironmentSelection>,
    ) -> CodexThreadSettingsOverrides {
        CodexThreadSettingsOverrides {
            environments: Some(TurnEnvironmentSelections::new(
                self.legacy_fallback_cwd.clone(),
                environment_selections,
            )),
            profile_workspace_roots: Some(
                self.permission_profile_state
                    .profile_workspace_roots()
                    .to_vec(),
            ),
            approval_policy: Some(self.approval_policy.value()),
            approvals_reviewer: Some(self.approvals_reviewer),
            permission_profile: Some(self.permission_profile()),
            active_permission_profile: self.active_permission_profile(),
            summary: self.model_reasoning_summary,
            service_tier: Some(self.service_tier.clone()),
            collaboration_mode: Some(self.collaboration_mode.clone()),
            personality: self.personality,
            ..Default::default()
        }
    }

    pub(super) fn validate(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> ConstraintResult<()> {
        self.validate_auto_review_requirement(environments)?;
        super::environment::validate_environment_selections(environments).map_err(|error| {
            ConstraintError::InvalidValue {
                field_name: "environments",
                candidate: "environment configuration".to_string(),
                allowed: format!("valid environment configuration ({error})"),
                requirement_source: codex_config::RequirementSource::Unknown,
            }
        })
    }

    fn validate_auto_review_requirement(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> ConstraintResult<()> {
        if self.trusted_guardian_reviewer {
            return Ok(());
        }

        let requirements = self
            .original_config_do_not_use
            .config_layer_stack
            .requirements();
        let model = self.collaboration_mode.model();
        if !requirements.auto_review_required_for_model(model) {
            return Ok(());
        }

        let permission_profile = ThreadEnvironments::primary_config_for(environments)
            .map(|config| config.permission_profile.permission_profile())
            .unwrap_or_else(|| self.permission_profile_state.permission_profile())
            .clone()
            .materialize_project_roots_with_workspace_roots(
                &ThreadEnvironments::primary_workspace_roots_for(environments),
            );
        if self.approvals_reviewer == ApprovalsReviewer::AutoReview
            && !permission_profile
                .file_system_sandbox_policy()
                .has_full_disk_write_access()
            && self
                .original_config_do_not_use
                .features
                .enabled(Feature::GuardianApproval)
        {
            return Ok(());
        }

        Err(ConstraintError::AutoReviewRequired {
            model: model.to_string(),
        })
    }

    pub(super) fn apply(
        &self,
        updates: &SessionSettingsUpdate,
        current_environments: &[TurnEnvironmentSelection],
    ) -> ConstraintResult<Self> {
        let mut next_configuration = self.clone();
        let current_sandbox_policy = self.sandbox_policy(current_environments);
        let current_file_system_sandbox_policy =
            self.file_system_sandbox_policy(current_environments);
        let current_network_sandbox_policy = self.network_sandbox_policy();
        let legacy_file_system_projection =
            FileSystemSandboxPolicy::from_legacy_sandbox_policy_preserving_deny_entries(
                &current_sandbox_policy,
                self.cwd(),
                &current_file_system_sandbox_policy,
            );
        let file_system_policy_matches_legacy = current_file_system_sandbox_policy
            .is_semantically_equivalent_to(&legacy_file_system_projection, self.cwd());
        let file_system_policy_has_rebindable_project_root_write =
            current_file_system_sandbox_policy
                .entries
                .iter()
                .any(|entry| {
                    entry.access.can_write()
                        && matches!(
                            &entry.path,
                            FileSystemPath::Special {
                                value: FileSystemSpecialPath::ProjectRoots { subpath: None },
                            }
                        )
                });
        if let Some(collaboration_mode) = updates.collaboration_mode.clone() {
            next_configuration.collaboration_mode = collaboration_mode;
        }
        if let Some(summary) = updates.reasoning_summary {
            next_configuration.model_reasoning_summary = Some(summary);
        }
        if let Some(service_tier) = updates.service_tier.clone() {
            // TODO(aibrahim): Remove once v2 clients no longer send the legacy
            // "fast" service tier value.
            next_configuration.service_tier = service_tier.map(|service_tier| {
                ServiceTier::from_request_value(&service_tier)
                    .map_or(service_tier, |service_tier| {
                        service_tier.request_value().to_string()
                    })
            });
        }
        if let Some(personality) = updates.personality {
            next_configuration.personality = Some(personality);
        }
        let memory_policy_changed = updates.memory_policy.is_some();
        if let Some(policy) = updates.memory_policy {
            next_configuration.memory_policy = policy.normalized();
        }
        if let Some(policy) = updates.user_preferences_memory_policy.clone() {
            next_configuration.user_preferences_memory_policy = policy;
        }
        if let Some(approval_policy) = updates.approval_policy {
            next_configuration.approval_policy.set(approval_policy)?;
        }
        if let Some(approvals_reviewer) = updates.approvals_reviewer {
            next_configuration
                .original_config_do_not_use
                .config_layer_stack
                .requirements()
                .approvals_reviewer
                .can_set(&approvals_reviewer)?;
            next_configuration.approvals_reviewer = approvals_reviewer;
        }
        if !next_configuration.trusted_guardian_reviewer
            && self.collaboration_mode.model() != next_configuration.collaboration_mode.model()
            && next_configuration
                .original_config_do_not_use
                .config_layer_stack
                .requirements()
                .auto_review_required_for_model(next_configuration.collaboration_mode.model())
            && updates.approvals_reviewer.is_none()
        {
            next_configuration
                .original_config_do_not_use
                .config_layer_stack
                .requirements()
                .approvals_reviewer
                .can_set(&ApprovalsReviewer::AutoReview)?;
            next_configuration.approvals_reviewer = ApprovalsReviewer::AutoReview;
        }
        if let Some(windows_sandbox_level) = updates.windows_sandbox_level {
            next_configuration.windows_sandbox_level = windows_sandbox_level;
        }

        let current_cwd = self.cwd().clone();
        if let Some(environments) = &updates.environments {
            next_configuration.legacy_fallback_cwd = environments.legacy_fallback_cwd.clone();
        }
        let cwd_changed = next_configuration.legacy_fallback_cwd != current_cwd;

        if let Some(permission_profile) = updates.permission_profile.clone() {
            let active_permission_profile =
                updates.active_permission_profile.clone().or_else(|| {
                    if permission_profile == self.permission_profile() {
                        self.active_permission_profile()
                    } else {
                        None
                    }
                });
            next_configuration.set_permission_profile_projection(
                permission_profile,
                active_permission_profile,
                updates.profile_workspace_roots.clone().unwrap_or_default(),
                Some(&current_file_system_sandbox_policy),
            )?;
            if let Some(active_permission_profile) = next_configuration.active_permission_profile()
            {
                let mut config = (*next_configuration.original_config_do_not_use).clone();
                let permission_profile = next_configuration.permission_profile();
                config.permissions.network = config
                    .network_proxy_spec_for_active_permission_profile(
                        &active_permission_profile,
                        &permission_profile,
                    )
                    .map_err(|err| ConstraintError::InvalidValue {
                        field_name: "default_permissions",
                        candidate: active_permission_profile.id.clone(),
                        allowed: format!(
                            "configured permission profile with valid network policy ({err})"
                        ),
                        requirement_source: codex_config::RequirementSource::Unknown,
                    })?;
                config
                    .permissions
                    .set_permission_profile_from_session_snapshot(
                        PermissionProfileSnapshot::active_with_profile_workspace_roots(
                            permission_profile,
                            active_permission_profile,
                            next_configuration
                                .permission_profile_state
                                .profile_workspace_roots()
                                .to_vec(),
                        ),
                    )?;
                next_configuration.original_config_do_not_use = Arc::new(config);
            }
        } else if let Some(sandbox_policy) = updates.sandbox_policy.clone() {
            let file_system_sandbox_policy =
                FileSystemSandboxPolicy::from_legacy_sandbox_policy_preserving_deny_entries(
                    &sandbox_policy,
                    next_configuration.cwd(),
                    &current_file_system_sandbox_policy,
                );
            let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);
            next_configuration
                .permission_profile_state
                .set_legacy_permission_profile(
                    PermissionProfile::from_runtime_permissions_with_enforcement(
                        SandboxEnforcement::from_legacy_sandbox_policy(&sandbox_policy),
                        &file_system_sandbox_policy,
                        network_sandbox_policy,
                    ),
                )?;
        } else if cwd_changed
            && file_system_policy_matches_legacy
            && file_system_policy_has_rebindable_project_root_write
        {
            // Preserve richer split policies across cwd-only updates; only
            // rederive when the session is already using a structurally
            // cwd-bound legacy bridge.
            let file_system_sandbox_policy =
                FileSystemSandboxPolicy::from_legacy_sandbox_policy_preserving_deny_entries(
                    &current_sandbox_policy,
                    next_configuration.cwd(),
                    &current_file_system_sandbox_policy,
                );
            next_configuration
                .permission_profile_state
                .set_legacy_permission_profile(
                    PermissionProfile::from_runtime_permissions_with_enforcement(
                        SandboxEnforcement::from_legacy_sandbox_policy(&current_sandbox_policy),
                        &file_system_sandbox_policy,
                        current_network_sandbox_policy,
                    ),
                )?;
        }
        if memory_policy_changed {
            next_configuration.apply_memory_policy_to_permission_profile(current_environments)?;
        }
        if let Some(app_server_client_name) = updates.app_server_client_name.clone() {
            next_configuration.app_server_client_name = Some(app_server_client_name);
        }
        if let Some(app_server_client_version) = updates.app_server_client_version.clone() {
            next_configuration.app_server_client_version = Some(app_server_client_version);
        }
        let next_environments = updates
            .environments
            .as_ref()
            .map_or(current_environments, |environments| {
                environments.environments.as_slice()
            });
        next_configuration.validate(next_environments)?;
        Ok(next_configuration)
    }

    fn set_permission_profile_projection(
        &mut self,
        permission_profile: PermissionProfile,
        active_permission_profile: Option<ActivePermissionProfile>,
        profile_workspace_roots: Vec<AbsolutePathBuf>,
        preserve_deny_reads_from: Option<&FileSystemSandboxPolicy>,
    ) -> ConstraintResult<()> {
        let enforcement = permission_profile.enforcement();
        let (mut file_system_sandbox_policy, network_sandbox_policy) =
            permission_profile.to_runtime_permissions();
        if let Some(existing_file_system_policy) = preserve_deny_reads_from {
            file_system_sandbox_policy
                .preserve_deny_read_restrictions_from(existing_file_system_policy);
        }
        let effective_permission_profile =
            PermissionProfile::from_runtime_permissions_with_enforcement(
                enforcement,
                &file_system_sandbox_policy,
                network_sandbox_policy,
            );

        let permission_snapshot = match active_permission_profile {
            Some(active_permission_profile) => {
                PermissionProfileSnapshot::active_with_profile_workspace_roots(
                    effective_permission_profile,
                    active_permission_profile,
                    profile_workspace_roots,
                )
            }
            None => PermissionProfileSnapshot::legacy(effective_permission_profile),
        };

        self.permission_profile_state
            .set_permission_profile_snapshot(permission_snapshot)
    }

    fn apply_memory_policy_to_permission_profile(
        &mut self,
        environments: &[TurnEnvironmentSelection],
    ) -> ConstraintResult<()> {
        let memory_root = memory_root(&self.codex_home);
        let legacy_user_preferences_root = self.codex_home.join("user_preferences_memory");
        let user_preferences_extension_root =
            memory_root.join("extensions").join("user_preferences");
        let managed_memory_roots = [
            memory_root.clone(),
            legacy_user_preferences_root.clone(),
            user_preferences_extension_root,
        ];
        let mut file_system_sandbox_policy = self.file_system_sandbox_policy(environments);
        file_system_sandbox_policy.entries.retain(|entry| {
            !matches!(
                &entry.path,
                FileSystemPath::Path { path }
                    if managed_memory_roots
                        .iter()
                        .any(|managed| path.to_abs_path().is_ok_and(|path| managed == &path))
            )
        });

        let memory_policy = self.memory_policy.normalized();
        if memory_policy.write {
            file_system_sandbox_policy = file_system_sandbox_policy
                .with_additional_writable_roots(self.cwd(), std::slice::from_ref(&memory_root));
        } else if memory_policy.read {
            file_system_sandbox_policy = file_system_sandbox_policy.with_additional_readable_roots(
                self.cwd(),
                &[memory_root, legacy_user_preferences_root],
            );
        }

        let active_permission_profile = self.active_permission_profile();
        let profile_workspace_roots = self
            .permission_profile_state
            .profile_workspace_roots()
            .to_vec();
        self.set_permission_profile_projection(
            PermissionProfile::from_runtime_permissions_with_enforcement(
                self.permission_profile_state
                    .permission_profile()
                    .enforcement(),
                &file_system_sandbox_policy,
                self.network_sandbox_policy(),
            ),
            active_permission_profile,
            profile_workspace_roots,
            /*preserve_deny_reads_from*/ None,
        )?;
        Ok(())
    }
}

impl Session {
    /// Reserve an automatic scratchpad loopback using the current session config.
    ///
    /// Holding the session-state lock while reserving the limiter slot makes a
    /// config reload and a loopback admission linearizable with one another.
    pub(crate) async fn try_record_scratchpad_loopback(
        &self,
        now: Instant,
    ) -> (bool, ScratchpadLoopbackConfig) {
        let state = self.state.lock().await;
        let loopback_config = state
            .session_configuration
            .original_config_do_not_use
            .scratchpad
            .loopback;
        let loopback_allowed = self
            .scratchpad_loopback_limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_record_at(now, loopback_config);
        (loopback_allowed, loopback_config)
    }

    pub(crate) async fn memory_write_enabled(&self) -> bool {
        let state = self.state.lock().await;
        state.session_configuration.memory_policy.normalized().write
    }

    pub(crate) async fn memory_write_permit(&self) -> Option<SemaphorePermit<'_>> {
        let Ok(permit) = self.memory_write_gate.acquire().await else {
            return None;
        };
        if self.memory_write_enabled().await {
            Some(permit)
        } else {
            None
        }
    }

    pub(crate) async fn user_preferences_memory_policy(&self) -> UserPreferencesMemoryBucketPolicy {
        let state = self.state.lock().await;
        state
            .session_configuration
            .user_preferences_memory_policy
            .clone()
    }
}

#[derive(Default, Clone)]
pub(crate) struct SessionSettingsUpdate {
    pub(crate) environments: Option<TurnEnvironmentSelections>,
    pub(crate) profile_workspace_roots: Option<Vec<AbsolutePathBuf>>,
    pub(crate) approval_policy: Option<AskForApproval>,
    pub(crate) approvals_reviewer: Option<ApprovalsReviewer>,
    pub(crate) sandbox_policy: Option<SandboxPolicy>,
    pub(crate) permission_profile: Option<PermissionProfile>,
    pub(crate) active_permission_profile: Option<ActivePermissionProfile>,
    pub(crate) windows_sandbox_level: Option<WindowsSandboxLevel>,
    pub(crate) collaboration_mode: Option<CollaborationMode>,
    pub(crate) reasoning_summary: Option<ReasoningSummaryConfig>,
    pub(crate) service_tier: Option<Option<String>>,
    pub(crate) final_output_json_schema: Option<Option<Value>>,
    pub(crate) personality: Option<Personality>,
    pub(crate) app_server_client_name: Option<String>,
    pub(crate) app_server_client_version: Option<String>,
    pub(crate) memory_policy: Option<MemoryAccessPolicy>,
    pub(crate) user_preferences_memory_policy: Option<UserPreferencesMemoryBucketPolicy>,
}

pub(crate) struct AppServerClientMetadata {
    pub(crate) client_name: Option<String>,
    pub(crate) client_version: Option<String>,
}

async fn warm_plugins_and_skills_for_session_init(
    config: Arc<Config>,
    plugins_manager: Arc<PluginsManager>,
    skills_service: Arc<HostSkillsService>,
    turn_environments: &TurnEnvironmentSnapshot,
) -> Vec<SkillError> {
    let fs = turn_environments.primary_filesystem();
    let plugins_input = config.plugins_config_input();
    let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
    let effective_skill_roots = plugin_outcome.effective_plugin_skill_roots();
    let plugin_skill_snapshots = plugins_manager.plugin_skill_snapshots_for_config(&plugins_input);
    let skills_input = skills_load_input_from_config(config.as_ref(), effective_skill_roots)
        .with_plugin_skill_snapshots(plugin_skill_snapshots);
    skills_service
        .snapshot_for_config(&skills_input, fs)
        .await
        .outcome()
        .errors
        .clone()
}

impl Session {
    /// Returns the concrete identity for this thread.
    pub(crate) fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    /// Returns the identity shared by the root thread and all descendant threads.
    pub(crate) fn session_id(&self) -> SessionId {
        self.services.agent_control.session_id()
    }

    pub(crate) fn session_tmp_agent_root(&self) -> Option<&Path> {
        self.session_tmp
            .as_ref()
            .map(codex_session_tmp::SessionTmpManager::agent_root)
    }

    pub(crate) async fn preview_settings(
        &self,
        updates: &SessionSettingsUpdate,
    ) -> ConstraintResult<ThreadConfigSnapshot> {
        let state = self.state.lock().await;
        let configuration = self.apply_session_settings(&state.session_configuration, updates)?;
        let environments = updates.environments.as_ref().map_or_else(
            || self.services.turn_environments.selections(),
            |environments| environments.environments.clone(),
        );
        Ok(configuration.thread_config_snapshot(environments))
    }

    pub(crate) async fn validate_settings(
        &self,
        updates: &SessionSettingsUpdate,
    ) -> ConstraintResult<()> {
        self.preview_settings(updates).await.map(|_| ())
    }

    pub(crate) async fn originator(&self) -> String {
        let state = self.state.lock().await;
        state.session_configuration.originator.clone()
    }

    #[instrument(name = "session_init", level = "info", skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new(
        mut session_configuration: SessionConfiguration,
        environment_selections: &[TurnEnvironmentSelection],
        config: Arc<Config>,
        user_instructions: Option<codex_extension_api::UserInstructions>,
        installation_id: String,
        auth_manager: Arc<AuthManager>,
        models_manager: SharedModelsManager,
        model_info: ModelInfo,
        exec_policy: Arc<ExecPolicyManager>,
        tx_event: Sender<Event>,
        agent_status: watch::Sender<AgentStatus>,
        mut initial_history: InitialHistory,
        fork_persistence: ForkPersistence,
        session_source: SessionSource,
        skills_service: Arc<HostSkillsService>,
        plugins_manager: Arc<PluginsManager>,
        mcp_manager: Arc<McpManager>,
        code_mode_session_provider: Arc<dyn codex_code_mode::CodeModeSessionProvider>,
        extensions: Arc<codex_extension_api::ExtensionRegistry<crate::config::Config>>,
        mut thread_extension_init: ExtensionDataInit,
        client_mcp_extensions: ClientMcpExtensions,
        agent_control: AgentControl,
        reserved_thread_id: Option<ThreadId>,
        environment_manager: Arc<EnvironmentManager>,
        inherited_environments: Option<TurnEnvironmentSnapshot>,
        analytics_events_client: Option<AnalyticsEventsClient>,
        thread_store: Arc<dyn ThreadStore>,
        parent_rollout_thread_trace: ThreadTraceContext,
        attestation_provider: Option<Arc<dyn AttestationProvider>>,
        external_time_provider: Option<Arc<dyn TimeProvider>>,
        multi_agent_version: Option<MultiAgentVersion>,
        git_enrichment_policy: GitEnrichmentPolicy,
        windows_sandbox_proxy_settings_mode: codex_sandboxing::WindowsSandboxProxySettingsMode,
    ) -> anyhow::Result<Arc<Self>> {
        debug!(
            "Configuring session: model={}; provider={:?}",
            session_configuration.collaboration_mode.model(),
            session_configuration.provider
        );
        let base_instructions_provenance = if config.base_instructions.is_some() {
            Some(
                config
                    .base_instructions_provenance
                    .clone()
                    .unwrap_or(BaseInstructionsProvenance::Custom),
            )
        } else if let Some(inherited_base_instructions) = initial_history.get_base_instructions() {
            let BaseInstructions { text, provenance } = inherited_base_instructions;
            provenance.or_else(|| {
                (text == model_info.get_model_instructions(config.personality)).then(|| {
                    BaseInstructionsProvenance::Model {
                        model: model_info.slug.clone(),
                    }
                })
            })
        } else {
            Some(BaseInstructionsProvenance::Model {
                model: model_info.slug.clone(),
            })
        };
        let forked_from_id = session_configuration
            .forked_from_thread_id
            .or_else(|| initial_history.forked_from_id());
        session_configuration.forked_from_thread_id = forked_from_id;
        let parent_thread_id = session_configuration
            .parent_thread_id
            .or_else(|| initial_history.get_resumed_parent_thread_id());
        session_configuration.parent_thread_id = parent_thread_id;
        let is_paginated_subagent = matches!(
            session_configuration.history_mode,
            ThreadHistoryMode::Paginated
        ) && matches!(
            session_configuration.thread_source.as_ref(),
            Some(ThreadSource::Subagent)
        );
        if let InitialHistory::Forked(items) = &mut initial_history {
            Self::assign_missing_rollout_response_item_ids(items);
        }
        let multi_agent_version = multi_agent_version.map(OnceLock::from).unwrap_or_default();
        let initial_multi_agent_version = multi_agent_version.get().copied();

        let thread_id = match (&initial_history, reserved_thread_id) {
            (
                InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_),
                Some(thread_id),
            ) => thread_id,
            (InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_), None) => {
                agent_control.generate_thread_id()
            }
            (InitialHistory::Resumed(resumed_history), None) => resumed_history.conversation_id,
            (InitialHistory::Resumed(_), Some(_)) => {
                return Err(anyhow::anyhow!(
                    "reserved thread ID cannot be used when resuming a thread"
                ));
            }
        };
        let resumed_session_id = match &initial_history {
            InitialHistory::Resumed(resumed) => {
                resumed.history.iter().find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.session_id),
                    _ => None,
                })
            }
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
        };
        // Legacy subagent rollouts synthesize session_id from their own thread id.
        let resumed_session_id = resumed_session_id.filter(|session_id| {
            !session_configuration.session_source.is_non_root_agent()
                || *session_id != SessionId::from(thread_id)
        });
        let session_id = resumed_session_id.unwrap_or_else(|| {
            if session_configuration.session_source.is_non_root_agent() {
                agent_control.session_id()
            } else {
                SessionId::from(thread_id)
            }
        });
        let initial_auto_compact_window_ids = AutoCompactWindowIds::new_initial();
        let restore_child_window = matches!(&initial_history, InitialHistory::Forked(_))
            && session_configuration.session_source.is_non_root_agent()
            && config.features.enabled(Feature::TokenBudget);
        if restore_child_window && let InitialHistory::Forked(items) = &mut initial_history {
            let child_window_id = initial_auto_compact_window_ids.window_id.to_string();
            for item in items {
                if let RolloutItem::Compacted(checkpoint) = item {
                    checkpoint.window_number = Some(0);
                    checkpoint.first_window_id = Some(child_window_id.clone());
                    checkpoint.previous_window_id = None;
                    checkpoint.window_id = Some(child_window_id.clone());
                }
            }
        }
        let agent_control = agent_control.with_session_id(
            session_id,
            config
                .effective_agent_max_threads(MultiAgentVersion::V2)
                .unwrap_or(usize::MAX),
        );
        let session_tmp_config = codex_session_tmp::SessionTmpConfig {
            enabled: config.session_tmp.enabled,
            root: config
                .session_tmp
                .root
                .as_ref()
                .map(AbsolutePathBuf::to_path_buf),
            stale_after: config.session_tmp.stale_after,
        };
        let is_root_session = session_id == SessionId::from(thread_id);
        let session_tmp = codex_session_tmp::SessionTmpManager::open(
            &session_tmp_config,
            config.codex_home.as_path(),
            &session_id.to_string(),
            &thread_id.to_string(),
            if is_root_session {
                codex_session_tmp::SessionTmpOwner::RootSession
            } else {
                codex_session_tmp::SessionTmpOwner::Agent
            },
        )?;
        session_configuration.session_tmp_agent_root = session_tmp
            .as_ref()
            .map(|manager| AbsolutePathBuf::from_absolute_path(manager.agent_root()))
            .transpose()?;
        let time_provider = crate::current_time::resolve_time_provider(
            config.current_time_reminder.as_ref(),
            external_time_provider,
        )?;
        if thread_extension_init.get::<SecurityRiskScore>().is_none()
            && let Some(score) = initial_history
                .get_rollout_items()
                .iter()
                .rev()
                .find_map(|item| match item {
                    RolloutItem::SecurityRiskScore(score) => Some(score),
                    _ => None,
                })
        {
            thread_extension_init.insert(score.clone());
        }
        let selected_capability_roots =
            match thread_extension_init.get::<Vec<SelectedCapabilityRoot>>() {
                Some(roots) => roots.as_ref().clone(),
                None => {
                    let roots = initial_history.get_selected_capability_roots();
                    if !roots.is_empty() {
                        thread_extension_init.insert(roots.clone());
                    }
                    roots
                }
            };
        thread_extension_init.insert(codex_extension_api::ThreadOriginator(
            session_configuration.originator.clone(),
        ));
        let mcp_thread_init = thread_extension_init.clone();
        let thread_extension_data = codex_extension_api::ExtensionData::new_with_init(
            thread_id.to_string(),
            thread_extension_init,
        );
        // Kick off independent async setup tasks in parallel to reduce startup latency.
        //
        // - initialize thread persistence with new or resumed session info
        // - perform default shell discovery
        // - load history metadata (skipped for subagents)
        let thread_persistence_fut = async {
            if config.ephemeral {
                Ok::<_, anyhow::Error>(None)
            } else {
                let live_thread = match &initial_history {
                    InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => {
                        let params = CreateThreadParams {
                            session_id,
                            thread_id,
                            extra_config: config.extra_config.clone(),
                            forked_from_id,
                            parent_thread_id,
                            source: session_source,
                            thread_source: session_configuration.thread_source.clone(),
                            originator: session_configuration.originator.clone(),
                            base_instructions: BaseInstructions {
                                text: session_configuration.base_instructions.clone(),
                                provenance: base_instructions_provenance.clone(),
                            },
                            dynamic_tools: session_configuration.dynamic_tools.clone(),
                            selected_capability_roots: selected_capability_roots.clone(),
                            multi_agent_version: initial_multi_agent_version,
                            history_mode: session_configuration.history_mode,
                            history_base: match &fork_persistence {
                                ForkPersistence::Copied => None,
                                ForkPersistence::Referenced { history_base, .. } => *history_base,
                            },
                            subagent_history_start_ordinal: None,
                            initial_window_id: initial_auto_compact_window_ids
                                .window_id
                                .to_string(),
                            metadata: ThreadPersistenceMetadata {
                                cwd: Some(config.cwd.to_path_buf()),
                                model_provider: config.model_provider_id.clone(),
                                memory_mode: if config.memories.generate_memories {
                                    ThreadMemoryMode::Enabled
                                } else {
                                    ThreadMemoryMode::Disabled
                                },
                            },
                        };
                        if is_paginated_subagent
                            && matches!(&fork_persistence, ForkPersistence::Copied)
                            && let InitialHistory::Forked(items) = &initial_history
                        {
                            LiveThread::create_with_inherited_model_context(
                                Arc::clone(&thread_store),
                                params,
                                items,
                            )
                            .await?
                        } else {
                            LiveThread::create(Arc::clone(&thread_store), params).await?
                        }
                    }
                    InitialHistory::Resumed(resumed_history) => {
                        let params = ResumeThreadParams {
                            thread_id: resumed_history.conversation_id,
                            rollout_path: resumed_history.rollout_path.clone(),
                            history: Some(resumed_history.history.clone()),
                            include_archived: true,
                            metadata: ThreadPersistenceMetadata {
                                cwd: Some(config.cwd.to_path_buf()),
                                model_provider: config.model_provider_id.clone(),
                                memory_mode: if config.memories.generate_memories {
                                    ThreadMemoryMode::Enabled
                                } else {
                                    ThreadMemoryMode::Disabled
                                },
                            },
                        };
                        LiveThread::resume(
                            Arc::clone(&thread_store),
                            session_configuration.history_mode,
                            params,
                        )
                        .await?
                    }
                };
                Ok(Some(live_thread))
            }
        }
        .instrument(info_span!(
            "session_init.thread_persistence",
            otel.name = "session_init.thread_persistence",
            session_init.ephemeral = config.ephemeral,
        ));
        let state_db_fut = async {
            if config.ephemeral {
                None
            } else if let Some(local_store) =
                thread_store.as_any().downcast_ref::<LocalThreadStore>()
            {
                local_store.state_db().await
            } else {
                None
            }
        }
        .instrument(info_span!(
            "session_init.state_db",
            otel.name = "session_init.state_db",
            session_init.ephemeral = config.ephemeral,
        ));

        let mut mcp_auth_changes = auth_manager.auth_change_receiver();
        let auth_manager_clone = Arc::clone(&auth_manager);
        let config_for_mcp = Arc::clone(&config);
        let mcp_manager_for_mcp = Arc::clone(&mcp_manager);
        let mcp_thread_init_for_startup = &mcp_thread_init;
        let thread_extension_data_for_mcp = &thread_extension_data;
        let mcp_originator = session_configuration.originator.clone();
        let mcp_session_source = session_configuration.session_source.clone();
        let mcp_runtime_cwd = environment_selections
            .first()
            .and_then(|environment| environment.cwd.to_abs_path().ok())
            .map(|cwd| cwd.to_path_buf())
            .unwrap_or_else(|| session_configuration.cwd().to_path_buf());
        let auth_and_mcp_fut = async move {
            let auth = auth_manager_clone.auth().await;
            let mcp_projection = mcp_manager_for_mcp
                .runtime_config_for_step(
                    &config_for_mcp,
                    mcp_thread_init_for_startup,
                    thread_extension_data_for_mcp,
                    McpThreadIdentity {
                        session_source: &mcp_session_source,
                        originator: &mcp_originator,
                        environments: McpEnvironmentScope::Initial(environment_selections),
                    },
                    /*ready_selected_capability_roots*/ &[],
                    /*executor_capability_discovery*/ None,
                )
                .await;
            (auth, mcp_projection)
        }
        .instrument(info_span!(
            "session_init.auth_mcp",
            otel.name = "session_init.auth_mcp",
        ));

        // Join all independent futures.
        let (thread_persistence_result, state_db_ctx, (auth, mcp_projection)) =
            tokio::join!(thread_persistence_fut, state_db_fut, auth_and_mcp_fut);

        let mut live_thread_init =
            LiveThreadInitGuard::new(thread_persistence_result.map_err(|e| {
                error!("failed to initialize thread persistence: {e:#}");
                e
            })?);
        let session_result: anyhow::Result<Arc<Self>> = async {
            let rollout_path = if let Some(live_thread) = live_thread_init.as_ref() {
                live_thread.local_rollout_path().await?
            } else {
                None
            };
            let trace_agent_path = session_configuration
                .session_source
                .get_agent_path()
                .unwrap_or_else(codex_protocol::AgentPath::root);
            let trace_task_name =
                (!trace_agent_path.is_root()).then(|| trace_agent_path.name().to_string());
            let trace_metadata = ThreadStartedTraceMetadata {
                thread_id: thread_id.to_string(),
                agent_path: trace_agent_path.to_string(),
                task_name: trace_task_name,
                nickname: session_configuration.session_source.get_nickname(),
                agent_role: session_configuration.session_source.get_agent_role(),
                session_source: session_configuration.session_source.clone(),
                cwd: session_configuration.cwd().to_path_buf(),
                rollout_path: rollout_path.clone(),
                model: session_configuration.collaboration_mode.model().to_string(),
                provider_name: config.model_provider_id.clone(),
                approval_policy: session_configuration.approval_policy.value().to_string(),
                sandbox_policy: format!(
                    "{:?}",
                    session_configuration.sandbox_policy(environment_selections)
                ),
            };
            let rollout_thread_trace = if matches!(
                session_configuration.session_source,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
            ) {
                // Spawned child threads are part of their root rollout tree. If the
                // parent had no trace bundle, do not create an orphan child bundle
                // that looks like an independent rollout.
                parent_rollout_thread_trace.start_child_thread_trace_or_disabled(trace_metadata)
            } else {
                ThreadTraceContext::start_root_or_disabled(trace_metadata)
            };

            let mut post_session_configured_events = Vec::<Event>::new();

            for usage in config.features.legacy_feature_usages() {
                post_session_configured_events.push(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::DeprecationNotice(DeprecationNoticeEvent {
                        summary: usage.summary.clone(),
                        details: usage.details.clone(),
                    }),
                });
            }
            for message in &config.startup_warnings {
                post_session_configured_events.push(Event {
                    id: "".to_owned(),
                    msg: EventMsg::Warning(WarningEvent {
                        message: message.clone(),
                    }),
                });
            }
            let config_path = config.codex_home.join(CONFIG_TOML_FILE);
            if let Some(event) = unstable_features_warning_event(
                config
                    .config_layer_stack
                    .effective_config()
                    .get("features")
                    .and_then(TomlValue::as_table),
                config.suppress_unstable_features_warning,
                &config.features,
                &config_path.display().to_string(),
            ) {
                post_session_configured_events.push(event);
            }
            let telemetry_auth = auth.as_ref();
            let auth_mode = telemetry_auth
                .map(CodexAuth::auth_mode)
                .map(TelemetryAuthMode::from);
            let account_id = telemetry_auth.and_then(CodexAuth::get_account_id);
            let account_email = telemetry_auth.and_then(CodexAuth::get_account_email);
            let originator = session_configuration.originator.clone();
            let terminal_type = user_agent();
            let session_model = session_configuration.collaboration_mode.model().to_string();
            let auth_env_telemetry = collect_auth_env_telemetry(
                session_configuration.provider.info(),
                auth_manager.codex_api_key_env_enabled(),
            );
            let mut session_telemetry = SessionTelemetry::new(
                thread_id,
                session_model.as_str(),
                session_model.as_str(),
                account_id.clone(),
                account_email.clone(),
                auth_mode,
                originator.clone(),
                config.otel.log_user_prompt,
                terminal_type.clone(),
                session_configuration.session_source.clone(),
            )
            .with_auth_env(auth_env_telemetry.to_otel_metadata());
            if let Some(service_name) = session_configuration.metrics_service_name.as_deref() {
                session_telemetry = session_telemetry.with_metrics_service_name(service_name);
            }
            let network_proxy_audit_metadata = NetworkProxyAuditMetadata {
                conversation_id: Some(thread_id.to_string()),
                app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                user_account_id: account_id,
                auth_mode: auth_mode.map(|mode| mode.to_string()),
                originator: Some(originator),
                user_email: account_email,
                terminal_type: Some(terminal_type),
                model: Some(session_model.clone()),
                slug: Some(session_model),
            };
            config.features.emit_metrics(&session_telemetry);
            session_telemetry.counter(
                THREAD_STARTED_METRIC,
                /*inc*/ 1,
                &[(
                    "is_git",
                    if get_git_repo_root(session_configuration.cwd()).is_some() {
                        "true"
                    } else {
                        "false"
                    },
                )],
            );

            let mcp_server_names =
                codex_mcp::effective_mcp_servers(
                    &mcp_projection.config,
                    auth.as_ref(),
                )
                    .into_iter()
                    .filter_map(|(name, server)| server.enabled().then_some(name))
                    .collect::<Vec<_>>();
            session_telemetry.conversation_starts(
                config.model_provider.name.as_str(),
                session_configuration.collaboration_mode.reasoning_effort(),
                config
                    .model_reasoning_summary
                    .unwrap_or(ReasoningSummaryConfig::Auto),
                config.model_context_window,
                config.model_auto_compact_token_limit,
                config.permissions.approval_policy.value(),
                config
                    .permissions
                    .legacy_sandbox_policy(session_configuration.cwd().as_path()),
                mcp_server_names.iter().map(String::as_str).collect(),
            );

            let use_zsh_fork_shell = config.features.enabled(Feature::ShellZshFork);
            let default_shell = if let Some(user_shell_override) =
                session_configuration.user_shell_override.clone()
            {
                user_shell_override
            } else if use_zsh_fork_shell {
                let zsh_path = config.zsh_path.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "zsh fork feature enabled, but no packaged zsh fork is available for this install"
                    )
                })?;
                if zsh_path.is_file() {
                    shell::Shell {
                        shell_type: shell::ShellType::Zsh,
                        shell_path: zsh_path.clone(),
                    }
                } else {
                    shell::get_shell(shell::ShellType::Zsh).ok_or_else(|| {
                        anyhow::anyhow!(
                            "zsh fork feature enabled, but packaged zsh fork `{}` is not usable",
                            zsh_path.display()
                        )
                    })?
                }
            } else {
                shell::default_user_shell()
            };
            let shell_snapshot = if config.features.enabled(Feature::ShellSnapshot) {
                ShellSnapshot::new(
                    config.codex_home.clone(),
                    thread_id,
                    session_telemetry.clone(),
                    state_db_ctx.clone(),
                )
            } else {
                ShellSnapshot::disabled()
            };
            let turn_environments = Arc::new(ThreadEnvironments::new(
                environment_manager,
                default_shell.clone(),
                session_configuration.inferred_environment_config(),
                shell_snapshot,
                inherited_environments.unwrap_or_default(),
                config.features.enabled(Feature::DeferredExecutor),
            ));
            turn_environments.update_selections(
                environment_selections,
                &session_configuration.inferred_environment_config(),
            );
            if let Some(root) = session_configuration.session_tmp_agent_root.as_ref() {
                turn_environments.add_local_writable_root(root);
            }
            let resolved_environments = turn_environments.snapshot().await;
            let agents_md_manager = Arc::new(AgentsMdManager::new(user_instructions));
            let plugin_skill_warmup = warm_plugins_and_skills_for_session_init(
                Arc::clone(&config),
                Arc::clone(&plugins_manager),
                Arc::clone(&skills_service),
                &resolved_environments,
            )
            .instrument(info_span!(
                "session_init.plugin_skill_warmup",
                otel.name = "session_init.plugin_skill_warmup",
            ));
            let thread_name_lookup =
                thread_title_from_thread_store(live_thread_init.as_ref(), &thread_store, thread_id)
                    .instrument(info_span!(
                        "session_init.thread_name_lookup",
                        otel.name = "session_init.thread_name_lookup",
                    ));
            let (agents_md_result, plugin_skill_errors, thread_name) = tokio::join!(
                agents_md_manager.refresh(
                    config.as_ref(),
                    &resolved_environments,
                    session_configuration.windows_sandbox_level,
                ),
                plugin_skill_warmup,
                thread_name_lookup,
            );
            // TODO(anp): Present AGENTS.md discovery errors more clearly to the user.
            agents_md_result?;
            for err in &plugin_skill_errors {
                error!(
                    "failed to load skill {}: {}",
                    err.path.display(),
                    err.message
                );
            }
            session_configuration.thread_name = thread_name.clone();
            validate_config_lock_if_configured(&session_configuration).await?;
            export_config_lock_if_configured(&session_configuration, thread_id).await?;
            let mut state = SessionState::new_with_auto_compact_window_ids(
                session_configuration.clone(),
                initial_auto_compact_window_ids,
            );
            if let Some(state_db_ctx) = state_db_ctx.as_ref() {
                let active_thread_control = state_db_ctx
                    .get_active_thread_control(thread_id)
                    .await?;
                state.set_active_thread_control(active_thread_control);
            }
            state.base_instructions_provenance = base_instructions_provenance.clone();
            let managed_network_requirements_configured = config
                .config_layer_stack
                .requirements_toml()
                .network
                .is_some();
            let managed_network_requirements_enabled = config.managed_network_requirements_enabled();
            let network_approval = Arc::new(NetworkApprovalService::default());
            // The managed proxy can call back into core for allowlist-miss decisions.
            let network_policy_decider_session = if managed_network_requirements_configured {
                config
                    .permissions
                    .network
                    .as_ref()
                    .map(|_| Arc::new(RwLock::new(std::sync::Weak::<Session>::new())))
            } else {
                None
            };
            let blocked_request_observer = config
                .permissions
                .network
                .as_ref()
                .map(|_| build_blocked_request_observer(Arc::clone(&network_approval)));
            let network_policy_decider =
                network_policy_decider_session
                    .as_ref()
                    .map(|network_policy_decider_session| {
                        build_network_policy_decider(
                            Arc::clone(&network_approval),
                            Arc::clone(network_policy_decider_session),
                        )
                    });
            let (network_proxy, session_network_proxy) =
                if let Some(spec) = config.permissions.network.as_ref() {
                    let current_exec_policy = exec_policy.current();
                    let (network_proxy, session_network_proxy) = Self::start_managed_network_proxy(
                        spec,
                        current_exec_policy.as_ref(),
                        config.permissions.permission_profile(),
                        network_policy_decider.as_ref().map(Arc::clone),
                        blocked_request_observer.as_ref().map(Arc::clone),
                        managed_network_requirements_configured,
                        network_proxy_audit_metadata.clone(),
                    )
                    .instrument(info_span!(
                        "session_init.network_proxy",
                        otel.name = "session_init.network_proxy",
                        session_init.managed_network_requirements_enabled =
                            managed_network_requirements_enabled,
                    ))
                    .await?;
                    (Some(network_proxy), Some(session_network_proxy))
                } else {
                    (None, None)
                };

            // Hooks and extensions share one stable thread-owned MCP runtime handle.
            let mcp_runtime = Arc::new(McpRuntime::empty(
                mcp_projection.config.prefix_mcp_tool_names,
            ));
            let hooks_config = build_hooks_config(
                &config,
                plugins_manager.as_ref(),
                resolved_environments.single_local_environment(),
            )
            .await;
            let (hooks, async_hook_results) = Hooks::new(
                hooks_config,
                thread_id,
                Arc::new(CoreHookMcpExecutor {
                    runtime: Arc::clone(&mcp_runtime),
                    thread_id,
                }),
            )?;
            for warning in hooks.startup_warnings() {
                post_session_configured_events.push(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::Warning(WarningEvent {
                        message: warning.clone(),
                    }),
                });
            }

            let analytics_events_client = analytics_events_client.unwrap_or_else(|| {
                AnalyticsEventsClient::new(
                    Arc::clone(&auth_manager),
                    config.chatgpt_base_url.trim_end_matches('/').to_string(),
                    config.analytics_enabled,
                )
            });
            for item in initial_history.get_rollout_items() {
                match item {
                    RolloutItem::Compacted(compacted) => {
                        if let Some(checkpoint) = &compacted.mcp_resource_origins {
                            mcp_runtime.restore_resource_origin_checkpoint(checkpoint);
                        }
                    }
                    RolloutItem::EventMsg(event) => mcp_runtime.observe_event(event),
                    RolloutItem::SessionMeta(_)
                    | RolloutItem::ResponseItem(_)
                    | RolloutItem::InterAgentCommunication(_)
                    | RolloutItem::InterAgentCommunicationMetadata { .. }
                    | RolloutItem::TurnContext(_)
                    | RolloutItem::WorldState(_)
                    | RolloutItem::SecurityRiskScore(_) => {}
                }
            }
            let session_extension_data =
                codex_extension_api::ExtensionData::new(session_id.to_string());
            session_extension_data.insert(analytics_events_client.clone());
            let mcp_resource_client = Arc::new(McpResourceClient::new(Arc::clone(&mcp_runtime)));
            let extension_metrics =
                extension_metrics::from_session_telemetry(session_telemetry.clone());
            for contributor in extensions.thread_lifecycle_contributors() {
                contributor.on_thread_start(codex_extension_api::ThreadStartInput {
                    config: config.as_ref(),
                    session_source: &session_configuration.session_source,
                    persistent_thread_state_available: state_db_ctx.is_some(),
                    environments: environment_selections,
                    mcp_resource_client: Some(Arc::clone(&mcp_resource_client)),
                    extension_metrics: Some(Arc::clone(&extension_metrics)),
                    session_store: &session_extension_data,
                    thread_store: &thread_extension_data,
                }).await;
            }

            let executed_tool_calls = config
                .features
                .enabled(Feature::ExecutedToolCallMetadata)
                .then(|| Arc::new(crate::state::ExecutedToolCallRecorder::default()));
            let services = SessionServices {
                // Start with an empty connection set. The initialized set is
                // published after SessionConfigured so MCP events follow it.
                mcp_runtime,
                mcp_handler_cache: Default::default(),
                unified_exec_manager: UnifiedExecProcessManager::new(
                    config.background_terminal_max_timeout,
                ),
                elicitations: crate::elicitation::ElicitationService::new(),
                shell_zsh_path: config.zsh_path.clone(),
                main_execve_wrapper_exe: config.main_execve_wrapper_exe.clone(),
                analytics_events_client,
                hooks: arc_swap::ArcSwap::from_pointee(hooks),
                rollout_thread_trace,
                user_shell: Arc::new(default_shell),
                show_raw_agent_reasoning: config.show_raw_agent_reasoning,
                exec_policy,
                auth_manager: Arc::clone(&auth_manager),
                openai_file_upload_client_pool: RouteAwareClientPool::new_without_request_logging(
                    config.http_client_factory(),
                    ClientRouteClass::Api,
                )
                .with_legacy_custom_ca_fallback(),
                session_telemetry,
                models_manager: Arc::clone(&models_manager),
                tool_approvals: Mutex::new(ApprovalStore::default()),
                guardian_rejection_circuit_breaker: Mutex::new(Default::default()),
                runtime_handle: tokio::runtime::Handle::current(),
                skills_service,
                agents_md_manager,
                plugins_manager: Arc::clone(&plugins_manager),
                mcp_manager: Arc::clone(&mcp_manager),
                extensions,
                // TODO(jif): extract session to share between sub-agents
                session_extension_data,
                thread_extension_data,
                selected_capability_roots,
                mcp_thread_init,
                client_mcp_extensions,
                agent_control,
                network_proxy: arc_swap::ArcSwapOption::from(network_proxy.map(Arc::new)),
                network_proxy_audit_metadata,
                managed_network_requirements_configured,
                network_approval: Arc::clone(&network_approval),
                state_db: state_db_ctx.clone(),
                live_thread: live_thread_init.as_ref().cloned(),
                thread_store: Arc::clone(&thread_store),
                attestation_provider: attestation_provider.clone(),
                time_provider,
                model_client: ModelClient::new(
                    Some(Arc::clone(&auth_manager)),
                    if config.features.enabled(Feature::UseAgentIdentity) {
                        AgentIdentityAuthPolicy::ChatGptAuth
                    } else {
                        AgentIdentityAuthPolicy::JwtOnly
                    },
                    thread_id,
                    session_configuration.provider.info().clone(),
                    session_configuration.session_source.clone(),
                    session_configuration.originator.clone(),
                    config.model_verbosity,
                    config.features.enabled(Feature::EnableRequestCompression),
                    config.features.enabled(Feature::RuntimeMetrics),
                    Self::build_model_client_beta_features_header(config.as_ref()),
                    /*concurrent_reasoning_summaries_enabled*/ config
                        .features
                        .enabled(Feature::ConcurrentReasoningSummaries),
                    attestation_provider,
                    config.http_client_factory(),
                )
                .with_prompt_cache_key_override(
                    crate::guardian::prompt_cache_key_override_for_review_session(
                        &session_configuration.session_source,
                        session_configuration.parent_thread_id,
                    ),
                ),
                executed_tool_calls,
                code_mode_service: crate::tools::code_mode::CodeModeService::new(
                    Arc::clone(&code_mode_session_provider),
                    &config.code_mode,
                ),
                orchestrator_memory_generation: AtomicU64::new(0),
                orchestrator_supervision:
                    crate::orchestrator_supervision::OrchestratorSupervisionStore::new(
                        config.codex_home.clone(),
                    ),
                tool_search_handler_cache: Default::default(),
                turn_environments: Arc::clone(&turn_environments),
            };
            let (mcp_prewarm_tx, mcp_prewarm_rx) = async_channel::bounded(1);
            let sess = Arc::new(Session {
                thread_id,
                installation_id,
                tx_event: tx_event.clone(),
                agent_status,
                state: Mutex::new(state),
                memory_write_gate: Semaphore::new(MEMORY_WRITE_GATE_PERMITS as usize),
                managed_network_proxy_refresh_lock: Semaphore::new(/*permits*/ 1),
                features: config.features.clone(),
                windows_sandbox_proxy_settings_mode,
                multi_agent_version,
                mcp_refresh: McpRefresh::new(),
                mcp_elicitation_reviewer_handle: OnceLock::new(),
                mcp_elicitation_lifecycle_handle: OnceLock::new(),
                mcp_prewarm_tx,
                mcp_prewarm_shutdown: CancellationToken::new(),
                mcp_prewarm_task: std::sync::Mutex::new(None),
                conversation: Arc::new(RealtimeConversationManager::new()),
                active_turn: Mutex::new(None),
                scratchpad_loopback_limiter: std::sync::Mutex::new(
                    ScratchpadLoopbackLimiter::default(),
                ),
                pending_user_message_admissions: Default::default(),
                async_hook_results,
                input_queue: InputQueue::new(),
                guardian_review_session: GuardianReviewSessionManager::default(),
                services,
                session_tmp,
                git_enrichment_policy,
                fork_persistence,
                next_internal_sub_id: AtomicU64::new(0),
            });
            if let Some(network_policy_decider_session) = network_policy_decider_session {
                let mut guard = network_policy_decider_session.write().await;
                *guard = Arc::downgrade(&sess);
            }
            // Dispatch the SessionConfiguredEvent first and then report any errors.
            // If resuming, include converted initial messages in the payload so UIs can render them immediately.
            let initial_messages = initial_history.get_event_msgs();
            let thread_config =
                session_configuration.thread_config_snapshot(turn_environments.selections());
            let events = std::iter::once(Event {
                id: INITIAL_SUBMIT_ID.to_owned(),
                msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                    session_id,
                    thread_id,
                    cwd: thread_config.cwd().clone(),
                    forked_from_id: thread_config.forked_from_thread_id,
                    parent_thread_id: thread_config.parent_thread_id,
                    thread_source: thread_config.thread_source,
                    thread_name: session_configuration.thread_name.clone(),
                    model: thread_config.model,
                    model_provider_id: thread_config.model_provider_id,
                    service_tier: thread_config.service_tier,
                    approval_policy: thread_config.approval_policy,
                    approvals_reviewer: thread_config.approvals_reviewer,
                    network_proxy: session_network_proxy.filter(|_| {
                        Self::managed_network_proxy_active_for_permission_profile(
                            &thread_config.permission_profile,
                        )
                    }),
                    permission_profile: thread_config.permission_profile,
                    active_permission_profile: thread_config.active_permission_profile,
                    reasoning_effort: thread_config.reasoning_effort,
                    initial_messages,
                    rollout_path,
                }),
            })
            .chain(post_session_configured_events.into_iter());
            for event in events {
                sess.send_event_raw(event).await;
            }
            turn_environments.start_connection_event_forwarding(tx_event.clone());

            let startup_auth_changed = mcp_auth_changes.has_changed().unwrap_or(false);
            if startup_auth_changed {
                mcp_auth_changes.mark_unchanged();
            }
            let latest_auth = sess.services.auth_manager.auth().await;
            let mcp_projection = if startup_auth_changed
                || mcp_auth_changes.has_changed().unwrap_or(false)
            {
                sess.services
                    .mcp_manager
                    .runtime_config_for_step(
                        config.as_ref(),
                        &sess.services.mcp_thread_init,
                        &sess.services.thread_extension_data,
                        McpThreadIdentity {
                            session_source: &session_configuration.session_source,
                            originator: &session_configuration.originator,
                            environments: McpEnvironmentScope::Live(
                                &sess.services.turn_environments,
                            ),
                        },
                        /*ready_selected_capability_roots*/ &[],
                        /*executor_capability_discovery*/ None,
                    )
                    .await
            } else {
                mcp_projection
            };
            sess.install_initial_mcp_runtime(
                &session_configuration,
                latest_auth,
                mcp_projection,
                &resolved_environments,
                mcp_runtime_cwd,
            )
            .await?;
            sess.start_mcp_prewarm_worker(mcp_prewarm_rx, mcp_auth_changes);
            sess.schedule_startup_prewarm(session_configuration.base_instructions.clone())
                .await;
            let session_start_source = match &initial_history {
                InitialHistory::Resumed(_) => codex_hooks::SessionStartSource::Resume,
                InitialHistory::New | InitialHistory::Forked(_) => {
                    codex_hooks::SessionStartSource::Startup
                }
                InitialHistory::Cleared => codex_hooks::SessionStartSource::Clear,
            };

            // record_initial_history can emit events. We record only after the SessionConfiguredEvent is emitted.
            Box::pin(sess.record_initial_history(initial_history)).await;
            if restore_child_window {
                sess.state.lock().await.restore_auto_compact_window(
                    /*window_number*/ 0,
                    initial_auto_compact_window_ids,
                );
            }
            if matches!(&sess.fork_persistence, ForkPersistence::Referenced { .. }) {
                // Keep the source reserved until the child's history reference is durable.
                sess.try_ensure_rollout_materialized(PersistContext::Standard)
                    .await?;
            }
            {
                let mut state = sess.state.lock().await;
                state.set_pending_session_start_source(Some(session_start_source));
            }

            crate::orchestrator_memory::start_scheduled_cleanup_task(
                &sess,
                &config,
                &session_configuration.session_source,
            );

            Ok(sess)
        }
        .await;
        match session_result {
            Ok(sess) => {
                live_thread_init.commit();
                Ok(sess)
            }
            Err(err) => {
                live_thread_init.discard().await;
                Err(err)
            }
        }
    }
}
