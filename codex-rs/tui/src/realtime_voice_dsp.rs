//! Stateful DSP for client-side GPT-Live output effects.

use anyhow::Result;
use std::collections::VecDeque;

use crate::realtime_voice::SAMPLE_RATE;
use crate::realtime_voice_effects::CompressorSettings;
use crate::realtime_voice_effects::EqBand;
use crate::realtime_voice_effects::EqFilterKind;
use crate::realtime_voice_effects::VoiceEffectPreset;

const PITCH_GRAIN_HOP: usize = 240;
const PITCH_GRAIN_SIZE: usize = PITCH_GRAIN_HOP * 2;
const REVERB_DELAY_SAMPLES: usize = 2_400;

#[derive(Clone, Copy)]
struct BiquadCoefficients {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

struct Biquad {
    coefficients: BiquadCoefficients,
    z1: f32,
    z2: f32,
}

struct StereoBiquad {
    left: Biquad,
    right: Biquad,
}

struct StereoCompressor {
    threshold_db: f32,
    ratio: f32,
    attack_coefficient: f32,
    release_coefficient: f32,
    makeup_gain: f32,
    envelope: f32,
    gain: f32,
}

struct StereoPitchShifter {
    buffer: VecDeque<[f32; 2]>,
    buffer_start: usize,
    output_index: usize,
    ratio: f32,
    latency: usize,
}

struct StereoReverb {
    delay: Vec<[f32; 2]>,
    index: usize,
    mix: f32,
}

/// Stateful output processor used by one GPT-Live remote audio track.
pub(crate) struct VoiceEffectProcessor {
    pitch_shifter: Option<StereoPitchShifter>,
    formant_filters: Vec<StereoBiquad>,
    filters: Vec<StereoBiquad>,
    compressor: Option<StereoCompressor>,
    saturation: f32,
    ring_mod_frequency_hz: f32,
    ring_mod_mix: f32,
    ring_mod_phase: f32,
    ring_mod_step: f32,
    bitcrush_bits: u8,
    reverb: Option<StereoReverb>,
    output_gain: f32,
}

impl VoiceEffectProcessor {
    pub(crate) fn new(preset: &VoiceEffectPreset) -> Result<Self> {
        preset.validate()?;
        let pitch_shifter = (preset.pitch_shift_semitones.abs() > f32::EPSILON)
            .then(|| StereoPitchShifter::new(preset.pitch_shift_semitones));
        let formant_filters = formant_filters(preset.formant_shift_semitones);
        let filters = preset.bands.iter().map(StereoBiquad::new).collect();
        let compressor = preset.compressor.as_ref().map(StereoCompressor::new);
        let reverb = (preset.reverb_mix > 0.0).then(|| StereoReverb::new(preset.reverb_mix));
        Ok(Self {
            pitch_shifter,
            formant_filters,
            filters,
            compressor,
            saturation: preset.saturation,
            ring_mod_frequency_hz: preset.ring_mod_frequency_hz,
            ring_mod_mix: preset.ring_mod_mix,
            ring_mod_phase: 0.0,
            ring_mod_step: 2.0 * std::f32::consts::PI * preset.ring_mod_frequency_hz
                / SAMPLE_RATE as f32,
            bitcrush_bits: preset.bitcrush_bits,
            reverb,
            output_gain: decibels_to_gain(preset.output_gain_db),
        })
    }

    pub(crate) fn process(&mut self, samples: &mut [i16]) {
        for frame in samples.chunks_exact_mut(2) {
            let mut left = f32::from(frame[0]) / f32::from(i16::MAX);
            let mut right = f32::from(frame[1]) / f32::from(i16::MAX);
            if let Some(pitch_shifter) = &mut self.pitch_shifter {
                (left, right) = pitch_shifter.process(left, right);
            }
            for filter in &mut self.formant_filters {
                (left, right) = filter.process(left, right);
            }
            for filter in &mut self.filters {
                (left, right) = filter.process(left, right);
            }
            if let Some(compressor) = &mut self.compressor {
                (left, right) = compressor.process(left, right);
            }
            (left, right) = self.process_texture(left, right);
            if let Some(reverb) = &mut self.reverb {
                (left, right) = reverb.process(left, right);
            }
            frame[0] = float_to_sample(left * self.output_gain);
            frame[1] = float_to_sample(right * self.output_gain);
        }
    }

    fn process_texture(&mut self, mut left: f32, mut right: f32) -> (f32, f32) {
        if self.ring_mod_mix > 0.0 && self.ring_mod_frequency_hz > 0.0 {
            let modulator = self.ring_mod_phase.sin();
            let dry_mix = 1.0 - self.ring_mod_mix;
            left = left * dry_mix + left * modulator * self.ring_mod_mix;
            right = right * dry_mix + right * modulator * self.ring_mod_mix;
            self.ring_mod_phase =
                (self.ring_mod_phase + self.ring_mod_step).rem_euclid(2.0 * std::f32::consts::PI);
        }
        if self.saturation > 0.0 {
            let drive = 1.0 + self.saturation * 9.0;
            let normalization = drive.tanh();
            let dry_mix = 1.0 - self.saturation;
            left = left * dry_mix + (left * drive).tanh() / normalization * self.saturation;
            right = right * dry_mix + (right * drive).tanh() / normalization * self.saturation;
        }
        if self.bitcrush_bits < 16 {
            left = quantize(left, self.bitcrush_bits);
            right = quantize(right, self.bitcrush_bits);
        }
        (left, right)
    }
}

impl StereoPitchShifter {
    fn new(semitones: f32) -> Self {
        let ratio = 2.0_f32.powf(semitones / 12.0);
        let lookahead = ((ratio - 1.0).max(0.0) * PITCH_GRAIN_SIZE as f32).ceil() as usize;
        Self {
            buffer: VecDeque::new(),
            buffer_start: 0,
            output_index: 0,
            ratio,
            latency: PITCH_GRAIN_SIZE + lookahead,
        }
    }

    fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.buffer.push_back([left, right]);
        let output = self
            .output_index
            .checked_sub(self.latency)
            .map(|source_index| {
                let grain_start = (source_index / PITCH_GRAIN_HOP) * PITCH_GRAIN_HOP;
                let phase = source_index % PITCH_GRAIN_HOP;
                let current = self
                    .read(self.position(grain_start, phase))
                    .unwrap_or([0.0; 2]);
                if grain_start >= PITCH_GRAIN_HOP {
                    let previous = self
                        .read(self.position(grain_start - PITCH_GRAIN_HOP, PITCH_GRAIN_HOP + phase))
                        .unwrap_or([0.0; 2]);
                    let current_weight = phase as f32 / PITCH_GRAIN_HOP as f32;
                    [
                        previous[0] * (1.0 - current_weight) + current[0] * current_weight,
                        previous[1] * (1.0 - current_weight) + current[1] * current_weight,
                    ]
                } else {
                    current
                }
            })
            .unwrap_or([0.0; 2]);
        self.output_index += 1;
        self.trim();
        (output[0], output[1])
    }

    fn position(&self, grain_start: usize, phase: usize) -> f32 {
        grain_start as f32 + phase as f32 * self.ratio
    }

    fn read(&self, position: f32) -> Option<[f32; 2]> {
        let first_index = position.floor() as usize;
        let fraction = position.fract();
        let first = self.frame(first_index)?;
        let second = self.frame(first_index + 1).unwrap_or(first);
        Some([
            first[0] + (second[0] - first[0]) * fraction,
            first[1] + (second[1] - first[1]) * fraction,
        ])
    }

    fn frame(&self, index: usize) -> Option<[f32; 2]> {
        index
            .checked_sub(self.buffer_start)
            .and_then(|offset| self.buffer.get(offset).copied())
    }

    fn trim(&mut self) {
        let source_index = self
            .output_index
            .saturating_sub(1)
            .saturating_sub(self.latency);
        let grain_start = (source_index / PITCH_GRAIN_HOP) * PITCH_GRAIN_HOP;
        let phase = source_index % PITCH_GRAIN_HOP;
        let current_position = self.position(grain_start, phase);
        let previous_position = if grain_start >= PITCH_GRAIN_HOP {
            self.position(grain_start - PITCH_GRAIN_HOP, PITCH_GRAIN_HOP + phase)
        } else {
            current_position
        };
        let keep_from = current_position.min(previous_position).floor() as usize;
        let keep_from = keep_from.saturating_sub(PITCH_GRAIN_SIZE * 2);
        while self.buffer_start < keep_from && !self.buffer.is_empty() {
            self.buffer.pop_front();
            self.buffer_start += 1;
        }
    }
}

#[cfg(test)]
#[path = "realtime_voice_dsp_tests.rs"]
mod tests;

impl StereoReverb {
    fn new(mix: f32) -> Self {
        Self {
            delay: vec![[0.0; 2]; REVERB_DELAY_SAMPLES],
            index: 0,
            mix,
        }
    }

    fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let delayed = self.delay[self.index];
        self.delay[self.index] = [left + delayed[0] * 0.35, right + delayed[1] * 0.35];
        self.index = (self.index + 1) % self.delay.len();
        let dry_mix = 1.0 - self.mix;
        (
            left * dry_mix + delayed[0] * self.mix,
            right * dry_mix + delayed[1] * self.mix,
        )
    }
}

fn formant_filters(semitones: f32) -> Vec<StereoBiquad> {
    if semitones.abs() <= f32::EPSILON {
        return Vec::new();
    }
    let ratio = 2.0_f32.powf(semitones / 12.0);
    [(500.0, 1.5, 1.0), (1_500.0, 1.0, 1.0), (2_500.0, 1.0, 1.2)]
        .into_iter()
        .flat_map(|(frequency, gain, q)| {
            let shifted_frequency = (frequency * ratio).clamp(20.0, SAMPLE_RATE as f32 * 0.45);
            [
                StereoBiquad::new(&EqBand {
                    kind: EqFilterKind::Peaking,
                    frequency_hz: frequency,
                    gain_db: -gain,
                    q,
                }),
                StereoBiquad::new(&EqBand {
                    kind: EqFilterKind::Peaking,
                    frequency_hz: shifted_frequency,
                    gain_db: gain,
                    q,
                }),
            ]
        })
        .collect()
}

fn quantize(value: f32, bits: u8) -> f32 {
    let levels = ((1_u32 << bits) - 1) as f32;
    (((value.clamp(-1.0, 1.0) + 1.0) * 0.5 * levels).round() / levels) * 2.0 - 1.0
}

impl StereoBiquad {
    fn new(band: &EqBand) -> Self {
        let coefficients = biquad_coefficients(band);
        Self {
            left: Biquad::new(coefficients),
            right: Biquad::new(coefficients),
        }
    }

    fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        (self.left.process(left), self.right.process(right))
    }
}

impl Biquad {
    fn new(coefficients: BiquadCoefficients) -> Self {
        Self {
            coefficients,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        let output = self.coefficients.b0 * input + self.z1;
        self.z1 = self.coefficients.b1 * input - self.coefficients.a1 * output + self.z2;
        self.z2 = self.coefficients.b2 * input - self.coefficients.a2 * output;
        output
    }
}

impl StereoCompressor {
    fn new(settings: &CompressorSettings) -> Self {
        Self {
            threshold_db: settings.threshold_db,
            ratio: settings.ratio,
            attack_coefficient: smoothing_coefficient(settings.attack_ms),
            release_coefficient: smoothing_coefficient(settings.release_ms),
            makeup_gain: decibels_to_gain(settings.makeup_gain_db),
            envelope: 0.0,
            gain: 1.0,
        }
    }

    fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let level = left.abs().max(right.abs());
        let envelope_coefficient = if level > self.envelope {
            self.attack_coefficient
        } else {
            self.release_coefficient
        };
        self.envelope = envelope_coefficient * self.envelope + (1.0 - envelope_coefficient) * level;

        let level_db = 20.0 * self.envelope.max(1.0e-6).log10();
        let over_threshold_db = (level_db - self.threshold_db).max(0.0);
        let reduction_db = over_threshold_db - over_threshold_db / self.ratio;
        let target_gain = decibels_to_gain(-reduction_db);
        let gain_coefficient = if target_gain < self.gain {
            self.attack_coefficient
        } else {
            self.release_coefficient
        };
        self.gain = gain_coefficient * self.gain + (1.0 - gain_coefficient) * target_gain;
        let gain = self.gain * self.makeup_gain;
        (left * gain, right * gain)
    }
}

fn biquad_coefficients(band: &EqBand) -> BiquadCoefficients {
    let a = decibels_to_gain(band.gain_db * 0.5);
    let omega = 2.0 * std::f32::consts::PI * band.frequency_hz / SAMPLE_RATE as f32;
    let sine = omega.sin();
    let cosine = omega.cos();
    let alpha = match band.kind {
        EqFilterKind::Peaking => sine / (2.0 * band.q),
        EqFilterKind::LowShelf | EqFilterKind::HighShelf => {
            let slope = band.q;
            sine / 2.0 * (((a + 1.0 / a) * (1.0 / slope - 1.0) + 2.0).sqrt())
        }
    };
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
    let (b0, b1, b2, a0, a1, a2) = match band.kind {
        EqFilterKind::Peaking => (
            1.0 + alpha * a,
            -2.0 * cosine,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cosine,
            1.0 - alpha / a,
        ),
        EqFilterKind::LowShelf => (
            a * ((a + 1.0) - (a - 1.0) * cosine + two_sqrt_a_alpha),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cosine),
            a * ((a + 1.0) - (a - 1.0) * cosine - two_sqrt_a_alpha),
            (a + 1.0) + (a - 1.0) * cosine + two_sqrt_a_alpha,
            -2.0 * ((a - 1.0) + (a + 1.0) * cosine),
            (a + 1.0) + (a - 1.0) * cosine - two_sqrt_a_alpha,
        ),
        EqFilterKind::HighShelf => (
            a * ((a + 1.0) + (a - 1.0) * cosine + two_sqrt_a_alpha),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cosine),
            a * ((a + 1.0) + (a - 1.0) * cosine - two_sqrt_a_alpha),
            (a + 1.0) - (a - 1.0) * cosine + two_sqrt_a_alpha,
            2.0 * ((a - 1.0) - (a + 1.0) * cosine),
            (a + 1.0) - (a - 1.0) * cosine - two_sqrt_a_alpha,
        ),
    };
    BiquadCoefficients {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

fn decibels_to_gain(decibels: f32) -> f32 {
    10.0_f32.powf(decibels / 20.0)
}

fn float_to_sample(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

fn smoothing_coefficient(milliseconds: f32) -> f32 {
    (-1.0 / (SAMPLE_RATE as f32 * milliseconds / 1_000.0)).exp()
}
