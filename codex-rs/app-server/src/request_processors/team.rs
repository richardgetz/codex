use codex_app_server_protocol::ThreadTeamSettings;
use codex_app_server_protocol::ThreadTeamSettingsUpdate;
use codex_protocol::protocol::ThreadTeamSettings as CoreThreadTeamSettings;
use codex_protocol::protocol::ThreadTeamSettingsUpdate as CoreThreadTeamSettingsUpdate;

/// Converts the core's persisted team state to the app-server representation.
pub(crate) fn team_settings_from_core(
    settings: Option<CoreThreadTeamSettings>,
) -> Option<ThreadTeamSettings> {
    settings.map(|settings| ThreadTeamSettings {
        mode: settings.mode,
        role: settings.role,
        lead_model: settings.lead_model,
        lead_reasoning_effort: settings.lead_reasoning_effort,
        lead_balance: settings.lead_balance,
        worker_model: settings.worker_model,
        worker_reasoning_effort: settings.worker_reasoning_effort,
        previous_model: settings.previous_model,
        previous_reasoning_effort: settings.previous_reasoning_effort,
    })
}

/// Builds the sparse core update accepted by `thread/settings/update`.
///
/// A client may patch one model profile for the current thread. Restoration
/// fields and the other profile remain owned by the trusted persisted snapshot.
pub(crate) fn team_settings_update_to_core(
    update: ThreadTeamSettingsUpdate,
) -> CoreThreadTeamSettingsUpdate {
    CoreThreadTeamSettingsUpdate {
        mode: update.mode,
        role: update.role,
        model: update.model,
        reasoning_effort: update.reasoning_effort,
        lead_balance: update.lead_balance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::TeamMode;
    use codex_app_server_protocol::TeamRole;
    use codex_protocol::openai_models::ReasoningEffort;

    #[test]
    fn preserves_sparse_profile_patch_fields() {
        let update = ThreadTeamSettingsUpdate {
            mode: TeamMode::Off,
            role: Some(TeamRole::Worker),
            model: Some("gpt-5.6-sol".to_string()),
            reasoning_effort: Some(ReasoningEffort::High),
            lead_balance: None,
        };
        assert_eq!(
            team_settings_update_to_core(update),
            CoreThreadTeamSettingsUpdate {
                mode: codex_protocol::protocol::TeamMode::Off,
                role: Some(codex_protocol::protocol::TeamRole::Worker),
                model: Some("gpt-5.6-sol".to_string()),
                reasoning_effort: Some(ReasoningEffort::High),
                lead_balance: None,
            }
        );
    }
}
