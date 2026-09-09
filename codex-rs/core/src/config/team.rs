use super::Config;
use codex_config::DEFAULT_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES;
use codex_config::TeamModelProfile;
use codex_config::TeamModelProfiles;
use codex_config::TeamRole as ConfigTeamRole;
use codex_protocol::protocol::TeamMode;
use codex_protocol::protocol::TeamRole;
use codex_protocol::protocol::ThreadTeamSettings;

impl Config {
    /// Returns the assignments captured for this thread, falling back to the
    /// current global configuration for a newly created thread.
    pub(crate) fn effective_team_profiles(&self) -> Option<&TeamModelProfiles> {
        self.team_runtime_profiles
            .as_ref()
            .or(self.team.profiles.as_ref())
    }

    /// Returns the effective model assignment for a team role.
    pub(crate) fn effective_team_profile(&self, role: ConfigTeamRole) -> Option<&TeamModelProfile> {
        let profiles = self.effective_team_profiles()?;
        Some(match role {
            ConfigTeamRole::Lead => &profiles.lead,
            ConfigTeamRole::Worker => &profiles.worker,
        })
    }

    /// Applies the durable team snapshot found in a rollout to this runtime.
    /// Assignment fields are accepted only as a complete Lead/Worker pair so a
    /// partially written or legacy snapshot cannot silently select one role.
    pub(crate) fn restore_team_snapshot(
        &mut self,
        settings: &ThreadTeamSettings,
    ) -> Result<(), String> {
        let oversight_timeout_minutes = self
            .effective_team_profiles()
            .map(|profiles| profiles.lead_oversight_timeout_minutes)
            .unwrap_or(DEFAULT_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES);
        if let Some(profiles) =
            team_profiles_from_snapshot_with_timeout(settings, oversight_timeout_minutes)?
        {
            self.team_runtime_profiles = Some(profiles);
        }
        self.team_state_persisted = true;
        self.team_persisted_role = settings.role;
        self.team_mode = settings.mode;
        self.team_previous_model = settings.previous_model.clone();
        self.team_previous_reasoning_effort = settings.previous_reasoning_effort.clone();
        Ok(())
    }

    /// Builds the protocol snapshot used in rollout and app-server responses.
    pub(crate) fn team_settings_snapshot(
        &self,
        role: Option<TeamRole>,
    ) -> Option<ThreadTeamSettings> {
        let profiles = self.effective_team_profiles();
        if !self.team_state_persisted && self.team_mode == TeamMode::Off && profiles.is_none() {
            return None;
        }
        Some(ThreadTeamSettings {
            mode: self.team_mode,
            role: self.team_persisted_role.or(role),
            lead_model: profiles.map(|profiles| profiles.lead.model.clone()),
            lead_reasoning_effort: profiles.map(|profiles| profiles.lead.reasoning_effort.clone()),
            worker_model: profiles.map(|profiles| profiles.worker.model.clone()),
            worker_reasoning_effort: profiles
                .map(|profiles| profiles.worker.reasoning_effort.clone()),
            previous_model: self.team_previous_model.clone(),
            previous_reasoning_effort: self.team_previous_reasoning_effort.clone(),
        })
    }
}

/// Parses the complete assignment pair carried by a persisted protocol snapshot.
pub(crate) fn team_profiles_from_snapshot(
    settings: &ThreadTeamSettings,
) -> Result<Option<TeamModelProfiles>, String> {
    team_profiles_from_snapshot_with_timeout(settings, DEFAULT_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES)
}

fn team_profiles_from_snapshot_with_timeout(
    settings: &ThreadTeamSettings,
    lead_oversight_timeout_minutes: u64,
) -> Result<Option<TeamModelProfiles>, String> {
    let assignments = (
        settings.lead_model.as_ref(),
        settings.lead_reasoning_effort.clone(),
        settings.worker_model.as_ref(),
        settings.worker_reasoning_effort.clone(),
    );
    let any_assignment = assignments.0.is_some()
        || assignments.1.is_some()
        || assignments.2.is_some()
        || assignments.3.is_some();
    if !any_assignment {
        return Ok(None);
    }
    let (
        Some(lead_model),
        Some(lead_reasoning_effort),
        Some(worker_model),
        Some(worker_reasoning_effort),
    ) = assignments
    else {
        return Err(
            "thread team snapshot must contain complete Lead and Worker assignments".to_string(),
        );
    };
    let lead_model = lead_model.trim();
    let worker_model = worker_model.trim();
    if lead_model.is_empty() {
        return Err("thread team snapshot Lead model must be a non-empty string".to_string());
    }
    if worker_model.is_empty() {
        return Err("thread team snapshot Worker model must be a non-empty string".to_string());
    }
    Ok(Some(TeamModelProfiles {
        lead: TeamModelProfile {
            model: lead_model.to_string(),
            reasoning_effort: lead_reasoning_effort,
        },
        worker: TeamModelProfile {
            model: worker_model.to_string(),
            reasoning_effort: worker_reasoning_effort,
        },
        lead_oversight_timeout_minutes,
    }))
}

#[cfg(test)]
#[path = "team_tests.rs"]
mod tests;
