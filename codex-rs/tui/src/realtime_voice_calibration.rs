//! Local reference analysis and GPT-Live voice calibration primitives.
//!
//! Calibration never creates a new server-side voice. It measures a bounded local reference
//! clip, compares it with raw audio captured from the existing GPT-Live V3 voices, and produces a
//! client-side effect preset that can be refined in the voice tuner.

use anyhow::Result;
use anyhow::bail;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RealtimeVoice;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::realtime_voice::SAMPLE_RATE;
use crate::realtime_voice_effects::VoiceEffectPreset;
use crate::realtime_voice_effects::jarvis_preset;

#[path = "realtime_voice_calibration_decoder.rs"]
mod decoder;

use decoder::decode_reference_file;
use decoder::decode_reference_file_cancellable;
use decoder::resample_mono;
use decoder::trim_silence;

pub(crate) const CALIBRATION_PHRASE: &str = "Certainly, sir. Calibrating the voice profile now. All systems are operating within normal parameters.";
pub(crate) const CALIBRATION_POLL_INTERVAL: Duration = Duration::from_millis(125);
pub(crate) const CALIBRATION_SETUP_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CALIBRATION_PREPARATION_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const CALIBRATION_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const CALIBRATION_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const CALIBRATION_STOP_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const CALIBRATION_STABLE_POLLS: u8 = 6;
pub(crate) const CALIBRATION_MIN_CAPTURE_SAMPLES: usize = SAMPLE_RATE as usize / 2;
pub(crate) const MAX_CALIBRATION_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_CALIBRATION_SECONDS: u32 = 30;
const MAX_CALIBRATION_SAMPLES: usize = SAMPLE_RATE as usize * MAX_CALIBRATION_SECONDS as usize;
const ANALYSIS_FRAME_SIZE: usize = 2_048;
const ANALYSIS_FRAME_HOP: usize = 1_024;

/// Aggregate voice characteristics used for ranking a reference against a GPT-Live sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VoiceAudioFeatures {
    pub(crate) duration_seconds: f32,
    pub(crate) rms_db: f32,
    pub(crate) peak_db: f32,
    pub(crate) pitch_hz: f32,
    pub(crate) brightness_hz: f32,
    pub(crate) low_energy_ratio: f32,
    pub(crate) high_energy_ratio: f32,
    pub(crate) zero_crossing_rate: f32,
}

/// One raw calibration sample associated with a base GPT-Live voice.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VoiceCalibrationSample {
    pub(crate) voice: RealtimeVoice,
    pub(crate) features: VoiceAudioFeatures,
}

/// Result of the cancellable preparation phase before a calibration session starts.
#[derive(Debug)]
pub(crate) struct VoiceCalibrationPreparation {
    pub(crate) thread_id: ThreadId,
    pub(crate) reference_path: PathBuf,
    pub(crate) reference: VoiceAudioFeatures,
    pub(crate) voices: Vec<RealtimeVoice>,
}

/// Ranked result returned after all candidate voices have been sampled.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VoiceCalibrationResult {
    pub(crate) ranked: Vec<(VoiceCalibrationSample, f32)>,
}

impl VoiceCalibrationResult {
    pub(crate) fn best(&self) -> Option<(VoiceCalibrationSample, f32)> {
        self.ranked.first().copied()
    }
}

/// Mutable state for one event-driven calibration run in the TUI.
#[derive(Debug)]
pub(crate) struct VoiceCalibrationRun {
    pub(crate) run_id: uuid::Uuid,
    pub(crate) thread_id: ThreadId,
    pub(crate) reference_path: PathBuf,
    pub(crate) reference: VoiceAudioFeatures,
    pub(crate) voices: Vec<RealtimeVoice>,
    pub(crate) samples: Vec<VoiceCalibrationSample>,
    pub(crate) candidate_index: usize,
    pub(crate) candidate_started_at: Instant,
    pub(crate) last_capture_samples: usize,
    pub(crate) stable_polls: u8,
    pub(crate) saw_audio: bool,
    pub(crate) capture_timed_out: bool,
    pub(crate) speech_sent: bool,
    pub(crate) requested_realtime_session_id: Option<String>,
    pub(crate) realtime_submission_id: Option<String>,
    pub(crate) started: bool,
    pub(crate) legacy_submission_id: bool,
    pub(crate) waiting_for_close: bool,
    pub(crate) next_candidate_pending: bool,
    pub(crate) finish_pending: bool,
    pub(crate) close_wait_started_at: Option<Instant>,
    pub(crate) error: Option<String>,
}

impl VoiceCalibrationRun {
    pub(crate) fn new(
        thread_id: ThreadId,
        reference_path: PathBuf,
        reference: VoiceAudioFeatures,
        voices: Vec<RealtimeVoice>,
    ) -> Self {
        Self {
            run_id: uuid::Uuid::new_v4(),
            thread_id,
            reference_path,
            reference,
            voices,
            samples: Vec::new(),
            candidate_index: 0,
            candidate_started_at: Instant::now(),
            last_capture_samples: 0,
            stable_polls: 0,
            saw_audio: false,
            capture_timed_out: false,
            speech_sent: false,
            requested_realtime_session_id: None,
            realtime_submission_id: None,
            started: false,
            legacy_submission_id: false,
            waiting_for_close: false,
            next_candidate_pending: false,
            finish_pending: false,
            close_wait_started_at: None,
            error: None,
        }
    }

    pub(crate) fn current_voice(&self) -> Option<RealtimeVoice> {
        self.voices.get(self.candidate_index).copied()
    }

    pub(crate) fn begin_candidate(&mut self, requested_realtime_session_id: String) {
        self.candidate_started_at = Instant::now();
        self.last_capture_samples = 0;
        self.stable_polls = 0;
        self.saw_audio = false;
        self.capture_timed_out = false;
        self.speech_sent = false;
        self.requested_realtime_session_id = Some(requested_realtime_session_id);
        self.realtime_submission_id = None;
        self.started = false;
        self.legacy_submission_id = false;
        self.waiting_for_close = false;
        self.next_candidate_pending = false;
        self.finish_pending = false;
        self.close_wait_started_at = None;
        self.error = None;
    }

    pub(crate) fn set_pending_submission_id(&mut self, submission_id: String) {
        self.realtime_submission_id = Some(submission_id);
    }

    pub(crate) fn speech_pending(&self) -> bool {
        !self.speech_sent
    }

    pub(crate) fn mark_speech_sent(&mut self) {
        self.speech_sent = true;
        self.candidate_started_at = Instant::now();
        self.last_capture_samples = 0;
        self.stable_polls = 0;
        self.saw_audio = false;
        self.capture_timed_out = false;
    }

    pub(crate) fn mark_started(
        &mut self,
        requested_realtime_session_id: Option<&str>,
        submission_id: String,
    ) -> bool {
        if self.started {
            return false;
        }
        if self
            .requested_realtime_session_id
            .as_deref()
            .is_some_and(|expected| requested_realtime_session_id != Some(expected))
        {
            return false;
        }
        self.started = true;
        self.legacy_submission_id = submission_id.is_empty();
        self.realtime_submission_id = (!submission_id.is_empty()).then_some(submission_id);
        true
    }

    pub(crate) fn begin_wait_for_close(&mut self, finish_pending: bool) {
        self.waiting_for_close = true;
        self.next_candidate_pending = !finish_pending;
        self.finish_pending = finish_pending;
        self.close_wait_started_at = Some(Instant::now());
    }

    pub(crate) fn mark_closed(&mut self) {
        if self.waiting_for_close {
            self.waiting_for_close = false;
            self.close_wait_started_at = None;
        }
    }

    pub(crate) fn close_wait_expired(&self) -> bool {
        self.close_wait_started_at
            .is_some_and(|started_at| started_at.elapsed() >= CALIBRATION_CLOSE_TIMEOUT)
    }

    pub(crate) fn observe_capture(&mut self, sample_count: usize) -> bool {
        if sample_count >= CALIBRATION_MIN_CAPTURE_SAMPLES {
            self.saw_audio = true;
        }
        if sample_count == self.last_capture_samples {
            self.stable_polls = self.stable_polls.saturating_add(1);
        } else {
            self.stable_polls = 0;
            self.last_capture_samples = sample_count;
        }
        if self.saw_audio && self.stable_polls >= CALIBRATION_STABLE_POLLS {
            return true;
        }
        if self.candidate_started_at.elapsed() >= CALIBRATION_RESPONSE_TIMEOUT {
            self.capture_timed_out = true;
            return true;
        }
        false
    }

    pub(crate) fn record_candidate(&mut self, features: VoiceAudioFeatures) {
        if let Some(voice) = self.current_voice() {
            self.samples
                .push(VoiceCalibrationSample { voice, features });
        }
        self.candidate_index += 1;
    }

    pub(crate) fn skip_candidate(&mut self) {
        self.candidate_index += 1;
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.candidate_index >= self.voices.len()
    }
}

/// Extracts stable, intentionally approximate voice characteristics from PCM samples.
pub(crate) fn analyze_pcm(
    samples: &[i16],
    sample_rate: u32,
    channels: u16,
) -> Result<VoiceAudioFeatures> {
    let channels = usize::from(channels);
    if channels == 0 || samples.is_empty() {
        bail!("cannot analyze empty PCM audio");
    }
    let mono = if channels == 1 {
        samples.to_vec()
    } else {
        samples
            .chunks_exact(channels)
            .map(|frame| {
                let sum = frame.iter().map(|sample| i32::from(*sample)).sum::<i32>();
                (sum / frame.len() as i32) as i16
            })
            .collect()
    };
    let mono = resample_mono(&mono, sample_rate.max(1), SAMPLE_RATE);
    analyze_mono(&trim_silence(&mono), SAMPLE_RATE)
}

pub(crate) fn analyze_reference_file(path: &Path) -> Result<VoiceAudioFeatures> {
    let samples = decode_reference_file(path)?;
    analyze_mono(&samples, SAMPLE_RATE)
}

pub(crate) fn analyze_reference_file_cancellable(
    path: &Path,
    cancellation: &AtomicBool,
) -> Result<VoiceAudioFeatures> {
    let samples = decode_reference_file_cancellable(path, cancellation)?;
    analyze_mono_cancellable(&samples, SAMPLE_RATE, cancellation)
}

fn analyze_mono(samples: &[i16], sample_rate: u32) -> Result<VoiceAudioFeatures> {
    analyze_mono_with_cancellation(samples, sample_rate, || false)
}

fn analyze_mono_cancellable(
    samples: &[i16],
    sample_rate: u32,
    cancellation: &AtomicBool,
) -> Result<VoiceAudioFeatures> {
    analyze_mono_with_cancellation(samples, sample_rate, || {
        cancellation.load(Ordering::Acquire)
    })
}

fn analyze_mono_with_cancellation(
    samples: &[i16],
    sample_rate: u32,
    is_cancelled: impl Fn() -> bool,
) -> Result<VoiceAudioFeatures> {
    if is_cancelled() {
        bail!("reference audio analysis was cancelled");
    }
    if samples.len() < CALIBRATION_MIN_CAPTURE_SAMPLES {
        bail!("audio must contain at least 0.5 seconds of audible samples");
    }
    let normalized = samples
        .iter()
        .map(|sample| f32::from(*sample) / f32::from(i16::MAX))
        .collect::<Vec<_>>();
    if is_cancelled() {
        bail!("reference audio analysis was cancelled");
    }
    let rms = (normalized.iter().map(|sample| sample * sample).sum::<f32>()
        / normalized.len() as f32)
        .sqrt();
    let peak = normalized
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0_f32, f32::max);
    if is_cancelled() {
        bail!("reference audio analysis was cancelled");
    }
    let zero_crossing_rate = normalized
        .windows(2)
        .filter(|pair| (pair[0] >= 0.0) != (pair[1] >= 0.0))
        .count() as f32
        / normalized.len() as f32;
    if is_cancelled() {
        bail!("reference audio analysis was cancelled");
    }

    let mut low_energy = 0.0_f32;
    let mut high_energy = 0.0_f32;
    let mut weighted_frequency = 0.0_f32;
    let mut total_frequency_energy = 0.0_f32;
    let mut frame_count = 0.0_f32;
    for frame_start in
        (0..normalized.len().saturating_sub(ANALYSIS_FRAME_SIZE)).step_by(ANALYSIS_FRAME_HOP)
    {
        if is_cancelled() {
            bail!("reference audio analysis was cancelled");
        }
        let frame = &normalized[frame_start..frame_start + ANALYSIS_FRAME_SIZE];
        let frequencies = [
            120.0, 220.0, 320.0, 450.0, 700.0, 1_000.0, 1_500.0, 2_200.0, 3_000.0, 4_500.0, 6_000.0,
        ];
        let mut frame_total = 0.0_f32;
        for frequency in frequencies {
            let energy = goertzel_power(frame, sample_rate, frequency);
            frame_total += energy;
            weighted_frequency += energy * frequency;
            if frequency < 400.0 {
                low_energy += energy;
            }
            if frequency >= 2_200.0 {
                high_energy += energy;
            }
        }
        total_frequency_energy += frame_total;
        frame_count += 1.0;
    }
    if frame_count == 0.0 || total_frequency_energy <= f32::EPSILON {
        bail!("audio did not contain enough analyzable signal");
    }
    let pitch_hz = estimate_pitch(&normalized, sample_rate, &is_cancelled)?;
    Ok(VoiceAudioFeatures {
        duration_seconds: normalized.len() as f32 / sample_rate as f32,
        rms_db: amplitude_to_db(rms),
        peak_db: amplitude_to_db(peak),
        pitch_hz,
        brightness_hz: weighted_frequency / total_frequency_energy,
        low_energy_ratio: low_energy / total_frequency_energy,
        high_energy_ratio: high_energy / total_frequency_energy,
        zero_crossing_rate,
    })
}

fn goertzel_power(frame: &[f32], sample_rate: u32, frequency: f32) -> f32 {
    let omega = 2.0 * std::f32::consts::PI * frequency / sample_rate as f32;
    let coefficient = 2.0 * omega.cos();
    let mut previous = 0.0_f32;
    let mut previous_previous = 0.0_f32;
    for sample in frame {
        let current = *sample + coefficient * previous - previous_previous;
        previous_previous = previous;
        previous = current;
    }
    (previous_previous * previous_previous + previous * previous
        - coefficient * previous * previous_previous)
        / frame.len().max(1) as f32
}

fn estimate_pitch(
    samples: &[f32],
    sample_rate: u32,
    is_cancelled: &impl Fn() -> bool,
) -> Result<f32> {
    if is_cancelled() {
        bail!("reference audio analysis was cancelled");
    }
    let decimation = 4;
    let downsampled = samples
        .iter()
        .step_by(decimation)
        .copied()
        .collect::<Vec<_>>();
    let rate = sample_rate / decimation as u32;
    let frame_size = 2_048.min(downsampled.len());
    if frame_size < 256 {
        return Ok(0.0);
    }
    let min_lag = (rate / 350).max(1) as usize;
    let max_lag = (rate / 70).min(frame_size.saturating_sub(1) as u32) as usize;
    let mut pitches = Vec::new();
    for start in (0..downsampled.len().saturating_sub(frame_size)).step_by(frame_size) {
        let frame = &downsampled[start..start + frame_size];
        let energy = frame.iter().map(|sample| sample * sample).sum::<f32>();
        if energy < 0.001 {
            continue;
        }
        let mut best_lag = min_lag;
        let mut best_correlation = 0.0_f32;
        for lag in min_lag..=max_lag {
            if is_cancelled() {
                bail!("reference audio analysis was cancelled");
            }
            let correlation = frame[..frame_size - lag]
                .iter()
                .zip(&frame[lag..])
                .map(|(left, right)| left * right)
                .sum::<f32>();
            if correlation > best_correlation {
                best_correlation = correlation;
                best_lag = lag;
            }
        }
        if best_correlation / energy > 0.25 {
            pitches.push(rate as f32 / best_lag as f32);
        }
        if pitches.len() == 16 {
            break;
        }
    }
    pitches.sort_by(f32::total_cmp);
    Ok(pitches.get(pitches.len() / 2).copied().unwrap_or(0.0))
}

fn amplitude_to_db(amplitude: f32) -> f32 {
    20.0 * amplitude.max(1.0e-6).log10()
}

pub(crate) fn rank_calibration_samples(
    reference: VoiceAudioFeatures,
    samples: Vec<VoiceCalibrationSample>,
) -> VoiceCalibrationResult {
    let mut ranked = samples
        .into_iter()
        .map(|sample| {
            let score = feature_distance(reference, sample.features);
            (sample, score)
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| left.1.total_cmp(&right.1));
    VoiceCalibrationResult { ranked }
}

fn feature_distance(reference: VoiceAudioFeatures, candidate: VoiceAudioFeatures) -> f32 {
    let pitch_distance = match (reference.pitch_hz, candidate.pitch_hz) {
        (reference, candidate) if reference > 0.0 && candidate > 0.0 => {
            (12.0 * (reference / candidate).log2()).abs() / 12.0
        }
        _ => 1.0,
    };
    ((reference.rms_db - candidate.rms_db).abs() / 24.0) * 0.15
        + pitch_distance * 0.35
        + ((reference.brightness_hz - candidate.brightness_hz).abs() / 4_000.0) * 0.25
        + (reference.low_energy_ratio - candidate.low_energy_ratio).abs() * 0.1
        + (reference.high_energy_ratio - candidate.high_energy_ratio).abs() * 0.1
        + ((reference.zero_crossing_rate - candidate.zero_crossing_rate).abs() * 10.0) * 0.05
}

pub(crate) fn estimate_effect_preset(
    name: String,
    reference: VoiceAudioFeatures,
    candidate: VoiceAudioFeatures,
) -> Result<VoiceEffectPreset> {
    let mut preset = jarvis_preset();
    preset.name = name;
    if reference.pitch_hz > 0.0 && candidate.pitch_hz > 0.0 {
        preset.pitch_shift_semitones =
            (12.0 * (reference.pitch_hz / candidate.pitch_hz).log2()).clamp(-6.0, 6.0);
    }
    let brightness_delta =
        (reference.brightness_hz - candidate.brightness_hz).clamp(-2_000.0, 2_000.0);
    preset.formant_shift_semitones = (brightness_delta / 900.0).clamp(-3.0, 3.0);
    preset.output_gain_db = ((reference.rms_db - candidate.rms_db) * 0.5).clamp(-6.0, 6.0);
    preset.saturation =
        (0.05 + (reference.high_energy_ratio - candidate.high_energy_ratio).abs()).clamp(0.0, 0.35);
    preset.ring_mod_frequency_hz = 35.0;
    preset.ring_mod_mix = if reference.zero_crossing_rate > candidate.zero_crossing_rate * 1.15 {
        0.08
    } else {
        0.0
    };
    preset.validate()?;
    Ok(preset)
}

pub(crate) fn format_ranked_candidates(result: &VoiceCalibrationResult) -> String {
    result
        .ranked
        .iter()
        .take(5)
        .enumerate()
        .map(|(index, (sample, score))| {
            format!("{}. {} ({score:.3})", index + 1, sample.voice.wire_name())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
#[path = "realtime_voice_calibration_tests.rs"]
mod tests;
