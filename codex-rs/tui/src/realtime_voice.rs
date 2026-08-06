//! Native WebRTC media for the live voice session.
//!
//! This module intentionally implements the desktop app's live transport: a WebRTC audio track
//! and an `oai-events` data channel. It does not use the separate composer dictation recorder or
//! the legacy realtime audio append API.

use anyhow::Context;
use anyhow::Result;
use codex_config::config_toml::RealtimeAudioConfig;
use cpal::traits::StreamTrait;
use opus::Application;
use opus::Channels;
use opus::Encoder;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MIME_TYPE_OPUS;
use webrtc::api::media_engine::MediaEngine;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::realtime_voice_audio::build_input_stream;
use crate::realtime_voice_audio::build_output_stream;
use crate::realtime_voice_audio::encode_input_frames;
use crate::realtime_voice_audio::install_remote_audio_handler;
use crate::realtime_voice_audio::select_input_config;
use crate::realtime_voice_audio::select_input_device;
use crate::realtime_voice_audio::select_output_config;
use crate::realtime_voice_audio::select_output_device;

pub(crate) const SAMPLE_RATE: u32 = 48_000;
pub(crate) const FRAME_SAMPLES: usize = 960;
pub(crate) const FRAME_DURATION: Duration = Duration::from_millis(20);
pub(crate) const MAX_OPUS_PACKET_SIZE: usize = 4_000;
pub(crate) const MAX_OUTPUT_SAMPLES: usize = SAMPLE_RATE as usize * 2;
pub(crate) const INPUT_BUFFER_FRAMES: usize = 30 * 1_000 / 20;
pub(crate) const INPUT_PREROLL_FRAMES: usize = 100 / 20;
pub(crate) const INPUT_SIGNAL_THRESHOLD: i16 = 98;

/// A native live voice peer that follows the desktop app's WebRTC media shape.
pub(crate) struct RealtimeVoiceSession {
    peer_connection: Arc<RTCPeerConnection>,
    input_muted: Arc<AtomicBool>,
    input_stream: cpal::Stream,
    output_stream: cpal::Stream,
    input_task: JoinHandle<()>,
}

impl RealtimeVoiceSession {
    /// Creates the local WebRTC peer, starts native audio streams, and returns the local SDP offer.
    pub(crate) async fn start(audio_config: &RealtimeAudioConfig) -> Result<(Self, String)> {
        let host = cpal::default_host();
        let input_device = select_input_device(&host, audio_config.microphone.as_deref())?;
        let output_device = select_output_device(&host, audio_config.speaker.as_deref())?;
        let input_supported = select_input_config(&input_device)?;
        let output_supported = select_output_config(&output_device)?;
        let input_config = input_supported.config();
        let output_config = output_supported.config();
        let input_muted = Arc::new(AtomicBool::new(false));
        let output_queue = Arc::new(Mutex::new(VecDeque::new()));
        let (input_tx, input_rx) = mpsc::channel(8);

        let input_stream = build_input_stream(
            &input_device,
            input_config,
            input_supported.sample_format(),
            input_supported.channels(),
            input_tx,
            Arc::clone(&input_muted),
        )?;
        let output_stream = build_output_stream(
            &output_device,
            output_config,
            output_supported.sample_format(),
            output_supported.channels(),
            Arc::clone(&output_queue),
        )?;

        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .context("registering WebRTC codecs")?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .context("registering WebRTC interceptors")?;
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();
        let peer_connection = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .context("creating WebRTC peer connection")?,
        );
        let input_released = Arc::new(AtomicBool::new(false));
        let input_released_for_handler = Arc::clone(&input_released);
        peer_connection.on_peer_connection_state_change(Box::new(
            move |state: RTCPeerConnectionState| {
                let input_released = Arc::clone(&input_released_for_handler);
                Box::pin(async move {
                    if state == RTCPeerConnectionState::Connected {
                        input_released.store(true, Ordering::Relaxed);
                    }
                })
            },
        ));

        let data_channel = peer_connection
            .create_data_channel("oai-events", None)
            .await
            .context("creating realtime events data channel")?;
        data_channel.on_message(Box::new(|_message: DataChannelMessage| Box::pin(async {})));

        let input_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: SAMPLE_RATE,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                ..Default::default()
            },
            "audio".to_owned(),
            "codex".to_owned(),
        ));
        peer_connection
            .add_track(input_track.clone())
            .await
            .context("adding realtime microphone track")?;

        install_remote_audio_handler(&peer_connection, Arc::clone(&output_queue));

        let mut gather_complete = peer_connection.gathering_complete_promise().await;
        let offer = peer_connection
            .create_offer(None)
            .await
            .context("creating realtime WebRTC offer")?;
        peer_connection
            .set_local_description(offer)
            .await
            .context("setting local realtime WebRTC offer")?;
        let _ = gather_complete.recv().await;
        let local_description = peer_connection
            .local_description()
            .await
            .context("realtime WebRTC offer was not created")?;

        let encoder = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip)
            .context("creating realtime Opus encoder")?;
        let input_task = tokio::spawn(encode_input_frames(
            input_rx,
            input_track,
            encoder,
            input_released,
        ));

        input_stream.play().context("starting microphone capture")?;
        output_stream
            .play()
            .context("starting realtime audio playback")?;

        Ok((
            Self {
                peer_connection,
                input_muted,
                input_stream,
                output_stream,
                input_task,
            },
            local_description.sdp,
        ))
    }

    pub(crate) fn set_input_muted(&self, muted: bool) {
        self.input_muted.store(muted, Ordering::Relaxed);
    }

    /// Applies the server answer delivered through `thread/realtime/sdp`.
    pub(crate) fn apply_remote_sdp(&self, sdp: String) {
        let peer_connection = Arc::clone(&self.peer_connection);
        tokio::spawn(async move {
            let Ok(answer) = RTCSessionDescription::answer(sdp) else {
                tracing::warn!("realtime WebRTC answer could not be parsed");
                return;
            };
            if peer_connection
                .set_remote_description(answer)
                .await
                .is_err()
            {
                tracing::warn!("realtime WebRTC answer could not be applied");
            }
        });
    }

    pub(crate) async fn close(self) {
        self.input_task.abort();
        let _ = self.peer_connection.close().await;
        let _ = (&self.input_stream, &self.output_stream);
    }
}

impl Drop for RealtimeVoiceSession {
    fn drop(&mut self) {
        self.input_task.abort();
    }
}
