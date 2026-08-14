use super::*;
use crate::realtime_voice_dsp::VoiceEffectProcessor;
use pretty_assertions::assert_eq;
use std::fs;

fn test_preset() -> VoiceEffectPreset {
    VoiceEffectPreset {
        version: PRESET_VERSION,
        name: "custom".to_string(),
        bands: vec![EqBand {
            kind: EqFilterKind::Peaking,
            frequency_hz: 2_000.0,
            gain_db: 6.0,
            q: 1.0,
        }],
        compressor: Some(CompressorSettings {
            threshold_db: -18.0,
            ratio: 2.0,
            attack_ms: 5.0,
            release_ms: 100.0,
            makeup_gain_db: 1.0,
        }),
        output_gain_db: -1.0,
        pitch_shift_semitones: 0.0,
        formant_shift_semitones: 0.0,
        saturation: 0.0,
        ring_mod_frequency_hz: 0.0,
        ring_mod_mix: 0.0,
        bitcrush_bits: 16,
        reverb_mix: 0.0,
    }
}

#[test]
fn custom_preset_round_trips_and_activation_persists() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let preset = test_preset();

    save_preset(codex_home.path(), &preset).expect("save preset");
    let activated = activate_preset(codex_home.path(), "custom").expect("activate preset");

    assert_eq!(activated, preset);
    assert_eq!(
        active_preset_name(codex_home.path()).expect("read active preset"),
        Some("custom".to_string())
    );
    assert_eq!(
        load_active_preset(codex_home.path()).expect("load active preset"),
        Some(preset)
    );
}

#[test]
fn saving_a_preset_replaces_the_previous_json_atomically() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let mut preset = test_preset();
    save_preset(codex_home.path(), &preset).expect("save initial preset");

    preset.output_gain_db = 4.0;
    save_preset(codex_home.path(), &preset).expect("replace preset");

    assert_eq!(
        load_named_preset(codex_home.path(), "custom").expect("load replaced preset"),
        preset
    );
}

#[test]
fn off_is_reserved_for_deactivating_a_preset() {
    assert!(
        preset_file_path(
            tempfile::tempdir().expect("temporary Codex home").path(),
            "off"
        )
        .is_err()
    );
}

#[test]
fn invalid_shelf_settings_are_rejected_before_dsp_construction() {
    let mut preset = test_preset();
    preset.bands[0] = EqBand {
        kind: EqFilterKind::HighShelf,
        frequency_hz: 2_000.0,
        gain_db: 24.0,
        q: 10.0,
    };

    assert!(VoiceEffectProcessor::new(&preset).is_err());
}

#[test]
fn processor_changes_stereo_pcm_without_changing_frame_count() {
    let preset = test_preset();
    let mut processor = VoiceEffectProcessor::new(&preset).expect("build processor");
    let mut samples = Vec::with_capacity(4_096 * 2);
    for index in 0..4_096 {
        let phase = 2.0 * std::f32::consts::PI * 2_000.0 * index as f32 / SAMPLE_RATE as f32;
        let sample = (phase.sin() * 20_000.0) as i16;
        samples.extend([sample, sample]);
    }
    let original = samples.clone();

    processor.process(&mut samples);

    assert_eq!(samples.len(), original.len());
    assert_ne!(samples, original);
    assert!(samples.chunks_exact(2).all(|frame| frame[0] == frame[1]));
}

#[test]
fn processor_supports_pitch_and_robot_texture_controls() {
    let mut preset = test_preset();
    preset.pitch_shift_semitones = -1.5;
    preset.formant_shift_semitones = -1.0;
    preset.saturation = 0.2;
    preset.ring_mod_frequency_hz = 35.0;
    preset.ring_mod_mix = 0.15;
    preset.bitcrush_bits = 12;
    preset.reverb_mix = 0.1;
    let mut processor = VoiceEffectProcessor::new(&preset).expect("build processor");
    let mut samples = vec![12_000_i16; 12_000];

    processor.process(&mut samples);

    assert!(samples.iter().any(|sample| *sample != 0));
}

#[test]
fn deactivating_a_preset_disables_processing_on_the_next_session() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    activate_preset(codex_home.path(), "jarvis").expect("activate built-in preset");

    deactivate_preset(codex_home.path()).expect("deactivate preset");

    assert_eq!(
        active_preset_name(codex_home.path()).expect("read active preset"),
        Some("off".to_string())
    );
    assert_eq!(
        load_active_preset(codex_home.path()).expect("load active preset"),
        None
    );
}

#[test]
fn shared_presets_are_loaded_case_insensitively() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let preset = test_preset();
    let directory = codex_home.path().join(PRESET_DIRECTORY);
    fs::create_dir_all(&directory).expect("create preset directory");
    fs::write(
        directory.join("Custom.json"),
        serde_json::to_vec(&preset).expect("serialize preset"),
    )
    .expect("write shared preset");

    assert_eq!(
        load_named_preset(codex_home.path(), "custom").expect("load shared preset"),
        preset
    );
}
