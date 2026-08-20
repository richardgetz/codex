//! Event-driven orchestration for local GPT-Live voice calibration.

use super::*;
use crate::realtime_voice::RealtimeVoiceSession;
use crate::realtime_voice::realtime_start_prompt;
use crate::realtime_voice::realtime_v3_voice;
use crate::realtime_voice_calibration::CALIBRATION_PHRASE;
use crate::realtime_voice_calibration::CALIBRATION_POLL_INTERVAL;
use crate::realtime_voice_calibration::CALIBRATION_PREPARATION_TIMEOUT;
use crate::realtime_voice_calibration::CALIBRATION_RESPONSE_TIMEOUT;
use crate::realtime_voice_calibration::CALIBRATION_SETUP_TIMEOUT;
use crate::realtime_voice_calibration::CALIBRATION_STOP_TIMEOUT;
use crate::realtime_voice_calibration::VoiceCalibrationPreparation;
use crate::realtime_voice_calibration::VoiceCalibrationRun;
use crate::realtime_voice_calibration::analyze_pcm;
use crate::realtime_voice_calibration::analyze_reference_file_cancellable;
use crate::realtime_voice_calibration::estimate_effect_preset;
use crate::realtime_voice_calibration::format_ranked_candidates;
use crate::realtime_voice_calibration::rank_calibration_samples;
use anyhow::Context;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadRealtimeListVoicesParams;
use codex_app_server_protocol::ThreadRealtimeListVoicesResponse;
use codex_app_server_protocol::ThreadRealtimeStartParams;
use codex_app_server_protocol::ThreadRealtimeStartTransport;
use codex_protocol::protocol::RealtimeConversationVersion;
use codex_protocol::protocol::RealtimeOutputModality;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

const CALIBRATION_SESSION_ID_PREFIX: &str = "codex-voice-calibration";

async fn stop_realtime_voice_calibration_server_session(
    app_server: &mut AppServerSession,
    thread_id: ThreadId,
) -> anyhow::Result<()> {
    tokio::time::timeout(
        CALIBRATION_STOP_TIMEOUT,
        app_server.thread_realtime_stop(thread_id),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out stopping the temporary GPT-Live session"))?
    .map_err(|err| anyhow::anyhow!("stopping the temporary GPT-Live session: {err:#}"))
}

async fn prepare_realtime_voice_calibration(
    app_event_tx: AppEventSender,
    request_handle: AppServerRequestHandle,
    request_id: uuid::Uuid,
    thread_id: ThreadId,
    path: PathBuf,
    cancellation: Arc<AtomicBool>,
) {
    let result = async {
        if cancellation.load(Ordering::Acquire) {
            return Err("GPT-Live voice calibration was cancelled".to_string());
        }
        let analysis_path = path.clone();
        let analysis_cancellation = cancellation.clone();
        let reference = tokio::time::timeout(
            CALIBRATION_PREPARATION_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                analyze_reference_file_cancellable(&analysis_path, &analysis_cancellation)
            }),
        )
        .await
        .map_err(|_| {
            cancellation.store(true, Ordering::Release);
            "timed out analyzing the reference audio".to_string()
        })?
        .map_err(|err| format!("reference audio analysis task failed: {err}"))?
        .map_err(|err| format!("failed to analyze the reference audio: {err:#}"))?;
        if cancellation.load(Ordering::Acquire) {
            return Err("GPT-Live voice calibration was cancelled".to_string());
        }
        let response = tokio::time::timeout(
            CALIBRATION_PREPARATION_TIMEOUT,
            request_handle.request_typed::<ThreadRealtimeListVoicesResponse>(
                ClientRequest::ThreadRealtimeListVoices {
                    request_id: RequestId::String(format!(
                        "codex-voice-calibration-list-{request_id}"
                    )),
                    params: ThreadRealtimeListVoicesParams {},
                },
            ),
        )
        .await
        .map_err(|_| "timed out listing GPT-Live V3 voices".to_string())?
        .map_err(|err| format!("failed to list GPT-Live V3 voices: {err}"))?;
        if cancellation.load(Ordering::Acquire) {
            return Err("GPT-Live voice calibration was cancelled".to_string());
        }
        if response.voices.v1.is_empty() {
            return Err("the GPT-Live V3 voice list is empty".to_string());
        }
        Ok(VoiceCalibrationPreparation {
            thread_id,
            reference_path: path,
            reference,
            voices: response.voices.v1,
        })
    }
    .await;
    // Always deliver the terminal preparation result. A timeout sets the cancellation flag so
    // the decoder stops at its next checkpoint, but suppressing this event would leave the TUI's
    // `realtime_voice_calibration_preparing` guard set forever until the process restarts.
    app_event_tx.send(AppEvent::RealtimeVoiceCalibrationPrepared { request_id, result });
}

impl App {
    pub(super) async fn start_realtime_voice_calibration(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        path: PathBuf,
    ) {
        if self.realtime_voice_calibration.is_some()
            || self.realtime_voice_calibration_preparing.is_some()
        {
            self.chat_widget.add_error_message(
                "A GPT-Live voice calibration is already running. Wait for it to finish or restart the TUI."
                    .to_string(),
            );
            return;
        }
        if self.realtime_voice_session.is_some() {
            self.chat_widget.add_error_message(
                "Stop the active GPT-Live session before starting voice calibration.".to_string(),
            );
            return;
        }
        if self.realtime_voice_profile.is_some()
            || self.realtime_voice_rotation_selected
            || self
                .config
                .realtime
                .voice_rotation
                .as_deref()
                .is_some_and(|voices| !voices.is_empty())
            || self
                .config
                .realtime
                .voice_profile_rotation
                .as_deref()
                .is_some_and(|profiles| !profiles.is_empty())
        {
            self.chat_widget.add_error_message(
                "Disable the active GPT-Live profile or voice rotation before starting calibration so the matched base voice can be auditioned directly.".to_string(),
            );
            return;
        }
        let Some(thread_id) = self.active_thread_id.or(self.chat_widget.thread_id()) else {
            self.chat_widget.add_error_message(
                "Voice calibration is unavailable until a thread starts.".to_string(),
            );
            return;
        };
        let path = if path.is_absolute() {
            path
        } else {
            self.config.cwd.to_path_buf().join(path)
        };
        self.chat_widget.add_info_message(
            format!(
                "Analyzing `{}` locally, then comparing the GPT-Live V3 voices with the fixed calibration phrase.",
                path.display()
            ),
            None,
        );
        let request_id = uuid::Uuid::new_v4();
        self.realtime_voice_calibration_preparing = Some(request_id);
        let preparation_cancellation = Arc::new(AtomicBool::new(false));
        let app_event_tx = self.app_event_tx.clone();
        let request_handle = app_server.request_handle();
        let preparation_task = tokio::spawn(prepare_realtime_voice_calibration(
            app_event_tx,
            request_handle,
            request_id,
            thread_id,
            path,
            preparation_cancellation.clone(),
        ));
        self.realtime_voice_calibration_preparation_abort = Some(preparation_task.abort_handle());
        self.realtime_voice_calibration_preparation_cancel = Some(preparation_cancellation);
        tui.frame_requester().schedule_frame();
    }

    pub(super) async fn handle_realtime_voice_calibration_prepared(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        request_id: uuid::Uuid,
        result: Result<VoiceCalibrationPreparation, String>,
    ) {
        if self.realtime_voice_calibration_preparing != Some(request_id) {
            return;
        }
        self.realtime_voice_calibration_preparing = None;
        self.realtime_voice_calibration_preparation_abort = None;
        self.realtime_voice_calibration_preparation_cancel = None;
        let preparation = match result {
            Ok(preparation) => preparation,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("GPT-Live voice calibration stopped: {error}"));
                tui.frame_requester().schedule_frame();
                return;
            }
        };
        let thread_id = preparation.thread_id;
        let voice_count = preparation.voices.len();
        self.realtime_voice_calibration = Some(VoiceCalibrationRun::new(
            preparation.thread_id,
            preparation.reference_path,
            preparation.reference,
            preparation.voices,
        ));
        self.chat_widget.add_info_message(
            format!(
                "Reference analyzed. Sampling {voice_count} GPT-Live V3 voices; calibration audio is muted locally."
            ),
            None,
        );
        if let Err(err) = self
            .start_next_realtime_voice_calibration_candidate(thread_id, app_server)
            .await
        {
            self.abort_realtime_voice_calibration(app_server, format!("{err:#}"))
                .await;
        }
        tui.frame_requester().schedule_frame();
    }

    pub(super) async fn handle_realtime_voice_calibration_poll(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        run_id: uuid::Uuid,
    ) {
        if self
            .realtime_voice_calibration
            .as_ref()
            .is_none_or(|run| run.run_id != run_id)
        {
            return;
        }
        let Some((
            thread_id,
            waiting_for_close,
            close_wait_expired,
            finish_pending,
            next_candidate_pending,
        )) = self.realtime_voice_calibration.as_ref().map(|run| {
            (
                run.thread_id,
                run.waiting_for_close,
                run.close_wait_expired(),
                run.finish_pending,
                run.next_candidate_pending,
            )
        })
        else {
            return;
        };
        if waiting_for_close {
            if close_wait_expired {
                self.abort_realtime_voice_calibration(
                    app_server,
                    "timed out waiting for the previous GPT-Live calibration session to close"
                        .to_string(),
                )
                .await;
            } else {
                self.schedule_realtime_voice_calibration_poll();
            }
            tui.frame_requester().schedule_frame();
            return;
        }
        if finish_pending {
            let Some(run) = self.realtime_voice_calibration.take() else {
                tui.frame_requester().schedule_frame();
                return;
            };
            self.finish_realtime_voice_calibration(app_server, run)
                .await;
            tui.frame_requester().schedule_frame();
            return;
        }
        if next_candidate_pending {
            if let Some(run) = self.realtime_voice_calibration.as_mut() {
                run.next_candidate_pending = false;
            }
            if let Err(err) = self
                .start_next_realtime_voice_calibration_candidate(thread_id, app_server)
                .await
            {
                self.abort_realtime_voice_calibration(app_server, format!("{err:#}"))
                    .await;
            }
            tui.frame_requester().schedule_frame();
            return;
        }
        if let Some(error) = self
            .realtime_voice_calibration
            .as_mut()
            .and_then(|run| run.error.take())
        {
            self.abort_realtime_voice_calibration(app_server, error)
                .await;
            tui.frame_requester().schedule_frame();
            return;
        }
        let Some(session) = self.realtime_voice_session.as_ref() else {
            self.abort_realtime_voice_calibration(
                app_server,
                "the temporary GPT-Live session disappeared before capture completed".to_string(),
            )
            .await;
            tui.frame_requester().schedule_frame();
            return;
        };
        let session_connected = session.is_connected();
        let captured_frame_count = session.captured_frame_count();
        let Some(candidate_started_at) = self
            .realtime_voice_calibration
            .as_ref()
            .map(|run| run.candidate_started_at)
        else {
            return;
        };
        if !session_connected && candidate_started_at.elapsed() < CALIBRATION_RESPONSE_TIMEOUT {
            self.schedule_realtime_voice_calibration_poll();
            return;
        }
        let speech_pending = self
            .realtime_voice_calibration
            .as_ref()
            .is_some_and(VoiceCalibrationRun::speech_pending);
        if session_connected && speech_pending {
            let result = tokio::time::timeout(
                CALIBRATION_SETUP_TIMEOUT,
                app_server.thread_realtime_append_speech(thread_id, CALIBRATION_PHRASE.to_string()),
            )
            .await
            .map_err(|_| "timed out sending the fixed GPT-Live calibration phrase".to_string())
            .and_then(|result| {
                result
                    .map_err(|err| format!("sending the fixed GPT-Live calibration phrase: {err}"))
            });
            if let Some(run) = self.realtime_voice_calibration.as_mut() {
                match result {
                    Ok(()) => run.mark_speech_sent(),
                    Err(err) => run.error = Some(err),
                }
            }
            self.schedule_realtime_voice_calibration_poll();
            tui.frame_requester().schedule_frame();
            return;
        }
        let finished = self
            .realtime_voice_calibration
            .as_mut()
            .is_some_and(|run| run.observe_capture(captured_frame_count));
        if !finished {
            self.schedule_realtime_voice_calibration_poll();
            return;
        }
        let Some(session) = self.realtime_voice_session.take() else {
            self.abort_realtime_voice_calibration(
                app_server,
                "the temporary GPT-Live session disappeared while finalizing capture".to_string(),
            )
            .await;
            tui.frame_requester().schedule_frame();
            return;
        };
        let Some((voice, finish_pending)) = self.realtime_voice_calibration.as_ref().map(|run| {
            (
                run.current_voice(),
                run.candidate_index + 1 >= run.voices.len(),
            )
        }) else {
            self.abort_realtime_voice_calibration(
                app_server,
                "calibration state disappeared while finalizing capture".to_string(),
            )
            .await;
            tui.frame_requester().schedule_frame();
            return;
        };
        if let Some(run) = self.realtime_voice_calibration.as_mut() {
            run.begin_wait_for_close(finish_pending);
        }
        let captured = session.take_captured_audio();
        let stop_result =
            stop_realtime_voice_calibration_server_session(app_server, thread_id).await;
        session.close().await;
        if let Err(err) = stop_result {
            let retry_result =
                stop_realtime_voice_calibration_server_session(app_server, thread_id).await;
            self.abort_realtime_voice_calibration(
                app_server,
                match retry_result {
                    Ok(()) => format!(
                        "could not stop the temporary GPT-Live session on the first attempt: {err:#}"
                    ),
                    Err(retry_err) => format!(
                        "could not stop the temporary GPT-Live session: {err:#}; retry failed: {retry_err:#}"
                    ),
                },
            )
            .await;
            tui.frame_requester().schedule_frame();
            return;
        }
        let Some(voice) = voice else {
            self.abort_realtime_voice_calibration(
                app_server,
                "the current calibration voice was lost".to_string(),
            )
            .await;
            tui.frame_requester().schedule_frame();
            return;
        };
        let capture_timed_out = self
            .realtime_voice_calibration
            .as_ref()
            .is_some_and(|run| run.capture_timed_out);
        let features = if capture_timed_out {
            self.chat_widget.add_info_message(
                format!(
                    "Skipping the `{}` calibration sample because the response timed out before its audio settled.",
                    voice.wire_name()
                ),
                None,
            );
            None
        } else {
            match analyze_pcm(&captured, 48_000, 2) {
                Ok(features) => Some(features),
                Err(err) => {
                    self.chat_widget.add_info_message(
                        format!(
                            "Skipping the `{}` calibration sample because it could not be analyzed: {err:#}",
                            voice.wire_name()
                        ),
                        None,
                    );
                    None
                }
            }
        };
        let Some(run) = self.realtime_voice_calibration.as_mut() else {
            return;
        };
        match features {
            Some(features) => run.record_candidate(features),
            None => run.skip_candidate(),
        }
        self.advance_realtime_voice_calibration_candidate(tui, app_server, thread_id)
            .await;
    }

    async fn advance_realtime_voice_calibration_candidate(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) {
        let complete = self
            .realtime_voice_calibration
            .as_ref()
            .is_some_and(VoiceCalibrationRun::is_complete);
        if self
            .realtime_voice_calibration
            .as_ref()
            .is_some_and(|run| run.waiting_for_close)
        {
            self.schedule_realtime_voice_calibration_poll();
            tui.frame_requester().schedule_frame();
            return;
        }
        if !complete {
            if let Err(err) = self
                .start_next_realtime_voice_calibration_candidate(thread_id, app_server)
                .await
            {
                self.abort_realtime_voice_calibration(app_server, format!("{err:#}"))
                    .await;
            }
            tui.frame_requester().schedule_frame();
            return;
        }
        let Some(run) = self.realtime_voice_calibration.take() else {
            tui.frame_requester().schedule_frame();
            return;
        };
        self.realtime_voice_requested_session_id = None;
        self.realtime_voice_ignore_legacy_notifications = true;
        self.finish_realtime_voice_calibration(app_server, run)
            .await;
        tui.frame_requester().schedule_frame();
    }

    async fn finish_realtime_voice_calibration(
        &mut self,
        app_server: &mut AppServerSession,
        run: VoiceCalibrationRun,
    ) {
        let result = rank_calibration_samples(run.reference, run.samples);
        let Some((best, score)) = result.best() else {
            self.chat_widget.add_error_message(
                "GPT-Live calibration produced no usable voice samples.".to_string(),
            );
            return;
        };
        let calibration_suffix = uuid::Uuid::new_v4().simple().to_string();
        let preset_name = format!("calibrated-{}-{calibration_suffix}", best.voice.wire_name());
        let preset = match estimate_effect_preset(preset_name.clone(), run.reference, best.features)
        {
            Ok(preset) => preset,
            Err(err) => {
                self.chat_widget.add_error_message(format!(
                    "GPT-Live voice matching succeeded, but the draft effect could not be built: {err:#}"
                ));
                return;
            }
        };
        if self.realtime_voice_profile.is_none() && !self.realtime_voice_rotation_selected {
            match crate::config_update::write_config_batch(
                app_server.request_handle(),
                vec![crate::config_update::build_realtime_voice_edit(
                    best.voice.wire_name(),
                )],
            )
            .await
            {
                Ok(_) => self.config.realtime.voice = Some(best.voice),
                Err(err) => self.chat_widget.add_error_message(format!(
                    "Matched GPT-Live voice `{}` for this session, but saving the base voice failed: {err:#}",
                    best.voice.wire_name()
                )),
            }
        }
        self.chat_widget.add_info_message(
            format!(
                "GPT-Live calibration matched `{}` (score {score:.3}) from `{}`. Ranking: {}. Draft `{preset_name}` is loaded in the tuner; press `s` to save it under ~/.codex/voice-presets, then use `/voice on` to audition it.",
                best.voice.wire_name(),
                run.reference_path.display(),
                format_ranked_candidates(&result),
            ),
            None,
        );
        self.chat_widget.open_realtime_voice_tuner(preset);
    }

    async fn start_next_realtime_voice_calibration_candidate(
        &mut self,
        thread_id: ThreadId,
        app_server: &mut AppServerSession,
    ) -> anyhow::Result<()> {
        let voice = self
            .realtime_voice_calibration
            .as_ref()
            .and_then(VoiceCalibrationRun::current_voice)
            .context("no GPT-Live voice remains to calibrate")?;
        let requested_realtime_session_id =
            format!("{CALIBRATION_SESSION_ID_PREFIX}-{}", uuid::Uuid::new_v4());
        if let Some(run) = self.realtime_voice_calibration.as_mut() {
            run.begin_candidate(requested_realtime_session_id.clone());
        }
        self.realtime_voice_ignore_legacy_notifications = false;
        self.realtime_voice_requested_session_id = Some(requested_realtime_session_id.clone());
        let (session, sdp) = tokio::time::timeout(
            CALIBRATION_SETUP_TIMEOUT,
            RealtimeVoiceSession::start_for_calibration(&self.config.realtime_audio),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("timed out creating the temporary GPT-Live calibration audio session")
        })?
        .map_err(|err| {
            anyhow::anyhow!("creating the temporary GPT-Live calibration audio session: {err:#}")
        })?;
        session.set_output_muted(true);
        let params = ThreadRealtimeStartParams {
            thread_id: thread_id.to_string(),
            client_managed_handoffs: Some(true),
            delegation_ack_filler: None,
            flush_transcript_tail_on_session_end: Some(false),
            codex_responses_as_items: Some(false),
            codex_response_item_prefix: None,
            codex_response_handoff_mode: None,
            codex_response_handoff_channel_prefixes: None,
            model: Some("gpt-live-1-codex".to_string()),
            output_modality: RealtimeOutputModality::Audio,
            include_startup_context: Some(false),
            initial_items: None,
            realtime_start_instructions: None,
            realtime_end_instructions: None,
            prompt: Some(Some(format!(
                "{}\n\nThis is a voice calibration sample. When a phrase is supplied for speech, speak exactly that phrase naturally. Do not add a greeting, preamble, explanation, or extra words.",
                realtime_start_prompt(false)
                    .and_then(|prompt| prompt)
                    .unwrap_or_default()
            ))),
            realtime_session_id: Some(requested_realtime_session_id),
            transport: Some(ThreadRealtimeStartTransport::Webrtc { sdp }),
            version: Some(RealtimeConversationVersion::V3),
            voice: Some(realtime_v3_voice(Some(voice))),
        };
        let start_request_id = app_server.next_request_id();
        if let Some(run) = self.realtime_voice_calibration.as_mut() {
            run.set_pending_submission_id(start_request_id.to_string());
        }
        self.realtime_voice_session = Some(session);
        let start_result = tokio::time::timeout(
            CALIBRATION_SETUP_TIMEOUT,
            app_server.thread_realtime_start_with_request_id(start_request_id, params),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("timed out starting the temporary GPT-Live calibration session")
        })
        .and_then(|result| {
            result.map_err(|err| {
                anyhow::anyhow!("starting the temporary GPT-Live calibration session: {err:#}")
            })
        });
        start_result?;
        self.schedule_realtime_voice_calibration_poll();
        Ok(())
    }

    fn schedule_realtime_voice_calibration_poll(&self) {
        let Some(run_id) = self
            .realtime_voice_calibration
            .as_ref()
            .map(|run| run.run_id)
        else {
            return;
        };
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CALIBRATION_POLL_INTERVAL).await;
            app_event_tx.send(AppEvent::RealtimeVoiceCalibrationPoll { run_id });
        });
    }

    async fn abort_realtime_voice_calibration(
        &mut self,
        app_server: &mut AppServerSession,
        mut message: String,
    ) {
        let calibration_thread_id = self
            .realtime_voice_calibration
            .as_ref()
            .map(|run| run.thread_id);
        self.realtime_voice_calibration = None;
        self.realtime_voice_requested_session_id = None;
        self.realtime_voice_ignore_legacy_notifications = true;
        if let Some(session) = self.realtime_voice_session.take() {
            let stop_result = if let Some(thread_id) =
                calibration_thread_id.or(self.active_thread_id.or(self.chat_widget.thread_id()))
            {
                stop_realtime_voice_calibration_server_session(app_server, thread_id)
                    .await
                    .err()
            } else {
                None
            };
            session.close().await;
            if let Some(err) = stop_result {
                message = format!("{message}; cleanup also failed: {err:#}");
            }
        }
        self.chat_widget
            .add_error_message(format!("GPT-Live voice calibration stopped: {message}"));
    }
}
