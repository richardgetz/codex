use super::*;
use codex_protocol::protocol::RealtimeVoice;
use pretty_assertions::assert_eq;
use std::f32::consts::PI;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

fn sine_samples(frequency_hz: f32, duration_samples: usize) -> Vec<i16> {
    (0..duration_samples)
        .map(|index| {
            let phase = 2.0 * PI * frequency_hz * index as f32 / SAMPLE_RATE as f32;
            (phase.sin() * 0.35 * f32::from(i16::MAX)) as i16
        })
        .collect()
}

fn write_pcm_wav(path: &std::path::Path, samples: &[i16]) {
    let data_len = samples.len() * 2;
    let riff_len = 36 + data_len;
    let mut bytes = Vec::with_capacity(riff_len + 8);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(riff_len as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, bytes).expect("write WAV fixture");
}

#[test]
fn analyzes_and_decodes_bounded_reference_wav() {
    let temp_dir = tempfile::tempdir().expect("create temporary directory");
    let path = temp_dir.path().join("reference.wav");
    let source = sine_samples(130.0, SAMPLE_RATE as usize);
    write_pcm_wav(&path, &source);

    let decoded = decode_reference_file(&path).expect("decode WAV fixture");
    assert!(decoded.len() >= CALIBRATION_MIN_CAPTURE_SAMPLES);
    assert!(decoded.len() <= MAX_CALIBRATION_SAMPLES);

    let features = analyze_reference_file(&path).expect("analyze WAV fixture");
    assert!(
        (features.pitch_hz - 130.0).abs() < 5.0,
        "pitch={}",
        features.pitch_hz
    );
    assert!(features.duration_seconds >= 0.5);
}

#[test]
fn cancellable_reference_decode_stops_before_opening_a_cancelled_file() {
    let cancellation = AtomicBool::new(true);
    let error = decoder::decode_reference_file_cancellable(
        std::path::Path::new("does-not-exist.wav"),
        &cancellation,
    )
    .expect_err("cancelled decoding should not open the file");

    assert_eq!(error.to_string(), "reference audio analysis was cancelled");
    assert!(cancellation.load(Ordering::Acquire));
}

#[test]
fn ranks_the_closest_calibration_sample() {
    let duration = SAMPLE_RATE as usize;
    let reference =
        analyze_pcm(&sine_samples(130.0, duration), SAMPLE_RATE, 1).expect("analyze reference");
    let close = analyze_pcm(&sine_samples(132.0, duration), SAMPLE_RATE, 1)
        .expect("analyze close candidate");
    let far = analyze_pcm(&sine_samples(260.0, duration), SAMPLE_RATE, 1)
        .expect("analyze distant candidate");

    let result = rank_calibration_samples(
        reference,
        vec![
            VoiceCalibrationSample {
                voice: RealtimeVoice::Ash,
                features: far,
            },
            VoiceCalibrationSample {
                voice: RealtimeVoice::Arbor,
                features: close,
            },
        ],
    );

    assert_eq!(
        result.best().map(|(sample, _)| sample.voice),
        Some(RealtimeVoice::Arbor)
    );
}

#[test]
fn estimates_a_valid_effect_preset_from_reference_and_candidate_features() {
    let duration = SAMPLE_RATE as usize;
    let reference =
        analyze_pcm(&sine_samples(120.0, duration), SAMPLE_RATE, 1).expect("analyze reference");
    let candidate =
        analyze_pcm(&sine_samples(180.0, duration), SAMPLE_RATE, 1).expect("analyze candidate");

    let preset = estimate_effect_preset("calibrated-arbor".to_string(), reference, candidate)
        .expect("estimate effect preset");

    assert_eq!(preset.name, "calibrated-arbor");
    assert_eq!(preset.bands.len(), 5);
    assert!(preset.pitch_shift_semitones < 0.0);
    preset.validate().expect("estimated preset validates");
}

#[test]
fn calibration_run_advances_once_per_voice() {
    let reference = VoiceAudioFeatures {
        duration_seconds: 1.0,
        rms_db: -12.0,
        peak_db: -3.0,
        pitch_hz: 130.0,
        brightness_hz: 900.0,
        low_energy_ratio: 0.4,
        high_energy_ratio: 0.1,
        zero_crossing_rate: 0.02,
    };
    let mut run = VoiceCalibrationRun::new(
        ThreadId::new(),
        "reference.wav".into(),
        reference,
        vec![RealtimeVoice::Arbor, RealtimeVoice::Ash],
    );
    assert_eq!(run.current_voice(), Some(RealtimeVoice::Arbor));
    run.record_candidate(reference);
    assert_eq!(run.current_voice(), Some(RealtimeVoice::Ash));
    run.record_candidate(reference);
    assert!(run.is_complete());
    assert_eq!(run.samples.len(), 2);
}

#[test]
fn calibration_run_rejects_stale_session_start_notifications() {
    let reference = VoiceAudioFeatures {
        duration_seconds: 1.0,
        rms_db: -12.0,
        peak_db: -3.0,
        pitch_hz: 130.0,
        brightness_hz: 900.0,
        low_energy_ratio: 0.4,
        high_energy_ratio: 0.1,
        zero_crossing_rate: 0.02,
    };
    let mut run = VoiceCalibrationRun::new(
        ThreadId::new(),
        "reference.wav".into(),
        reference,
        vec![RealtimeVoice::Arbor],
    );

    run.begin_candidate("candidate-a".to_string());
    assert!(!run.mark_started(Some("candidate-b"), "submission-b".to_string()));
    assert!(!run.mark_started(None, "submission-without-session-id".to_string()));
    assert_eq!(run.realtime_submission_id, None);
    assert!(run.mark_started(Some("candidate-a"), "submission-a".to_string()));
    assert_eq!(run.realtime_submission_id.as_deref(), Some("submission-a"));
}

#[test]
fn calibration_run_fences_duplicate_legacy_session_start_notifications() {
    let reference = VoiceAudioFeatures {
        duration_seconds: 1.0,
        rms_db: -12.0,
        peak_db: -3.0,
        pitch_hz: 130.0,
        brightness_hz: 900.0,
        low_energy_ratio: 0.4,
        high_energy_ratio: 0.1,
        zero_crossing_rate: 0.02,
    };
    let mut run = VoiceCalibrationRun::new(
        ThreadId::new(),
        "reference.wav".into(),
        reference,
        vec![RealtimeVoice::Arbor],
    );

    run.begin_candidate("candidate-a".to_string());
    assert!(run.mark_started(Some("candidate-a"), String::new()));
    assert!(run.legacy_submission_id);
    assert!(!run.mark_started(Some("candidate-a"), String::new()));
}

#[test]
fn calibration_phrase_waits_for_the_connected_media_path() {
    let reference = VoiceAudioFeatures {
        duration_seconds: 1.0,
        rms_db: -12.0,
        peak_db: -3.0,
        pitch_hz: 130.0,
        brightness_hz: 900.0,
        low_energy_ratio: 0.4,
        high_energy_ratio: 0.1,
        zero_crossing_rate: 0.02,
    };
    let mut run = VoiceCalibrationRun::new(
        ThreadId::new(),
        "reference.wav".into(),
        reference,
        vec![RealtimeVoice::Arbor],
    );

    run.begin_candidate("candidate-a".to_string());
    assert!(run.speech_pending());
    run.mark_speech_sent();
    assert!(!run.speech_pending());
}
