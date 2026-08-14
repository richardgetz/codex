use super::select_startup_selection;
use super::select_startup_voice;
use crate::realtime_voice_profiles::VoiceProfile;
use crate::realtime_voice_profiles::save_profile;
use codex_config::config_toml::RealtimeConfig;
use codex_protocol::protocol::RealtimeVoice;
use std::fs;

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

#[test]
fn profile_rotation_runs_after_the_configured_base_voices() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    save_profile(
        codex_home.path(),
        &VoiceProfile {
            version: 1,
            name: "robot-cove".to_string(),
            voice: RealtimeVoice::Cove,
            effect: "jarvis".to_string(),
        },
    )
    .expect("save profile");
    let config = RealtimeConfig {
        voice_rotation: Some(vec![RealtimeVoice::Arbor]),
        voice_profile_rotation: Some(vec!["robot-cove".to_string()]),
        ..RealtimeConfig::default()
    };

    assert_eq!(
        select_startup_selection(&config, codex_home.path()),
        Some(super::SelectedVoice {
            voice: RealtimeVoice::Arbor,
            profile: None,
        })
    );
    assert_eq!(
        select_startup_selection(&config, codex_home.path()),
        Some(super::SelectedVoice {
            voice: RealtimeVoice::Cove,
            profile: Some("robot-cove".to_string()),
        })
    );
}

#[test]
fn legacy_voice_rotation_state_keeps_its_cursor() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    fs::write(
        codex_home.path().join("realtime_voice_rotation.json"),
        r#"{"voices":["arbor","cove"],"next_index":1}"#,
    )
    .expect("write legacy rotation state");
    let config = RealtimeConfig {
        voice_rotation: Some(vec![RealtimeVoice::Arbor, RealtimeVoice::Cove]),
        ..RealtimeConfig::default()
    };

    assert_eq!(
        select_startup_voice(&config, codex_home.path()),
        Some(RealtimeVoice::Cove)
    );
}

#[test]
fn busy_rotation_lock_preserves_the_persisted_profile_selection() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    fs::write(
        codex_home.path().join("realtime_voice_rotation.lock"),
        b"busy",
    )
    .expect("write rotation lock");
    let config = RealtimeConfig {
        voice_rotation: Some(vec![RealtimeVoice::Arbor]),
        ..RealtimeConfig::default()
    };

    assert_eq!(select_startup_selection(&config, codex_home.path()), None);
}
