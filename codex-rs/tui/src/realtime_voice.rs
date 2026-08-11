//! Native WebRTC media for the live voice session.
//!
//! This module intentionally implements the desktop app's live transport: a WebRTC audio track
//! and an `oai-events` data channel. It does not use the separate composer dictation recorder or
//! the legacy realtime audio append API.

use anyhow::Context;
use anyhow::Result;
use codex_config::config_toml::RealtimeAudioConfig;
pub(crate) use codex_protocol::protocol::REALTIME_NO_PREAMBLES_PROMPT;
use codex_protocol::protocol::RealtimeVoice;
use codex_protocol::protocol::RealtimeVoicesList;
use cpal::traits::StreamTrait;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::ModifierKeyCode;
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
use crate::realtime_voice_audio::list_input_devices;
use crate::realtime_voice_audio::list_output_devices;
use crate::realtime_voice_audio::select_input_config;
use crate::realtime_voice_audio::select_input_device;
use crate::realtime_voice_audio::select_output_config;
use crate::realtime_voice_audio::select_output_device;
use crate::realtime_voice_devices::resolve_device_name;
use crate::realtime_voice_sound::RealtimeAcknowledgementSound;
use crate::realtime_voice_sound::load_acknowledgement_sound;

pub(crate) const SAMPLE_RATE: u32 = 48_000;
pub(crate) const FRAME_SAMPLES: usize = 960;
pub(crate) const FRAME_DURATION: Duration = Duration::from_millis(20);
pub(crate) const MAX_OPUS_PACKET_SIZE: usize = 4_000;
pub(crate) const MAX_OUTPUT_SAMPLES: usize = SAMPLE_RATE as usize * 2;
pub(crate) const INPUT_BUFFER_FRAMES: usize = 30 * 1_000 / 20;
pub(crate) const INPUT_PREROLL_FRAMES: usize = 100 / 20;
pub(crate) const INPUT_SIGNAL_THRESHOLD: i16 = 98;
pub(crate) const DEFAULT_REALTIME_HOTKEY: &str = "right-option";
pub(crate) fn realtime_start_prompt(enable_preambles: bool) -> Option<Option<String>> {
    (!enable_preambles).then(|| Some(REALTIME_NO_PREAMBLES_PROMPT.to_string()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RealtimeMicMode {
    Disabled,
    PushToTalk,
    Hot,
    CaptureHotkey,
}

impl RealtimeMicMode {
    pub(crate) fn from_config_enabled(enabled: bool) -> Self {
        if enabled {
            Self::PushToTalk
        } else {
            Self::Disabled
        }
    }

    pub(crate) fn status_label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::PushToTalk => "enabled (push-to-talk)",
            Self::Hot => "enabled (hot mic; always listening)",
            Self::CaptureHotkey => "waiting for a push-to-talk key",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RealtimeMicCommand {
    Toggle,
    On,
    Off,
    Status,
    Hot,
    Push,
    CaptureHotkey,
    ChangeMicrophone,
    ListDevices,
    ChangeSpeaker,
    ListSpeakers,
    SetMicrophone(String),
    SetSpeaker(String),
    ListMicrophoneAliases,
    ListSpeakerAliases,
    SetMicrophoneAlias {
        alias: String,
        device: Option<String>,
    },
    SetSpeakerAlias {
        alias: String,
        device: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RealtimeVoiceDebugCommand {
    Toggle,
    On,
    Off,
    Status,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RealtimeVoiceCommand {
    On,
    Off,
    List,
    Status,
    Debug(RealtimeVoiceDebugCommand),
    Set(RealtimeVoice),
}

pub(crate) fn realtime_voice_from_name(name: &str) -> Option<RealtimeVoice> {
    match name.trim().to_ascii_lowercase().as_str() {
        "alloy" => Some(RealtimeVoice::Alloy),
        "arbor" => Some(RealtimeVoice::Arbor),
        "ash" => Some(RealtimeVoice::Ash),
        "ballad" => Some(RealtimeVoice::Ballad),
        "breeze" => Some(RealtimeVoice::Breeze),
        "cedar" => Some(RealtimeVoice::Cedar),
        "coral" => Some(RealtimeVoice::Coral),
        "cove" => Some(RealtimeVoice::Cove),
        "echo" => Some(RealtimeVoice::Echo),
        "ember" => Some(RealtimeVoice::Ember),
        "juniper" => Some(RealtimeVoice::Juniper),
        "maple" => Some(RealtimeVoice::Maple),
        "marin" => Some(RealtimeVoice::Marin),
        "sage" => Some(RealtimeVoice::Sage),
        "shimmer" => Some(RealtimeVoice::Shimmer),
        "sol" => Some(RealtimeVoice::Sol),
        "spruce" => Some(RealtimeVoice::Spruce),
        "vale" => Some(RealtimeVoice::Vale),
        "verse" => Some(RealtimeVoice::Verse),
        _ => None,
    }
}

pub(crate) fn realtime_v3_voice(configured: Option<RealtimeVoice>) -> RealtimeVoice {
    let voice = configured.unwrap_or(RealtimeVoice::Arbor);
    if RealtimeVoicesList::builtin().v1.contains(&voice) {
        voice
    } else {
        RealtimeVoice::Arbor
    }
}

pub(crate) fn realtime_hotkey_matches(spec: Option<&str>, event: KeyEvent) -> bool {
    let expected = spec.unwrap_or(DEFAULT_REALTIME_HOTKEY).trim();
    realtime_hotkey_spec_from_event(event)
        .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
}

pub(crate) fn realtime_hotkey_spec_from_event(event: KeyEvent) -> Option<String> {
    let key = match event.code {
        KeyCode::Modifier(ModifierKeyCode::RightAlt) => "right-option".to_string(),
        KeyCode::Modifier(ModifierKeyCode::LeftAlt) => "left-option".to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "page-up".to_string(),
        KeyCode::PageDown => "page-down".to_string(),
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(character) if !character.is_control() => {
            character.to_ascii_lowercase().to_string()
        }
        KeyCode::F(number) => format!("f{number}"),
        _ => return None,
    };
    if key == "right-option" || key == "left-option" {
        return Some(key);
    }

    let mut parts = Vec::new();
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl");
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt");
    }
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift");
    }
    parts.push(key.as_str());
    Some(parts.join("-"))
}

/// A native live voice peer that follows the desktop app's WebRTC media shape.
pub(crate) struct RealtimeVoiceSession {
    peer_connection: Arc<RTCPeerConnection>,
    input_muted: Arc<AtomicBool>,
    input_stream: cpal::Stream,
    output_stream: cpal::Stream,
    acknowledgement_queue: Arc<Mutex<VecDeque<i16>>>,
    acknowledgement_samples: Option<Vec<i16>>,
    input_task: JoinHandle<()>,
}

impl RealtimeVoiceSession {
    /// Creates the local WebRTC peer, starts native audio streams, and returns the local SDP offer.
    pub(crate) async fn start(
        audio_config: &RealtimeAudioConfig,
        acknowledgement_sound: &RealtimeAcknowledgementSound,
    ) -> Result<(Self, String)> {
        let host = cpal::default_host();
        let input_device = match audio_config.microphone.as_deref() {
            Some(requested) => {
                let devices = list_input_devices()?;
                let resolved = resolve_device_name(
                    requested,
                    &devices,
                    &audio_config.microphone_aliases,
                )
                .with_context(|| {
                    format!(
                        "configured realtime microphone `{requested}` or its alias was not found"
                    )
                })?;
                select_input_device(&host, Some(&resolved))?
            }
            None => select_input_device(&host, None)?,
        };
        let output_device = match audio_config.speaker.as_deref() {
            Some(requested) => {
                let devices = list_output_devices()?;
                let resolved = resolve_device_name(
                    requested,
                    &devices,
                    &audio_config.speaker_aliases,
                )
                .with_context(|| {
                    format!("configured realtime speaker `{requested}` or its alias was not found")
                })?;
                select_output_device(&host, Some(&resolved))?
            }
            None => select_output_device(&host, None)?,
        };
        let input_supported = select_input_config(&input_device)?;
        let output_supported = select_output_config(&output_device)?;
        let input_config = input_supported.config();
        let output_config = output_supported.config();
        let input_muted = Arc::new(AtomicBool::new(false));
        let output_queue = Arc::new(Mutex::new(VecDeque::new()));
        let acknowledgement_queue = Arc::new(Mutex::new(VecDeque::new()));
        let acknowledgement_samples = load_acknowledgement_sound(acknowledgement_sound)?;
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
            Arc::clone(&acknowledgement_queue),
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
                acknowledgement_queue,
                acknowledgement_samples,
                input_task,
            },
            local_description.sdp,
        ))
    }

    pub(crate) fn set_input_muted(&self, muted: bool) {
        self.input_muted.store(muted, Ordering::Relaxed);
    }

    pub(crate) fn play_acknowledgement_sound(&self) {
        let Some(samples) = &self.acknowledgement_samples else {
            return;
        };
        let Ok(mut acknowledgement_queue) = self.acknowledgement_queue.lock() else {
            return;
        };
        let excess = acknowledgement_queue
            .len()
            .saturating_add(samples.len())
            .saturating_sub(MAX_OUTPUT_SAMPLES);
        if excess > 0 {
            acknowledgement_queue.drain(..excess);
        }
        acknowledgement_queue.extend(samples.iter().copied());
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

#[cfg(test)]
#[path = "realtime_voice_tests.rs"]
mod tests;

impl Drop for RealtimeVoiceSession {
    fn drop(&mut self) {
        self.input_task.abort();
    }
}
