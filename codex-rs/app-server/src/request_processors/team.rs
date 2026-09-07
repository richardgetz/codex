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
        worker_model: settings.worker_model,
        worker_reasoning_effort: settings.worker_reasoning_effort,
        previous_model: settings.previous_model,
        previous_reasoning_effort: settings.previous_reasoning_effort,
    })
}

/// Builds the sparse core update accepted by `thread/settings/update`.
///
/// Assignment and restoration fields belong to trusted persisted snapshots. They are deliberately
/// left empty here so a client can select only the mode and cannot forge a role or model history.
pub(crate) fn team_settings_update_to_core(
    update: ThreadTeamSettingsUpdate,
) -> CoreThreadTeamSettingsUpdate {
    CoreThreadTeamSettingsUpdate { mode: update.mode }
}
