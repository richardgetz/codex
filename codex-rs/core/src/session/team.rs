use crate::config::Config;
use crate::config::ConstraintError;
use crate::config::ConstraintResult;
use crate::context::world_state::TeamPolicyState;
use crate::session::session::SessionConfiguration;
use crate::session::step_settings::StepSettingsUpdate;
use crate::thread_manager::ThreadSettingsOverrideFlags;
use codex_config::TeamRole as ConfigTeamRole;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TeamRole;
use codex_protocol::protocol::ThreadTeamSettings;
use codex_protocol::protocol::ThreadTeamSettingsUpdate;

/// Selects whether team validation must also admit a delegation backend.
#[derive(Clone, Copy)]
pub(crate) enum TeamValidationScope {
    /// Require a usable multi-agent backend and nested Worker capacity.
    Delegation,
    /// Validate profile model and effort routing while allowing delegation to be disabled.
    RoutingOnly,
}

const MAX_TEAM_STARTUP_WARNING_CHARS: usize = 1_024;

/// Resolves the only session sources that receive a team assignment. Internal
/// memory and guardian sessions keep their dedicated model policies.
pub(crate) fn role_for_session_source(source: &SessionSource) -> Option<ConfigTeamRole> {
    match source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        | SessionSource::SubAgent(SubAgentSource::Review) => Some(ConfigTeamRole::Worker),
        SessionSource::Internal(_)
        | SessionSource::SubAgent(
            SubAgentSource::Compact
            | SubAgentSource::MemoryExtraction
            | SubAgentSource::MemoryConsolidation
            | SubAgentSource::Other(_),
        ) => None,
        SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => Some(ConfigTeamRole::Lead),
    }
}

pub(crate) fn effective_role_for_session_source(
    config: &Config,
    source: &SessionSource,
) -> Option<ConfigTeamRole> {
    let source_role = role_for_session_source(source)?;
    Some(match source_role {
        ConfigTeamRole::Worker => ConfigTeamRole::Worker,
        ConfigTeamRole::Lead => match config.team_persisted_role {
            Some(TeamRole::Lead) => ConfigTeamRole::Lead,
            Some(TeamRole::Worker) => ConfigTeamRole::Worker,
            None => ConfigTeamRole::Lead,
        },
    })
}

pub(crate) fn protocol_role_for_session_source(
    config: &Config,
    source: &SessionSource,
) -> Option<TeamRole> {
    effective_role_for_session_source(config, source).map(|role| match role {
        ConfigTeamRole::Lead => TeamRole::Lead,
        ConfigTeamRole::Worker => TeamRole::Worker,
    })
}

/// Disables an invalid root assignment in this session's config while keeping
/// the configured profile pair available for a later explicit toggle.
pub(crate) fn disable_for_startup(
    config: &mut Config,
    baseline_model: Option<&String>,
    baseline_reasoning_effort: Option<&ReasoningEffort>,
    overrides: ThreadSettingsOverrideFlags,
    error: &ConstraintError,
) {
    let fallback_model = if overrides.model {
        baseline_model.cloned()
    } else {
        config
            .team_previous_model
            .clone()
            .or_else(|| baseline_model.cloned())
    };
    let fallback_reasoning_effort = if overrides.reasoning_effort {
        baseline_reasoning_effort.cloned()
    } else if config.team_previous_model.is_some() {
        config.team_previous_reasoning_effort.clone()
    } else {
        baseline_reasoning_effort.cloned()
    };
    let reason = error.to_string();
    let warning = format!(
        "Team mode was disabled for this thread because its assignment is unavailable: {reason}"
    );
    let warning = if warning.chars().count() > MAX_TEAM_STARTUP_WARNING_CHARS {
        warning
            .chars()
            .take(MAX_TEAM_STARTUP_WARNING_CHARS.saturating_sub(1))
            .chain(std::iter::once('…'))
            .collect()
    } else {
        warning
    };
    config.startup_warnings.push(warning);
    config.team_mode = codex_protocol::protocol::TeamMode::Off;
    config.team_state_persisted = true;
    config.team_persisted_role = None;
    config.team_previous_model = None;
    config.team_previous_reasoning_effort = None;
    config.model = fallback_model;
    config.model_reasoning_effort = fallback_reasoning_effort;
}

/// Allows one Worker-owned child for an independent review when the team uses
/// the legacy depth-limited agent backend. An explicit lower user limit remains
/// authoritative and produces an actionable startup error.
pub(crate) fn prepare_spawn_depth(
    config: &mut Config,
    source: &SessionSource,
    multi_agent_version: Option<MultiAgentVersion>,
) -> Result<(), String> {
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) = source else {
        return Ok(());
    };
    prepare_spawn_depth_for_child(config, *depth, multi_agent_version)?;
    Ok(())
}

/// Computes the depth available to a child before spawn-time feature pruning.
///
/// The legacy agent backend needs one extra level for a Worker to launch an
/// independent Worker review. This must run before child feature overrides are
/// applied, while the child still has the parent's configuration snapshot.
pub(crate) fn prepare_spawn_depth_for_child(
    config: &mut Config,
    child_depth: i32,
    multi_agent_version: Option<MultiAgentVersion>,
) -> Result<(), String> {
    let multi_agent_version = multi_agent_version
        .or_else(|| config.multi_agent_version_override())
        .unwrap_or_else(|| config.multi_agent_version_from_features());
    if config.team_mode != codex_protocol::protocol::TeamMode::LeadWorker
        || multi_agent_version == MultiAgentVersion::V2
        || config.agent_max_depth >= 2
        || child_depth < config.agent_max_depth
    {
        return Ok(());
    }
    if config.agent_max_depth_explicit {
        return Err(
            "team mode requires agents.max_depth >= 2 for nested Worker reviews; increase agents.max_depth or enable MultiAgentV2"
                .to_string(),
        );
    }
    config.agent_max_depth = 2;
    Ok(())
}

pub(crate) fn world_state_policy(
    config: &Config,
    source: &SessionSource,
) -> Option<TeamPolicyState> {
    role_for_session_source(source)?;
    if config.team_mode == codex_protocol::protocol::TeamMode::LeadWorker {
        let worker_max_concurrent = config.team.worker_max_concurrent;
        let dynamic_handoff = config
            .effective_team_profiles()
            .is_some_and(|profiles| profiles.lead_dynamic_handoff);
        protocol_role_for_session_source(config, source).map(|role| {
            TeamPolicyState::new(role, worker_max_concurrent).with_dynamic_handoff(dynamic_handoff)
        })
    } else if config.team_state_persisted {
        Some(TeamPolicyState::disabled())
    } else {
        None
    }
}

pub(crate) fn apply_assignment(config: &mut Config, role: ConfigTeamRole) -> Result<bool, String> {
    if config.team_mode != codex_protocol::protocol::TeamMode::LeadWorker {
        return Ok(false);
    }
    let profile = config
        .effective_team_profile(role)
        .cloned()
        .ok_or_else(|| "team mode requires configured Lead and Worker profiles".to_string())?;
    config.team_persisted_role = Some(match role {
        ConfigTeamRole::Lead => TeamRole::Lead,
        ConfigTeamRole::Worker => TeamRole::Worker,
    });
    if config.team_previous_model.is_none() {
        config.team_previous_model = config.model.clone();
        config.team_previous_reasoning_effort = config.model_reasoning_effort.clone();
    }
    config.model = Some(profile.model);
    config.model_reasoning_effort = Some(profile.reasoning_effort);
    Ok(true)
}

fn invalid_team(candidate: impl Into<String>) -> ConstraintError {
    ConstraintError::InvalidValue {
        field_name: "team",
        candidate: candidate.into(),
        allowed: "configured Lead and Worker profiles".to_string(),
        requirement_source: codex_config::RequirementSource::Unknown,
    }
}

pub(crate) fn team_update_changes_profile(update: &ThreadTeamSettingsUpdate) -> bool {
    update.role.is_some() || update.model.is_some() || update.reasoning_effort.is_some()
}

/// Applies a sparse profile patch to a cloned config before it is validated or
/// committed. The selected role must be explicit so a client cannot accidentally
/// overwrite the active assignment or the other role's profile.
pub(crate) fn apply_team_profile_update(
    config: &mut Config,
    update: &ThreadTeamSettingsUpdate,
) -> ConstraintResult<()> {
    if !team_update_changes_profile(update) {
        return Ok(());
    }
    let role = update
        .role
        .ok_or_else(|| invalid_team("profile role is required when updating a team profile"))?;
    if update.model.is_none() && update.reasoning_effort.is_none() {
        return Err(invalid_team(
            "a team profile update must include a model or reasoning effort",
        ));
    }
    let mut profiles = config
        .effective_team_profiles()
        .cloned()
        .ok_or_else(|| invalid_team("configured Lead and Worker profiles"))?;
    let profile = match role {
        TeamRole::Lead => &mut profiles.lead,
        TeamRole::Worker => &mut profiles.worker,
    };
    if let Some(model) = update.model.as_ref() {
        let model = model.trim();
        if model.is_empty() {
            return Err(invalid_team(format!(
                "{role:?} model must be a non-empty string"
            )));
        }
        profile.model = model.to_string();
    }
    if let Some(reasoning_effort) = update.reasoning_effort.clone() {
        profile.reasoning_effort = reasoning_effort;
    }
    config.team_runtime_profiles = Some(profiles);
    Ok(())
}

pub(crate) fn restore_team_snapshot(
    config: &mut Config,
    snapshot: &ThreadTeamSettings,
) -> ConstraintResult<()> {
    config.restore_team_snapshot(snapshot).map_err(invalid_team)
}

/// Applies a client team update to the session configuration and returns the
/// canonical step-settings patch. Trusted snapshots are restored by the session
/// configuration layer before this transition is evaluated.
pub(crate) fn apply_team_update(
    next_configuration: &mut SessionConfiguration,
    current_configuration: &SessionConfiguration,
    team_update: ThreadTeamSettingsUpdate,
    step_settings_update: &mut StepSettingsUpdate,
) -> ConstraintResult<()> {
    if team_update.mode == codex_protocol::protocol::TeamMode::Off
        && effective_role_for_session_source(
            next_configuration.original_config_do_not_use.as_ref(),
            &current_configuration.session_source,
        ) == Some(ConfigTeamRole::Worker)
    {
        return Err(invalid_team("Worker sessions cannot disable team mode"));
    }
    let config = std::sync::Arc::make_mut(&mut next_configuration.original_config_do_not_use);
    apply_team_profile_update(config, &team_update)?;
    match team_update.mode {
        codex_protocol::protocol::TeamMode::Off => {
            let saved_model = config.team_previous_model.clone();
            let restore_model = config.team_previous_model.clone().unwrap_or_else(|| {
                current_configuration
                    .step_settings
                    .collaboration_mode
                    .model()
                    .to_string()
            });
            let restore_effort = if saved_model.is_some() {
                config.team_previous_reasoning_effort.clone()
            } else {
                current_configuration
                    .step_settings
                    .collaboration_mode
                    .reasoning_effort()
            };
            config.team_mode = codex_protocol::protocol::TeamMode::Off;
            config.team_state_persisted = true;
            config.team_previous_model = None;
            config.team_previous_reasoning_effort = None;
            config.model = Some(restore_model.clone());
            config.model_reasoning_effort = restore_effort.clone();
            step_settings_update.model = Some(restore_model);
            step_settings_update.effort = Some(restore_effort);
            if let Some(collaboration_mode) = step_settings_update.collaboration_mode.take() {
                step_settings_update.collaboration_mode = Some(collaboration_mode.with_updates(
                    step_settings_update.model.clone(),
                    step_settings_update.effort.clone(),
                    None,
                ));
            }
        }
        codex_protocol::protocol::TeamMode::LeadWorker => {
            let role =
                effective_role_for_session_source(config, &current_configuration.session_source)
                    .ok_or_else(|| invalid_team("internal session"))?;
            let profile = config
                .effective_team_profile(role)
                .cloned()
                .ok_or_else(|| invalid_team("leadWorker"))?;
            if config.team_previous_model.is_none() {
                config.team_previous_model = Some(
                    current_configuration
                        .step_settings
                        .collaboration_mode
                        .model()
                        .to_string(),
                );
                config.team_previous_reasoning_effort = current_configuration
                    .step_settings
                    .collaboration_mode
                    .reasoning_effort();
            }
            config.team_mode = codex_protocol::protocol::TeamMode::LeadWorker;
            config.team_state_persisted = true;
            config.team_persisted_role = Some(match role {
                ConfigTeamRole::Lead => TeamRole::Lead,
                ConfigTeamRole::Worker => TeamRole::Worker,
            });
            config.model = Some(profile.model.clone());
            config.model_reasoning_effort = Some(profile.reasoning_effort.clone());
            step_settings_update.model = Some(profile.model.clone());
            step_settings_update.effort = Some(Some(profile.reasoning_effort.clone()));
            if let Some(collaboration_mode) = step_settings_update.collaboration_mode.take() {
                step_settings_update.collaboration_mode = Some(collaboration_mode.with_updates(
                    Some(profile.model),
                    Some(Some(profile.reasoning_effort)),
                    None,
                ));
            }
        }
    }
    Ok(())
}

/// Canonicalizes every later model or effort patch while team mode is active.
pub(crate) fn enforce_active_assignment(
    configuration: &SessionConfiguration,
    step_settings_update: &mut StepSettingsUpdate,
) -> ConstraintResult<()> {
    if configuration.original_config_do_not_use.team_mode
        != codex_protocol::protocol::TeamMode::LeadWorker
    {
        return Ok(());
    }
    let Some(role) = effective_role_for_session_source(
        &configuration.original_config_do_not_use,
        &configuration.session_source,
    ) else {
        return Ok(());
    };
    let profile = configuration
        .original_config_do_not_use
        .effective_team_profile(role)
        .cloned()
        .ok_or_else(|| invalid_team("leadWorker"))?;
    step_settings_update.model = Some(profile.model.clone());
    step_settings_update.effort = Some(Some(profile.reasoning_effort.clone()));
    if let Some(collaboration_mode) = step_settings_update.collaboration_mode.take() {
        step_settings_update.collaboration_mode = Some(collaboration_mode.with_updates(
            Some(profile.model),
            Some(Some(profile.reasoning_effort)),
            None,
        ));
    }
    Ok(())
}

/// Checks both configured assignments against the current model catalog before
/// a live team-mode transition is committed.
pub(crate) async fn validate_profiles(
    config: &Config,
    models_manager: &SharedModelsManager,
    selected_multi_agent_version: Option<MultiAgentVersion>,
    validation_scope: TeamValidationScope,
) -> ConstraintResult<MultiAgentVersion> {
    let profiles = config
        .effective_team_profiles()
        .ok_or_else(|| invalid_team("leadWorker"))?;
    let profiles = [(&profiles.lead, "Lead"), (&profiles.worker, "Worker")];
    let _ = models_manager
        .list_models(
            RefreshStrategy::OnlineIfUncached,
            config.http_client_factory(),
        )
        .await;
    let lead_model_info = models_manager
        .get_model_info(&profiles[0].0.model, &config.to_models_manager_config())
        .await;
    let effective_multi_agent_version = selected_multi_agent_version.unwrap_or_else(|| {
        config.multi_agent_version_for_model(lead_model_info.multi_agent_version)
    });
    if matches!(validation_scope, TeamValidationScope::Delegation)
        && effective_multi_agent_version == MultiAgentVersion::Disabled
    {
        return Err(invalid_team(
            "team mode requires multi-agent delegation; enable multi_agent or multi_agent_v2",
        ));
    }
    if matches!(validation_scope, TeamValidationScope::Delegation)
        && effective_multi_agent_version == MultiAgentVersion::V1
        && config.agent_max_depth_explicit
        && config.agent_max_depth < 2
    {
        return Err(invalid_team(
            "agents.max_depth must be at least 2 for team mode with legacy multi-agent support",
        ));
    }
    for (profile, role) in profiles {
        let model_info = models_manager
            .get_model_info(&profile.model, &config.to_models_manager_config())
            .await;
        if model_info.used_fallback_model_metadata {
            return Err(invalid_team(format!(
                "{role} model `{}` is missing from the model catalog",
                profile.model
            )));
        }
        if matches!(validation_scope, TeamValidationScope::Delegation)
            && effective_multi_agent_version == MultiAgentVersion::V2
            && role == "Worker"
            && model_info.multi_agent_version == Some(MultiAgentVersion::Disabled)
        {
            return Err(invalid_team(format!(
                "Worker model `{}` is disabled for MultiAgentV2; choose a model that supports multi-agent delegation",
                profile.model
            )));
        }
        if !model_info
            .supported_reasoning_levels
            .iter()
            .any(|preset| preset.effort == profile.reasoning_effort)
        {
            return Err(invalid_team(format!(
                "{role} effort `{}` is unsupported by model `{}`",
                profile.reasoning_effort, profile.model
            )));
        }
    }
    Ok(effective_multi_agent_version)
}
