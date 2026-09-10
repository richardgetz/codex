use super::team_settings_update_params;
use crate::chatwidget::TeamCommand;
use codex_app_server_protocol::TeamMode;
use codex_app_server_protocol::TeamRole;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_app_server_protocol::ThreadTeamSettingsUpdate;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

#[test]
fn team_commands_build_sparse_thread_settings_updates() {
    let thread_id = ThreadId::new();
    let expected = ThreadSettingsUpdateParams {
        thread_id: thread_id.to_string(),
        team: Some(ThreadTeamSettingsUpdate {
            mode: TeamMode::LeadWorker,
            ..Default::default()
        }),
        ..ThreadSettingsUpdateParams::default()
    };

    assert_eq!(
        team_settings_update_params(thread_id, TeamCommand::On, TeamMode::Off),
        Some(expected)
    );
    assert_eq!(
        team_settings_update_params(thread_id, TeamCommand::Off, TeamMode::LeadWorker),
        Some(ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            team: Some(ThreadTeamSettingsUpdate {
                mode: TeamMode::Off,
                ..Default::default()
            }),
            ..ThreadSettingsUpdateParams::default()
        })
    );
    assert_eq!(
        team_settings_update_params(thread_id, TeamCommand::Status, TeamMode::Off),
        None
    );
}

#[test]
fn profile_commands_keep_the_current_mode_in_sparse_updates() {
    let thread_id = ThreadId::new();
    assert_eq!(
        team_settings_update_params(
            thread_id,
            TeamCommand::ConfigureProfile {
                role: TeamRole::Lead,
                model: "gpt-5.6-sol".to_string(),
                effort: ReasoningEffort::High,
            },
            TeamMode::Off,
        ),
        Some(ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            team: Some(ThreadTeamSettingsUpdate {
                mode: TeamMode::Off,
                role: Some(TeamRole::Lead),
                model: Some("gpt-5.6-sol".to_string()),
                reasoning_effort: Some(ReasoningEffort::High),
            }),
            ..ThreadSettingsUpdateParams::default()
        })
    );
}
