//! Bounded container decoding helpers for GPT-Live voice calibration.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::FormatReader;
use symphonia::core::formats::TrackType;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use super::CALIBRATION_MIN_CAPTURE_SAMPLES;
use super::MAX_CALIBRATION_FILE_BYTES;
use super::MAX_CALIBRATION_SAMPLES;
use super::MAX_CALIBRATION_SECONDS;
use crate::realtime_voice::SAMPLE_RATE;

const MAX_SOURCE_SAMPLE_RATE: usize = 192_000;
const MAX_DECODED_SAMPLES: usize = MAX_SOURCE_SAMPLE_RATE * MAX_CALIBRATION_SECONDS as usize;

/// Decodes an audio or video container into bounded mono 48 kHz PCM for local analysis.
pub(super) fn decode_reference_file(path: &Path) -> Result<Vec<i16>> {
    decode_reference_file_inner(path, || false)
}

/// Decodes a reference file while allowing the caller to stop packet processing.
pub(super) fn decode_reference_file_cancellable(
    path: &Path,
    cancellation: &AtomicBool,
) -> Result<Vec<i16>> {
    decode_reference_file_inner(path, || cancellation.load(Ordering::Acquire))
}

fn decode_reference_file_inner(path: &Path, is_cancelled: impl Fn() -> bool) -> Result<Vec<i16>> {
    if is_cancelled() {
        bail!("reference audio analysis was cancelled");
    }
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading reference audio metadata from {}", path.display()))?;
    if metadata.len() == 0 {
        bail!("reference audio file {} is empty", path.display());
    }
    if metadata.len() > MAX_CALIBRATION_FILE_BYTES {
        bail!(
            "reference audio file {} is too large ({} bytes; maximum is {} bytes)",
            path.display(),
            metadata.len(),
            MAX_CALIBRATION_FILE_BYTES
        );
    }

    let file = File::open(path)
        .with_context(|| format!("opening reference audio file {}", path.display()))?;
    let media_source = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
        hint.with_extension(extension);
    }
    let probed = symphonia::default::get_probe()
        .probe(
            &hint,
            media_source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .with_context(|| format!("probing reference audio file {}", path.display()))?;
    decode_format(probed, is_cancelled)
}

fn decode_format(
    mut format: Box<dyn FormatReader>,
    is_cancelled: impl Fn() -> bool,
) -> Result<Vec<i16>> {
    let (track_id, codec_params) = {
        let track = format
            .default_track(TrackType::Audio)
            .context("reference file does not contain an audio track")?;
        (
            track.id,
            track
                .codec_params
                .clone()
                .context("reference audio track is missing codec parameters")?,
        )
    };
    let audio_params = codec_params
        .audio()
        .context("reference file's default track is not audio")?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(audio_params, &AudioDecoderOptions::default())
        .context("creating the reference audio decoder")?;
    let mut mono = Vec::new();
    let mut sample_rate = None;
    let mut channels = None;
    let max_source_frames = audio_params
        .sample_rate
        .map(|rate| (rate as usize * MAX_CALIBRATION_SECONDS as usize).min(MAX_DECODED_SAMPLES));

    loop {
        if is_cancelled() {
            bail!("reference audio analysis was cancelled");
        }
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => return Err(anyhow::anyhow!("decoding reference audio: {error}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .context("decoding a reference audio packet")?;
        if decoded.samples_interleaved() > MAX_DECODED_SAMPLES {
            bail!(
                "reference audio packet is too large to decode safely ({} samples)",
                decoded.samples_interleaved()
            );
        }
        let spec = decoded.spec();
        sample_rate.get_or_insert(spec.rate());
        channels.get_or_insert(spec.channels().count());
        append_mono_samples(&decoded, &mut mono, MAX_DECODED_SAMPLES);
        if mono.len() >= max_source_frames.unwrap_or(MAX_DECODED_SAMPLES) {
            break;
        }
    }

    if is_cancelled() {
        bail!("reference audio analysis was cancelled");
    }

    let sample_rate = sample_rate.context("reference audio has no sample rate")?;
    let channels = channels.context("reference audio has no channel layout")?;
    if channels == 0 || mono.is_empty() {
        bail!("reference audio did not contain decodable samples");
    }
    let mono = resample_mono_with_cancellation(&mono, sample_rate, SAMPLE_RATE, &is_cancelled)
        .context("reference audio analysis was cancelled")?;
    let mono = trim_silence_with_cancellation(&mono, &is_cancelled)
        .context("reference audio analysis was cancelled")?;
    if mono.len() < CALIBRATION_MIN_CAPTURE_SAMPLES {
        bail!("reference audio must contain at least 0.5 seconds of audible audio");
    }
    Ok(mono.into_iter().take(MAX_CALIBRATION_SAMPLES).collect())
}

fn append_mono_samples(
    decoded: &GenericAudioBufferRef<'_>,
    destination: &mut Vec<i16>,
    max_samples: usize,
) {
    let channels = decoded.spec().channels().count();
    if channels == 0 {
        return;
    }
    let mut interleaved = vec![0_i16; decoded.samples_interleaved()];
    decoded.copy_to_slice_interleaved::<i16, _>(&mut interleaved);
    let remaining = max_samples.saturating_sub(destination.len());
    destination.extend(
        interleaved
            .chunks_exact(channels)
            .take(remaining)
            .map(|frame| {
                let sum = frame.iter().map(|sample| i32::from(*sample)).sum::<i32>();
                (sum / frame.len() as i32) as i16
            }),
    );
}

pub(super) fn resample_mono(samples: &[i16], source_rate: u32, target_rate: u32) -> Vec<i16> {
    let never_cancelled = || false;
    resample_mono_with_cancellation(samples, source_rate, target_rate, &never_cancelled)
        .unwrap_or_default()
}

fn resample_mono_with_cancellation(
    samples: &[i16],
    source_rate: u32,
    target_rate: u32,
    is_cancelled: &impl Fn() -> bool,
) -> Option<Vec<i16>> {
    if is_cancelled() {
        return None;
    }
    if source_rate == target_rate {
        return Some(samples.to_vec());
    }
    let output_len = (((samples.len() as u64 * u64::from(target_rate))
        / u64::from(source_rate.max(1))) as usize)
        .min(MAX_CALIBRATION_SAMPLES);
    let mut output = Vec::with_capacity(output_len);
    for index in 0..output_len {
        if index % 4_096 == 0 && is_cancelled() {
            return None;
        }
        let source_position = index as f64 * f64::from(source_rate) / f64::from(target_rate);
        let source_index = source_position.floor() as usize;
        let fraction = source_position.fract();
        let left = samples.get(source_index).copied().unwrap_or_default();
        let right = samples.get(source_index + 1).copied().unwrap_or(left);
        output.push(
            (f64::from(left) + (f64::from(right) - f64::from(left)) * fraction)
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16,
        );
    }
    Some(output)
}

pub(super) fn trim_silence(samples: &[i16]) -> Vec<i16> {
    let never_cancelled = || false;
    trim_silence_with_cancellation(samples, &never_cancelled).unwrap_or_default()
}

fn trim_silence_with_cancellation(
    samples: &[i16],
    is_cancelled: &impl Fn() -> bool,
) -> Option<Vec<i16>> {
    if is_cancelled() {
        return None;
    }
    let peak = samples
        .iter()
        .map(|sample| i32::from(*sample).unsigned_abs())
        .max()
        .unwrap_or_default();
    if is_cancelled() {
        return None;
    }
    let threshold = peak.max(1) / 50;
    let mut first = None;
    for (index, sample) in samples.iter().enumerate() {
        if index % 4_096 == 0 && is_cancelled() {
            return None;
        }
        if i32::from(*sample).unsigned_abs() >= threshold {
            first = Some(index);
            break;
        }
    }
    let Some(first) = first else {
        return Some(Vec::new());
    };
    let mut last = first;
    for (index, sample) in samples.iter().enumerate().rev() {
        if index % 4_096 == 0 && is_cancelled() {
            return None;
        }
        if i32::from(*sample).unsigned_abs() >= threshold {
            last = index;
            break;
        }
    }
    let padding = SAMPLE_RATE as usize / 20;
    let start = first.saturating_sub(padding);
    let end = (last + padding + 1).min(samples.len());
    Some(samples[start..end].to_vec())
}
