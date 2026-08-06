//! Native audio capture, Opus packetization, and WebRTC audio playback.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use bytes::Bytes;
use cpal::SampleFormat;
use cpal::StreamConfig;
use cpal::SupportedStreamConfig;
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

enum AudioDirection {
    Input,
    Output,
}

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
    supported
        .into_iter()
        .filter_map(|range| range.try_with_sample_rate(SAMPLE_RATE))
        .find(|config| supported_sample_format(config.sample_format()))
        .with_context(|| {
            format!(
                "realtime audio device `{device}` does not support a supported 48 kHz PCM format"
            )
        })
}

fn supported_sample_format(format: SampleFormat) -> bool {
    matches!(
        format,
        SampleFormat::F32
            | SampleFormat::F64
            | SampleFormat::I8
            | SampleFormat::I16
            | SampleFormat::I32
            | SampleFormat::I64
            | SampleFormat::U8
            | SampleFormat::U16
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
    input_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    input_muted: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    match format {
        SampleFormat::F32 => {
            build_input_stream_for(device, config, channels, input_tx, input_muted, f32_to_i16)
        }
        SampleFormat::F64 => {
            build_input_stream_for(device, config, channels, input_tx, input_muted, f64_to_i16)
        }
        SampleFormat::I8 => {
            build_input_stream_for(device, config, channels, input_tx, input_muted, i8_to_i16)
        }
        SampleFormat::I16 => {
            build_input_stream_for(device, config, channels, input_tx, input_muted, |sample| {
                sample
            })
        }
        SampleFormat::I32 => {
            build_input_stream_for(device, config, channels, input_tx, input_muted, i32_to_i16)
        }
        SampleFormat::I64 => {
            build_input_stream_for(device, config, channels, input_tx, input_muted, i64_to_i16)
        }
        SampleFormat::U8 => {
            build_input_stream_for(device, config, channels, input_tx, input_muted, u8_to_i16)
        }
        SampleFormat::U16 => {
            build_input_stream_for(device, config, channels, input_tx, input_muted, u16_to_i16)
        }
        SampleFormat::U32 => {
            build_input_stream_for(device, config, channels, input_tx, input_muted, u32_to_i16)
        }
        SampleFormat::U64 => {
            build_input_stream_for(device, config, channels, input_tx, input_muted, u64_to_i16)
        }
        _ => bail!("unsupported realtime microphone sample format `{format}`"),
    }
}

fn build_input_stream_for<T>(
    device: &cpal::Device,
    config: StreamConfig,
    channels: u16,
    input_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    input_muted: Arc<AtomicBool>,
    converter: impl Fn(T) -> i16 + Send + 'static,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
{
    let mut accumulator = InputFrameAccumulator::new(channels, input_tx, input_muted);
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
) -> Result<cpal::Stream> {
    match format {
        SampleFormat::F32 => {
            build_output_stream_for(device, config, channels, output_queue, |sample, value| {
                *sample = value as f32 / 32_768.0
            })
        }
        SampleFormat::F64 => {
            build_output_stream_for(device, config, channels, output_queue, |sample, value| {
                *sample = value as f64 / 32_768.0
            })
        }
        SampleFormat::I8 => {
            build_output_stream_for(device, config, channels, output_queue, |sample, value| {
                *sample = (value / 256) as i8
            })
        }
        SampleFormat::I16 => {
            build_output_stream_for(device, config, channels, output_queue, |sample, value| {
                *sample = value
            })
        }
        SampleFormat::I32 => {
            build_output_stream_for(device, config, channels, output_queue, |sample, value| {
                *sample = i32::from(value) << 16
            })
        }
        SampleFormat::I64 => {
            build_output_stream_for(device, config, channels, output_queue, |sample, value| {
                *sample = i64::from(value) << 48
            })
        }
        SampleFormat::U8 => {
            build_output_stream_for(device, config, channels, output_queue, |sample, value| {
                *sample = (i32::from(value) / 256 + 128) as u8
            })
        }
        SampleFormat::U16 => {
            build_output_stream_for(device, config, channels, output_queue, |sample, value| {
                *sample = (i32::from(value) + 32_768) as u16
            })
        }
        SampleFormat::U32 => {
            build_output_stream_for(device, config, channels, output_queue, |sample, value| {
                *sample = ((i64::from(value) << 16) + 2_147_483_648) as u32
            })
        }
        SampleFormat::U64 => {
            build_output_stream_for(device, config, channels, output_queue, |sample, value| {
                *sample = ((i128::from(value) << 48) + 9_223_372_036_854_775_808) as u64
            })
        }
        _ => bail!("unsupported realtime speaker sample format `{format}`"),
    }
}

fn build_output_stream_for<T>(
    device: &cpal::Device,
    config: StreamConfig,
    channels: u16,
    output_queue: Arc<Mutex<VecDeque<i16>>>,
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
                for frame in data.chunks_mut(channels) {
                    let (left, right) = pop_stereo_sample(&mut queue);
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

struct InputFrameAccumulator {
    channels: usize,
    pending: Vec<i16>,
    input_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    input_muted: Arc<AtomicBool>,
}

impl InputFrameAccumulator {
    fn new(
        channels: u16,
        input_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
        input_muted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            channels: channels as usize,
            pending: Vec::with_capacity(FRAME_SAMPLES),
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
            self.pending.push(sample);
            if self.pending.len() == FRAME_SAMPLES {
                let frame = std::mem::replace(&mut self.pending, Vec::with_capacity(FRAME_SAMPLES));
                let _ = self.input_tx.try_send(frame);
            }
        }
    }
}

pub(crate) fn install_remote_audio_handler(
    peer_connection: &Arc<RTCPeerConnection>,
    output_queue: Arc<Mutex<VecDeque<i16>>>,
) {
    peer_connection.on_track(Box::new(move |track, _receiver, _transceiver| {
        let output_queue = Arc::clone(&output_queue);
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
                let Ok(mut queue) = output_queue.lock() else {
                    return;
                };
                let excess = queue
                    .len()
                    .saturating_add(decoded.len())
                    .saturating_sub(MAX_OUTPUT_SAMPLES);
                if excess > 0 {
                    queue.drain(..excess);
                }
                queue.extend(decoded);
            }
        })
    }));
}

pub(crate) async fn encode_input_frames(
    mut input_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
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

fn release_input_buffer(buffered_frames: &mut VecDeque<Vec<i16>>) {
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
    frame: Vec<i16>,
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

fn u32_to_i16(value: u32) -> i16 {
    ((i64::from(value) - 2_147_483_648) >> 16) as i16
}

fn u64_to_i16(value: u64) -> i16 {
    ((i128::from(value) - 9_223_372_036_854_775_808) >> 48) as i16
}

#[cfg(test)]
#[path = "realtime_voice_audio_tests.rs"]
mod tests;
