//! Local acknowledgement sounds for the native live voice session.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::realtime_voice::SAMPLE_RATE;

const MAX_ACKNOWLEDGEMENT_SOUND_DURATION_MS: usize = 1_000;
const BUILT_IN_DING_DURATION_MS: usize = 140;
const BUILT_IN_DING_SPLIT_MS: usize = 70;
const BUILT_IN_DING_FIRST_FREQUENCY: f64 = 880.0;
const BUILT_IN_DING_SECOND_FREQUENCY: f64 = 1_320.0;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RealtimeAcknowledgementSound {
    Disabled,
    BuiltIn,
    File(PathBuf),
}

pub(crate) fn load_acknowledgement_sound(
    sound: &RealtimeAcknowledgementSound,
) -> Result<Option<Vec<i16>>> {
    match sound {
        RealtimeAcknowledgementSound::Disabled => Ok(None),
        RealtimeAcknowledgementSound::BuiltIn => Ok(Some(built_in_ding())),
        RealtimeAcknowledgementSound::File(path) => decode_wav_file(path).map(Some),
    }
}

fn built_in_ding() -> Vec<i16> {
    let frame_count = SAMPLE_RATE as usize * BUILT_IN_DING_DURATION_MS / 1_000;
    let split_frame = SAMPLE_RATE as usize * BUILT_IN_DING_SPLIT_MS / 1_000;
    (0..frame_count)
        .flat_map(|frame| {
            let frequency = if frame < split_frame {
                BUILT_IN_DING_FIRST_FREQUENCY
            } else {
                BUILT_IN_DING_SECOND_FREQUENCY
            };
            let phase = 2.0 * std::f64::consts::PI * frequency * frame as f64 / SAMPLE_RATE as f64;
            let fade_in = (frame as f64 / (SAMPLE_RATE as f64 * 0.004)).min(1.0);
            let remaining = frame_count.saturating_sub(frame + 1);
            let fade_out = (remaining as f64 / (SAMPLE_RATE as f64 * 0.018)).min(1.0);
            let sample = (phase.sin() * 0.18 * fade_in * fade_out * 32_767.0).round() as i16;
            [sample, sample]
        })
        .collect()
}

fn decode_wav_file(path: &Path) -> Result<Vec<i16>> {
    let bytes = fs::read(path)
        .with_context(|| format!("reading acknowledgement sound `{}`", path.display()))?;
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        bail!(
            "acknowledgement sound `{}` is not a RIFF/WAVE file",
            path.display()
        );
    }

    let mut offset: usize = 12;
    let mut format = None;
    let mut data = None;
    while offset.saturating_add(8) <= bytes.len() {
        let chunk_id = &bytes[offset..offset + 4];
        let chunk_len = read_u32(&bytes, offset + 4, "WAV chunk length")? as usize;
        let chunk_start = offset + 8;
        let chunk_end = chunk_start
            .checked_add(chunk_len)
            .context("WAV chunk length overflowed")?;
        if chunk_end > bytes.len() {
            bail!(
                "acknowledgement sound `{}` contains a truncated WAV chunk",
                path.display()
            );
        }
        match chunk_id {
            b"fmt " => format = Some(parse_wav_format(&bytes[chunk_start..chunk_end], path)?),
            b"data" => data = Some(&bytes[chunk_start..chunk_end]),
            _ => {}
        }
        offset = chunk_end
            .checked_add(chunk_len % 2)
            .context("WAV chunk padding overflowed")?;
    }

    let (audio_format, channels, source_rate, bits_per_sample) =
        format.context("WAV file has no format chunk")?;
    let data = data.context("WAV file has no audio data chunk")?;
    let bytes_per_sample = usize::from(bits_per_sample / 8);
    let frame_bytes = usize::from(channels)
        .checked_mul(bytes_per_sample)
        .context("WAV channel count overflowed")?;
    if frame_bytes == 0 || data.len() < frame_bytes {
        bail!(
            "acknowledgement sound `{}` has no complete audio frames",
            path.display()
        );
    }

    let source_frames = data.len() / frame_bytes;
    let output_frames = ((source_frames as u64 * u64::from(SAMPLE_RATE))
        .saturating_add(u64::from(source_rate).saturating_sub(1))
        / u64::from(source_rate)) as usize;
    let max_frames = SAMPLE_RATE as usize * MAX_ACKNOWLEDGEMENT_SOUND_DURATION_MS / 1_000;
    if output_frames > max_frames {
        bail!(
            "acknowledgement sound `{}` is longer than {MAX_ACKNOWLEDGEMENT_SOUND_DURATION_MS} ms",
            path.display()
        );
    }

    let mut stereo = Vec::with_capacity(source_frames * 2);
    for frame in 0..source_frames {
        let frame_start = frame * frame_bytes;
        let left = decode_wav_sample(
            &data[frame_start..frame_start + bytes_per_sample],
            audio_format,
            bits_per_sample,
        )?;
        let right = if channels == 1 {
            left
        } else {
            decode_wav_sample(
                &data[frame_start + bytes_per_sample..frame_start + 2 * bytes_per_sample],
                audio_format,
                bits_per_sample,
            )?
        };
        stereo.push((left, right));
    }

    Ok(resample_stereo(&stereo, source_rate))
}

fn parse_wav_format(bytes: &[u8], path: &Path) -> Result<(u16, u16, u32, u16)> {
    if bytes.len() < 16 {
        bail!(
            "acknowledgement sound `{}` has a truncated WAV format chunk",
            path.display()
        );
    }
    let audio_format = read_u16(bytes, 0, "WAV audio format")?;
    if !matches!(audio_format, 1 | 3) {
        bail!(
            "acknowledgement sound `{}` uses unsupported WAV audio format {audio_format}",
            path.display()
        );
    }
    let channels = read_u16(bytes, 2, "WAV channel count")?;
    let source_rate = read_u32(bytes, 4, "WAV sample rate")?;
    let bits_per_sample = read_u16(bytes, 14, "WAV bits per sample")?;
    if !matches!(channels, 1 | 2) || source_rate == 0 {
        bail!(
            "acknowledgement sound `{}` has unsupported WAV channel or rate metadata",
            path.display()
        );
    }
    if !matches!(
        (audio_format, bits_per_sample),
        (1, 8 | 16 | 24 | 32) | (3, 32 | 64)
    ) {
        bail!(
            "acknowledgement sound `{}` uses unsupported WAV sample depth {bits_per_sample}",
            path.display()
        );
    }
    Ok((audio_format, channels, source_rate, bits_per_sample))
}

fn decode_wav_sample(bytes: &[u8], audio_format: u16, bits_per_sample: u16) -> Result<f64> {
    let sample = match (audio_format, bits_per_sample) {
        (1, 8) => (f64::from(bytes[0]) - 128.0) / 128.0,
        (1, 16) => f64::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32_768.0,
        (1, 24) => {
            let value = i32::from_le_bytes([
                bytes[0],
                bytes[1],
                bytes[2],
                if bytes[2] & 0x80 == 0 { 0 } else { 0xff },
            ]);
            f64::from(value) / 8_388_608.0
        }
        (1, 32) => {
            f64::from(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                / 2_147_483_648.0
        }
        (3, 32) => f64::from(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        (3, 64) => f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]),
        _ => bail!("unsupported WAV sample format"),
    };
    Ok(sample.clamp(-1.0, 1.0))
}

fn resample_stereo(samples: &[(f64, f64)], source_rate: u32) -> Vec<i16> {
    let output_frames = ((samples.len() as u64 * u64::from(SAMPLE_RATE))
        .saturating_add(u64::from(source_rate).saturating_sub(1))
        / u64::from(source_rate)) as usize;
    let mut output = Vec::with_capacity(output_frames * 2);
    for frame in 0..output_frames {
        let source_position = frame as f64 * f64::from(source_rate) / f64::from(SAMPLE_RATE);
        let first = source_position.floor() as usize;
        let second = (first + 1).min(samples.len() - 1);
        let fraction = source_position.fract();
        let left = samples[first].0 + (samples[second].0 - samples[first].0) * fraction;
        let right = samples[first].1 + (samples[second].1 - samples[first].1) * fraction;
        output.push(to_i16(left));
        output.push(to_i16(right));
    }
    output
}

fn to_i16(sample: f64) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

fn read_u16(bytes: &[u8], offset: usize, label: &str) -> Result<u16> {
    let bytes = bytes
        .get(offset..offset + 2)
        .with_context(|| format!("truncated {label}"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(bytes: &[u8], offset: usize, label: &str) -> Result<u32> {
    let bytes = bytes
        .get(offset..offset + 4)
        .with_context(|| format!("truncated {label}"))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
#[path = "realtime_voice_sound_tests.rs"]
mod tests;
