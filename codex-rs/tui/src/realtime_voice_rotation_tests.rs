use super::select_startup_voice;
use codex_config::config_toml::RealtimeConfig;
use codex_protocol::protocol::RealtimeVoice;

#[test]
fn rotation_advances_persisted_cursor_between_process_starts() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let config = RealtimeConfig {
        voice_rotation: Some(vec![RealtimeVoice::Arbor, RealtimeVoice::Cove]),
        ..RealtimeConfig::default()
    };

    assert_eq!(
        select_startup_voice(&config, codex_home.path()),
        Some(RealtimeVoice::Arbor)
    );
    assert_eq!(
        select_startup_voice(&config, codex_home.path()),
        Some(RealtimeVoice::Cove)
    );
    assert_eq!(
        select_startup_voice(&config, codex_home.path()),
        Some(RealtimeVoice::Arbor)
    );
}

#[test]
fn absent_rotation_does_not_override_configured_voice() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let config = RealtimeConfig {
        voice: Some(RealtimeVoice::Cedar),
        ..RealtimeConfig::default()
    };

    assert_eq!(select_startup_voice(&config, codex_home.path()), None);
}
