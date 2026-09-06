use super::team_settings_update_params;
use crate::chatwidget::TeamCommand;
use codex_app_server_protocol::TeamMode;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_app_server_protocol::ThreadTeamSettingsUpdate;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[test]
fn team_commands_build_sparse_thread_settings_updates() {
    let thread_id = ThreadId::new();
    let expected = ThreadSettingsUpdateParams {
        thread_id: thread_id.to_string(),
        team: Some(ThreadTeamSettingsUpdate {
            mode: TeamMode::LeadWorker,
        }),
        ..ThreadSettingsUpdateParams::default()
    };

    assert_eq!(
        team_settings_update_params(thread_id, TeamCommand::On),
        Some(expected)
    );
    assert_eq!(
        team_settings_update_params(thread_id, TeamCommand::Off),
        Some(ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            team: Some(ThreadTeamSettingsUpdate {
                mode: TeamMode::Off,
            }),
            ..ThreadSettingsUpdateParams::default()
        })
    );
    assert_eq!(
        team_settings_update_params(thread_id, TeamCommand::Status),
        None
    );
}
