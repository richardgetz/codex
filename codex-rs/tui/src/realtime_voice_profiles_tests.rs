use super::*;
use crate::realtime_voice_effects::active_preset_name;
use codex_protocol::protocol::RealtimeVoice;
use pretty_assertions::assert_eq;
use std::fs;

#[test]
fn built_in_profile_round_trips_and_activates_its_effect() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");

    let profile = activate_profile(codex_home.path(), "jarvis").expect("activate profile");

    assert_eq!(profile.voice, RealtimeVoice::Arbor);
    assert_eq!(profile.effect, "jarvis");
    assert_eq!(
        active_profile_name(codex_home.path()).expect("read active profile"),
        Some("jarvis".to_string())
    );
    assert_eq!(
        active_preset_name(codex_home.path()).expect("read active effect"),
        Some("jarvis".to_string())
    );
    assert_eq!(
        load_active_profile(codex_home.path()).expect("load active profile"),
        Some(profile)
    );
}

#[test]
fn custom_profiles_can_be_shared_as_json() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let profile = VoiceProfile {
        version: PROFILE_VERSION,
        name: "robot-cove".to_string(),
        voice: RealtimeVoice::Cove,
        effect: "jarvis".to_string(),
    };

    save_profile(codex_home.path(), &profile).expect("save profile");

    assert_eq!(
        load_named_profile(codex_home.path(), "robot-cove").expect("load profile"),
        profile
    );
    assert!(
        profile_file_path(codex_home.path(), "robot-cove")
            .expect("profile path")
            .exists()
    );
}

#[test]
fn profiles_must_reference_an_existing_effect() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let profile = VoiceProfile {
        version: PROFILE_VERSION,
        name: "missing-effect".to_string(),
        voice: RealtimeVoice::Cove,
        effect: "does-not-exist".to_string(),
    };

    assert!(save_profile(codex_home.path(), &profile).is_err());
}

#[test]
fn shared_profiles_are_loaded_case_insensitively() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let profile = VoiceProfile {
        version: PROFILE_VERSION,
        name: "robot-cove".to_string(),
        voice: RealtimeVoice::Cove,
        effect: "jarvis".to_string(),
    };
    let directory = codex_home.path().join(PROFILE_DIRECTORY);
    fs::create_dir_all(&directory).expect("create profile directory");
    fs::write(
        directory.join("Robot-Cove.json"),
        serde_json::to_vec(&profile).expect("serialize profile"),
    )
    .expect("write shared profile");

    assert_eq!(
        load_named_profile(codex_home.path(), "robot-cove").expect("load shared profile"),
        profile
    );
}

#[test]
fn profile_file_name_must_match_embedded_name() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let directory = codex_home.path().join(PROFILE_DIRECTORY);
    fs::create_dir_all(&directory).expect("create profile directory");
    fs::write(
        directory.join("robot-cove.json"),
        r#"{"version":1,"name":"other","voice":"cove","effect":"jarvis"}"#,
    )
    .expect("write mismatched profile");

    assert!(load_named_profile(codex_home.path(), "robot-cove").is_err());
}
