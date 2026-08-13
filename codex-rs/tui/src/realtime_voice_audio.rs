//! Native audio capture, Opus packetization, and WebRTC audio playback.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use bytes::Bytes;
use cpal::I24;
use cpal::SampleFormat;
use cpal::StreamConfig;
use cpal::SupportedStreamConfig;
use cpal::SupportedStreamConfigRange;
use cpal::U24;
use cpal::traits::DeviceTrait;
use cpal::traits::HostTrait;
use opus::Channels;
use opus::Decoder;
use opus::Encoder;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::realtime_voice::FRAME_DURATION;
use crate::realtime_voice::FRAME_SAMPLES;
use crate::realtime_voice::INPUT_BUFFER_FRAMES;
use crate::realtime_voice::INPUT_PREROLL_FRAMES;
use crate::realtime_voice::INPUT_SIGNAL_THRESHOLD;
use crate::realtime_voice::MAX_OPUS_PACKET_SIZE;
use crate::realtime_voice::MAX_OUTPUT_SAMPLES;
use crate::realtime_voice::SAMPLE_RATE;

pub(crate) fn select_input_device(
    host: &cpal::Host,
    requested: Option<&str>,
) -> Result<cpal::Device> {
    match requested {
        Some(requested) => select_device(host.input_devices()?, requested, "microphone"),
        None => host
            .default_input_device()
            .context("no default realtime microphone is available"),
    }
}

pub(crate) fn list_input_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let mut devices = host
        .input_devices()
        .context("listing realtime microphones")?
        .map(|device| device.to_string())
        .collect::<Vec<_>>();
    devices.sort();
    devices.dedup();
    Ok(devices)
}

pub(crate) fn list_output_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let mut devices = host
        .output_devices()
        .context("listing realtime speakers")?
        .map(|device| device.to_string())
        .collect::<Vec<_>>();
    devices.sort();
    devices.dedup();
    Ok(devices)
}

pub(crate) fn select_output_device(
    host: &cpal::Host,
    requested: Option<&str>,
) -> Result<cpal::Device> {
    match requested {
        Some(requested) => select_device(host.output_devices()?, requested, "speaker"),
        None => host
            .default_output_device()
            .context("no default realtime speaker is available"),
    }
}

fn select_device<I>(mut devices: I, requested: &str, kind: &str) -> Result<cpal::Device>
where
    I: Iterator<Item = cpal::Device>,
{
    devices
        .find(|device| device.to_string() == requested)
        .with_context(|| format!("configured realtime {kind} `{requested}` was not found"))
}

#[derive(Clone, Copy)]
enum AudioDirection {
    Input,
    Output,
}

#[derive(Clone, Copy)]
enum SampleRatePolicy {
    Exact,
    AllowFallback,
}

const INPUT_SAMPLE_RATES: &[u32] = &[
    SAMPLE_RATE,
    44_100,
    32_000,
    24_000,
    22_050,
    16_000,
    12_000,
    11_025,
    8_000,
];

fn select_audio_config(
    device: &cpal::Device,
    direction: AudioDirection,
) -> Result<SupportedStreamConfig> {
    let supported = match direction {
        AudioDirection::Input => device
            .supported_input_configs()
            .context("listing microphone formats")?
            .collect::<Vec<_>>(),
        AudioDirection::Output => device
            .supported_output_configs()
            .context("listing speaker formats")?
            .collect::<Vec<_>>(),
    };
    let config = match direction {
        AudioDirection::Input => select_supported_audio_config(
            &supported,
            INPUT_SAMPLE_RATES,
            SampleRatePolicy::AllowFallback,
        ),
        AudioDirection::Output => {
            select_supported_audio_config(&supported, &[SAMPLE_RATE], SampleRatePolicy::Exact)
        }
    };
    config.with_context(|| {
        let kind = match direction {
            AudioDirection::Input => "microphone",
            AudioDirection::Output => "speaker",
        };
        format!("realtime {kind} `{device}` does not support a compatible PCM format")
    })
}

fn select_supported_audio_config(
    ranges: &[SupportedStreamConfigRange],
    preferred_sample_rates: &[u32],
    sample_rate_policy: SampleRatePolicy,
) -> Option<SupportedStreamConfig> {
    for &sample_rate in preferred_sample_rates {
        if let Some(config) = ranges
            .iter()
            .copied()
            .filter(|range| supported_sample_format(range.sample_format()))
            .find_map(|range| range.try_with_sample_rate(sample_rate))
        {
            return Some(config);
        }
    }

    if matches!(sample_rate_policy, SampleRatePolicy::Exact) {
        return None;
    }

    ranges
        .iter()
        .copied()
        .filter(|range| supported_sample_format(range.sample_format()))
        .map(|range| {
            let sample_rate = SAMPLE_RATE.clamp(range.min_sample_rate(), range.max_sample_rate());
            range.with_sample_rate(sample_rate)
        })
        .min_by_key(|config| config.sample_rate().abs_diff(SAMPLE_RATE))
}

fn supported_sample_format(format: SampleFormat) -> bool {
    matches!(
        format,
        SampleFormat::F32
            | SampleFormat::F64
            | SampleFormat::I8
            | SampleFormat::I16
            | SampleFormat::I24
            | SampleFormat::I32
            | SampleFormat::I64
            | SampleFormat::U8
            | SampleFormat::U16
            | SampleFormat::U24
            | SampleFormat::U32
            | SampleFormat::U64
    )
}

pub(crate) fn select_input_config(device: &cpal::Device) -> Result<SupportedStreamConfig> {
    select_audio_config(device, AudioDirection::Input)
}

pub(crate) fn select_output_config(device: &cpal::Device) -> Result<SupportedStreamConfig> {
    select_audio_config(device, AudioDirection::Output)
}

pub(crate) fn build_input_stream(
    device: &cpal::Device,
    config: StreamConfig,
    format: SampleFormat,
    channels: u16,
    input_tx: tokio::sync::mpsc::Sender<InputFrame>,
    input_muted: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    let accumulator =
        InputFrameAccumulator::new(channels, config.sample_rate, input_tx, input_muted);
    match format {
        SampleFormat::F32 => build_input_stream_for(device, config, accumulator, f32_to_i16),
        SampleFormat::F64 => build_input_stream_for(device, config, accumulator, f64_to_i16),
        SampleFormat::I8 => build_input_stream_for(device, config, accumulator, i8_to_i16),
        SampleFormat::I16 => build_input_stream_for(device, config, accumulator, |sample| sample),
        SampleFormat::I24 => build_input_stream_for(device, config, accumulator, i24_to_i16),
        SampleFormat::I32 => build_input_stream_for(device, config, accumulator, i32_to_i16),
        SampleFormat::I64 => build_input_stream_for(device, config, accumulator, i64_to_i16),
        SampleFormat::U8 => build_input_stream_for(device, config, accumulator, u8_to_i16),
        SampleFormat::U16 => build_input_stream_for(device, config, accumulator, u16_to_i16),
        SampleFormat::U24 => build_input_stream_for(device, config, accumulator, u24_to_i16),
        SampleFormat::U32 => build_input_stream_for(device, config, accumulator, u32_to_i16),
        SampleFormat::U64 => build_input_stream_for(device, config, accumulator, u64_to_i16),
        _ => bail!("unsupported realtime microphone sample format `{format}`"),
    }
}

fn build_input_stream_for<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut accumulator: InputFrameAccumulator,
    converter: impl Fn(T) -> i16 + Send + 'static,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _| accumulator.push(data, &converter),
            |error| tracing::warn!(error = %error, "realtime microphone stream failed"),
            None,
        )
        .context("creating realtime microphone stream")
}

pub(crate) fn build_output_stream(
    device: &cpal::Device,
    config: StreamConfig,
    format: SampleFormat,
    channels: u16,
    output_queue: Arc<Mutex<VecDeque<i16>>>,
    acknowledgement_queue: Arc<Mutex<VecDeque<i16>>>,
) -> Result<cpal::Stream> {
    match format {
        SampleFormat::F32 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| *sample = value as f32 / 32_768.0,
        ),
        SampleFormat::F64 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| *sample = value as f64 / 32_768.0,
        ),
        SampleFormat::I8 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| *sample = (value / 256) as i8,
        ),
        SampleFormat::I16 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| *sample = value,
        ),
        SampleFormat::I24 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| *sample = i16_to_i24(value),
        ),
        SampleFormat::I32 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| *sample = i32::from(value) << 16,
        ),
        SampleFormat::I64 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| *sample = i64::from(value) << 48,
        ),
        SampleFormat::U8 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| *sample = (i32::from(value) / 256 + 128) as u8,
        ),
        SampleFormat::U16 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| *sample = (i32::from(value) + 32_768) as u16,
        ),
        SampleFormat::U24 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| *sample = i16_to_u24(value),
        ),
        SampleFormat::U32 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| *sample = ((i64::from(value) << 16) + 2_147_483_648) as u32,
        ),
        SampleFormat::U64 => build_output_stream_for(
            device,
            config,
            channels,
            output_queue,
            acknowledgement_queue,
            |sample, value| {
                *sample = ((i128::from(value) << 48) + 9_223_372_036_854_775_808) as u64
            },
        ),
        _ => bail!("unsupported realtime speaker sample format `{format}`"),
    }
}

fn build_output_stream_for<T>(
    device: &cpal::Device,
    config: StreamConfig,
    channels: u16,
    output_queue: Arc<Mutex<VecDeque<i16>>>,
    acknowledgement_queue: Arc<Mutex<VecDeque<i16>>>,
    converter: impl Fn(&mut T, i16) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
{
    let channels = channels as usize;
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let Ok(mut queue) = output_queue.lock() else {
                    return;
                };
                let Ok(mut acknowledgement_queue) = acknowledgement_queue.lock() else {
                    return;
                };
                for frame in data.chunks_mut(channels) {
                    let (left, right) = pop_output_sample(&mut queue, &mut acknowledgement_queue);
                    let mono = ((i32::from(left) + i32::from(right)) / 2) as i16;
                    for (channel, sample) in frame.iter_mut().enumerate() {
                        converter(
                            sample,
                            if channels == 1 {
                                mono
                            } else if channel % 2 == 0 {
                                left
                            } else {
                                right
                            },
                        );
                    }
                }
            },
            |error| tracing::warn!(error = %error, "realtime speaker stream failed"),
            None,
        )
        .context("creating realtime speaker stream")
}

fn pop_output_sample(
    output_queue: &mut VecDeque<i16>,
    acknowledgement_queue: &mut VecDeque<i16>,
) -> (i16, i16) {
    if !acknowledgement_queue.is_empty() {
        pop_stereo_sample(acknowledgement_queue)
    } else {
        pop_stereo_sample(output_queue)
    }
}

type InputFrame = [i16; FRAME_SAMPLES];

const MAX_RESAMPLER_BUFFER_SAMPLES: usize = 4_096;
const MAX_LOW_PASS_TAPS: usize = 128;

struct InputLowPass {
    window: VecDeque<i16>,
    coefficients: Vec<f64>,
}

impl InputLowPass {
    fn new(source_rate: u32, target_rate: u32, rate_ratio: usize) -> Self {
        let taps = rate_ratio.saturating_mul(32).clamp(16, MAX_LOW_PASS_TAPS);
        let cutoff = 0.4 * f64::from(target_rate) / f64::from(source_rate);
        let center = (taps - 1) as f64 / 2.0;
        let denominator = (taps - 1) as f64;
        let mut coefficients = (0..taps)
            .map(|index| {
                let distance = index as f64 - center;
                let sinc = if distance == 0.0 {
                    2.0 * cutoff
                } else {
                    (2.0 * std::f64::consts::PI * cutoff * distance).sin()
                        / (std::f64::consts::PI * distance)
                };
                let window = 0.42
                    - 0.5 * (2.0 * std::f64::consts::PI * index as f64 / denominator).cos()
                    + 0.08 * (4.0 * std::f64::consts::PI * index as f64 / denominator).cos();
                sinc * window
            })
            .collect::<Vec<_>>();
        let coefficient_sum = coefficients.iter().sum::<f64>();
        for coefficient in &mut coefficients {
            *coefficient /= coefficient_sum;
        }
        Self {
            window: VecDeque::with_capacity(taps),
            coefficients,
        }
    }

    fn push(&mut self, sample: i16) -> i16 {
        if self.window.is_empty() {
            self.window.resize(self.coefficients.len(), sample);
        } else {
            if self.window.len() == self.coefficients.len() {
                let _ = self.window.pop_front();
            }
            self.window.push_back(sample);
        }
        self.window
            .iter()
            .zip(&self.coefficients)
            .map(|(sample, coefficient)| f64::from(*sample) * coefficient)
            .sum::<f64>()
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
    }
}

struct InputResampler {
    source_rate: u32,
    target_rate: u32,
    input: VecDeque<i16>,
    output: VecDeque<i16>,
    low_pass: Option<InputLowPass>,
    position: f64,
}

impl InputResampler {
    fn new(source_rate: u32, target_rate: u32) -> Self {
        let source_rate = source_rate.max(1);
        let target_rate = target_rate.max(1);
        let rate_ratio = source_rate
            .max(target_rate)
            .div_ceil(source_rate.min(target_rate));
        let rate_ratio = usize::try_from(rate_ratio)
            .unwrap_or(MAX_RESAMPLER_BUFFER_SAMPLES)
            .min(MAX_RESAMPLER_BUFFER_SAMPLES);
        let buffer_capacity = rate_ratio
            .saturating_add(4)
            .clamp(8, MAX_RESAMPLER_BUFFER_SAMPLES);
        Self {
            source_rate,
            target_rate,
            input: VecDeque::with_capacity(buffer_capacity),
            output: VecDeque::with_capacity(buffer_capacity),
            low_pass: if source_rate > target_rate {
                Some(InputLowPass::new(source_rate, target_rate, rate_ratio))
            } else {
                None
            },
            position: 0.0,
        }
    }

    fn push(&mut self, sample: i16) {
        let sample = self
            .low_pass
            .as_mut()
            .map_or(sample, |low_pass| low_pass.push(sample));
        if self.source_rate == self.target_rate {
            self.output.push_back(sample);
            return;
        }

        self.input.push_back(sample);
        let step = f64::from(self.source_rate) / f64::from(self.target_rate);
        while self.position + 1.0 < self.input.len() as f64 {
            let index = self.position.floor() as usize;
            let fraction = self.position - index as f64;
            let left = f64::from(self.input[index]);
            let right = f64::from(self.input[index + 1]);
            let sample = left + (right - left) * fraction;
            self.output.push_back(
                sample
                    .round()
                    .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16,
            );
            self.position += step;
        }

        let consumed = (self.position.floor() as usize).min(self.input.len());
        for _ in 0..consumed {
            let _ = self.input.pop_front();
        }
        self.position -= consumed as f64;
    }

    fn pop_output(&mut self) -> Option<i16> {
        self.output.pop_front()
    }
}

struct InputFrameAccumulator {
    channels: usize,
    pending: InputFrame,
    pending_len: usize,
    resampler: InputResampler,
    input_tx: tokio::sync::mpsc::Sender<InputFrame>,
    input_muted: Arc<AtomicBool>,
}

impl InputFrameAccumulator {
    fn new(
        channels: u16,
        sample_rate: u32,
        input_tx: tokio::sync::mpsc::Sender<InputFrame>,
        input_muted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            channels: channels as usize,
            pending: [0; FRAME_SAMPLES],
            pending_len: 0,
            resampler: InputResampler::new(sample_rate, SAMPLE_RATE),
            input_tx,
            input_muted,
        }
    }

    fn push<T>(&mut self, data: &[T], converter: &impl Fn(T) -> i16)
    where
        T: Copy,
    {
        for frame in data.chunks_exact(self.channels) {
            let sample = if self.input_muted.load(Ordering::Relaxed) {
                0
            } else {
                let sum = frame
                    .iter()
                    .copied()
                    .map(converter)
                    .map(i32::from)
                    .sum::<i32>();
                (sum / frame.len() as i32) as i16
            };
            self.resampler.push(sample);
            while let Some(sample) = self.resampler.pop_output() {
                self.pending[self.pending_len] = sample;
                self.pending_len += 1;
                if self.pending_len == FRAME_SAMPLES {
                    let frame = std::mem::replace(&mut self.pending, [0; FRAME_SAMPLES]);
                    self.pending_len = 0;
                    let _ = self.input_tx.try_send(frame);
                }
            }
        }
    }
}

pub(crate) fn install_remote_audio_handler(
    peer_connection: &Arc<RTCPeerConnection>,
    output_queue: Arc<Mutex<VecDeque<i16>>>,
    output_muted: Arc<AtomicBool>,
) {
    peer_connection.on_track(Box::new(move |track, _receiver, _transceiver| {
        let output_queue = Arc::clone(&output_queue);
        let output_muted = Arc::clone(&output_muted);
        Box::pin(async move {
            let Ok(mut decoder) = Decoder::new(SAMPLE_RATE, Channels::Stereo) else {
                return;
            };
            loop {
                let Ok((packet, _attributes)) = track.read_rtp().await else {
                    return;
                };
                let mut decoded = vec![0i16; FRAME_SAMPLES * 2 * 6];
                let Ok(samples_per_channel) = decoder.decode(&packet.payload, &mut decoded, false)
                else {
                    continue;
                };
                let decoded = &decoded[..samples_per_channel * 2];
                append_remote_audio(&output_queue, &output_muted, decoded);
            }
        })
    }));
}

fn append_remote_audio(
    output_queue: &Arc<Mutex<VecDeque<i16>>>,
    output_muted: &Arc<AtomicBool>,
    decoded: &[i16],
) {
    if output_muted.load(Ordering::Relaxed) {
        return;
    }
    let Ok(mut queue) = output_queue.lock() else {
        return;
    };
    if output_muted.load(Ordering::Relaxed) {
        return;
    }
    let excess = queue
        .len()
        .saturating_add(decoded.len())
        .saturating_sub(MAX_OUTPUT_SAMPLES);
    if excess > 0 {
        queue.drain(..excess);
    }
    queue.extend(decoded);
}

pub(crate) async fn encode_input_frames(
    mut input_rx: tokio::sync::mpsc::Receiver<InputFrame>,
    input_track: Arc<TrackLocalStaticSample>,
    mut encoder: Encoder,
    input_released: Arc<AtomicBool>,
) {
    let mut buffered_frames = VecDeque::with_capacity(INPUT_BUFFER_FRAMES);
    let mut released = false;
    while let Some(frame) = input_rx.recv().await {
        if !released && !input_released.load(Ordering::Relaxed) {
            if buffered_frames.len() == INPUT_BUFFER_FRAMES {
                buffered_frames.pop_front();
            }
            buffered_frames.push_back(frame);
            continue;
        }

        if !released {
            released = true;
            release_input_buffer(&mut buffered_frames);
            while let Some(buffered_frame) = buffered_frames.pop_front() {
                if !encode_and_write_frame(&mut encoder, &input_track, buffered_frame).await {
                    return;
                }
            }
        }

        if !encode_and_write_frame(&mut encoder, &input_track, frame).await {
            return;
        }
    }
}

fn release_input_buffer(buffered_frames: &mut VecDeque<InputFrame>) {
    let Some(first_signal_frame) = buffered_frames.iter().position(|frame| {
        frame
            .iter()
            .any(|sample| i32::from(*sample).abs() >= i32::from(INPUT_SIGNAL_THRESHOLD))
    }) else {
        buffered_frames.clear();
        return;
    };
    let start = first_signal_frame.saturating_sub(INPUT_PREROLL_FRAMES);
    buffered_frames.drain(..start);
}

async fn encode_and_write_frame(
    encoder: &mut Encoder,
    input_track: &Arc<TrackLocalStaticSample>,
    frame: InputFrame,
) -> bool {
    let Ok(encoded) = encoder.encode_vec(&frame, MAX_OPUS_PACKET_SIZE) else {
        return true;
    };
    let sample = Sample {
        data: Bytes::from(encoded),
        duration: FRAME_DURATION,
        ..Default::default()
    };
    input_track.write_sample(&sample).await.is_ok()
}

fn pop_stereo_sample(queue: &mut VecDeque<i16>) -> (i16, i16) {
    let left = queue.pop_front().unwrap_or_default();
    let right = queue.pop_front().unwrap_or(left);
    (left, right)
}

fn f32_to_i16(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

fn f64_to_i16(value: f64) -> i16 {
    (value.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

fn i16_to_i24(value: i16) -> I24 {
    I24::from(i32::from(value) << 8)
}

fn i24_to_i16(value: I24) -> i16 {
    (value.inner() >> 8) as i16
}

fn i8_to_i16(value: i8) -> i16 {
    i16::from(value) << 8
}

fn i32_to_i16(value: i32) -> i16 {
    (value >> 16) as i16
}

fn i64_to_i16(value: i64) -> i16 {
    (value >> 48) as i16
}

fn u8_to_i16(value: u8) -> i16 {
    (i16::from(value) - 128) << 8
}

fn u16_to_i16(value: u16) -> i16 {
    (i32::from(value) - 32_768) as i16
}

fn i16_to_u24(value: i16) -> U24 {
    U24::from((i32::from(value) << 8) + 8_388_608)
}

fn u24_to_i16(value: U24) -> i16 {
    ((value.inner() - 8_388_608) >> 8) as i16
}

fn u32_to_i16(value: u32) -> i16 {
    ((i64::from(value) - 2_147_483_648) >> 16) as i16
}

fn u64_to_i16(value: u64) -> i16 {
    ((i128::from(value) - 9_223_372_036_854_775_808) >> 48) as i16
}

#[cfg(test)]
#[path = "realtime_voice_audio_tests.rs"]
mod tests;
