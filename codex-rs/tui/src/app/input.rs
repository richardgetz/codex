//! Keyboard input, external editor, and status-line dispatch for the TUI app.
//!
//! This module owns global key bindings that sit above ChatWidget, including transcript overlay
//! entry, Ctrl-L clear, external editor launch, and agent navigation shortcuts.

use super::*;
use crate::app_backtrack::SIDE_EDIT_PREVIOUS_UNAVAILABLE_MESSAGE;
use crate::realtime_voice::DEFAULT_REALTIME_HOTKEY;
use crate::realtime_voice::RealtimeVoiceDebugCommand;
use crate::realtime_voice::RealtimeVoiceEffectCommand;
use crate::realtime_voice::RealtimeVoiceProfileCommand;
use crate::realtime_voice::realtime_hotkey_matches;
use crate::realtime_voice::realtime_hotkey_spec_from_event;
use crate::realtime_voice::realtime_start_prompt;
use crate::realtime_voice::realtime_v3_voice;
use crate::realtime_voice_devices::display_device_name;
use crate::realtime_voice_devices::format_device_aliases;
use crate::realtime_voice_devices::normalize_device_alias;
use crate::realtime_voice_devices::resolve_device_name;
use crate::realtime_voice_effects::VoiceEffectPreset;
use crate::realtime_voice_effects::active_preset_name;
use crate::realtime_voice_effects::list_preset_names;
use crate::realtime_voice_effects::load_active_preset;
use crate::realtime_voice_effects::load_named_preset;
use crate::realtime_voice_effects::preset_file_path;
use crate::realtime_voice_effects::save_preset;
use crate::realtime_voice_profiles::activate_preset_and_deactivate_profile;
use crate::realtime_voice_profiles::activate_profile;
use crate::realtime_voice_profiles::deactivate_profile;
use crate::realtime_voice_profiles::deactivate_profile_and_preset;
use crate::realtime_voice_profiles::list_profile_names;
use crate::realtime_voice_profiles::load_named_profile;
use crate::realtime_voice_profiles::profile_file_path;
use crate::realtime_voice_sound::RealtimeAcknowledgementSound;
use codex_protocol::models::MessagePhase;
use color_eyre::eyre::eyre;

const REALTIME_HANDOFF_DEBUG_DEDUPE_CAPACITY: usize = 128;
const REALTIME_HANDOFF_DEBUG_VALUE_LIMIT: usize = 96;
const REALTIME_OUTPUT_DEBUG_AUDIO_CHUNK_LIMIT: usize = 32;
const REALTIME_OUTPUT_DEBUG_TRANSCRIPT_DELTA_LIMIT: usize = 32;

fn realtime_handoff_debug_preview(value: &str) -> String {
    let mut preview = String::new();
    let mut character_count = 0;
    let mut needs_separator = false;
    let mut truncated = false;

    for word in value.split_whitespace() {
        if needs_separator {
            if character_count == REALTIME_HANDOFF_DEBUG_VALUE_LIMIT {
                truncated = true;
                break;
            }
            preview.push(' ');
            character_count += 1;
        }
        needs_separator = true;

        for character in word.chars() {
            if character_count == REALTIME_HANDOFF_DEBUG_VALUE_LIMIT {
                truncated = true;
                break;
            }
            preview.push(character);
            character_count += 1;
        }
        if truncated {
            break;
        }
    }

    if truncated {
        preview.push('…');
    }
    preview
}

fn realtime_handoff_debug_id_key(value: &str) -> u64 {
    use std::hash::Hash;
    use std::hash::Hasher;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone, Copy)]
enum RealtimeDevicePicker {
    Microphone,
    Speaker,
}

impl App {
    pub(super) fn handle_realtime_voice_effect_update(
        &mut self,
        preset: VoiceEffectPreset,
        persist: bool,
        bypass: bool,
    ) {
        let result = if bypass {
            self.realtime_voice_session
                .as_ref()
                .map_or(Ok(()), |session| session.set_voice_effect(None))
        } else {
            self.realtime_voice_session
                .as_ref()
                .map_or(Ok(()), |session| session.set_voice_effect(Some(&preset)))
        };
        if let Err(err) = result {
            self.chat_widget.add_error_message(format!(
                "Failed to update the live GPT-Live effect: {err:#}"
            ));
            return;
        }
        if persist && let Err(err) = save_preset(self.config.codex_home.as_path(), &preset) {
            self.chat_widget
                .add_error_message(format!("Failed to save the GPT-Live tuner preset: {err:#}"));
        }
    }

    pub(super) fn route_key_chord_event(
        &mut self,
        tui: &mut tui::Tui,
        key_event: KeyEvent,
    ) -> Option<KeyEvent> {
        let contexts = self.active_keymap_contexts();
        let was_pending = self.key_chord_matcher.is_pending();
        match self.key_chord_matcher.advance(
            key_event,
            &self.keymap.chords,
            contexts,
            tokio::time::Instant::now(),
        ) {
            crate::keymap::KeyChordMatch::PassThrough => {
                if was_pending && !self.key_chord_matcher.is_pending() {
                    self.chat_widget.set_footer_hint_override(/*items*/ None);
                }
                Some(key_event)
            }
            crate::keymap::KeyChordMatch::Pending(prefix) => {
                if self.backtrack.primed {
                    self.reset_backtrack_state();
                }
                self.chat_widget.set_footer_hint_override(Some(vec![
                    (
                        format!("{} …", prefix.display_label()),
                        "waiting for next key".to_string(),
                    ),
                    ("esc".to_string(), "cancel".to_string()),
                ]));
                tui.frame_requester()
                    .schedule_frame_in(crate::keymap::KEY_CHORD_TIMEOUT);
                None
            }
            crate::keymap::KeyChordMatch::Completed(dispatch_event) => {
                self.chat_widget.set_footer_hint_override(/*items*/ None);
                Some(dispatch_event)
            }
            crate::keymap::KeyChordMatch::Cancelled => {
                self.chat_widget.set_footer_hint_override(/*items*/ None);
                None
            }
            crate::keymap::KeyChordMatch::Ignored => None,
        }
    }

    pub(super) fn expire_pending_key_chord(&mut self) {
        let contexts = self.active_keymap_contexts();
        if self
            .key_chord_matcher
            .expire(contexts, tokio::time::Instant::now())
        {
            self.chat_widget.set_footer_hint_override(/*items*/ None);
        }
    }

    pub(super) fn cancel_pending_key_chord(&mut self) {
        if self.key_chord_matcher.cancel() {
            self.chat_widget.set_footer_hint_override(/*items*/ None);
        }
    }

    fn active_keymap_contexts(&self) -> crate::keymap::KeymapContextSet {
        if self.overlay.is_some() {
            return crate::keymap::KeymapContextSet::new(crate::keymap::KeymapContext::Pager);
        }

        let contexts = self.chat_widget.keymap_contexts();
        if self.chat_widget.no_modal_or_popup_active() {
            contexts
                .with(crate::keymap::KeymapContext::Global)
                .with(crate::keymap::KeymapContext::Chat)
        } else {
            contexts
        }
    }

    pub(super) async fn launch_external_editor(&mut self, tui: &mut tui::Tui) {
        let editor_cmd = match external_editor::resolve_editor_command() {
            Ok(cmd) => cmd,
            Err(external_editor::EditorError::MissingEditor) => {
                self.chat_widget
                    .add_to_history(history_cell::new_error_event(
                    "Cannot open external editor: set $VISUAL or $EDITOR before starting Codex."
                        .to_string(),
                ));
                self.reset_external_editor_state(tui);
                return;
            }
            Err(err) => {
                self.chat_widget
                    .add_to_history(history_cell::new_error_event(format!(
                        "Failed to open editor: {err}",
                    )));
                self.reset_external_editor_state(tui);
                return;
            }
        };

        let seed = self.chat_widget.composer_text_with_pending();
        let editor_result = tui
            .with_restored(|| async { external_editor::run_editor(&seed, &editor_cmd).await })
            .await;
        self.reset_external_editor_state(tui);

        match editor_result {
            Ok(new_text) => {
                // Trim trailing whitespace
                let cleaned = new_text.trim_end().to_string();
                self.chat_widget.apply_external_edit(cleaned);
            }
            Err(err) => {
                self.chat_widget
                    .add_to_history(history_cell::new_error_event(format!(
                        "Failed to open editor: {err}",
                    )));
            }
        }
        tui.frame_requester().schedule_frame();
    }

    pub(super) fn request_external_editor_launch(&mut self, tui: &mut tui::Tui) {
        self.chat_widget
            .set_external_editor_state(ExternalEditorState::Requested);
        self.chat_widget.set_footer_hint_override(Some(vec![(
            EXTERNAL_EDITOR_HINT.to_string(),
            String::new(),
        )]));
        tui.frame_requester().schedule_frame();
    }

    pub(super) fn reset_external_editor_state(&mut self, tui: &mut tui::Tui) {
        self.chat_widget
            .set_external_editor_state(ExternalEditorState::Closed);
        self.chat_widget.set_footer_hint_override(/*items*/ None);
        tui.frame_requester().schedule_frame();
    }

    pub(super) fn apply_raw_output_mode(
        &mut self,
        tui: &mut tui::Tui,
        enabled: bool,
        notify: bool,
    ) {
        if notify {
            self.chat_widget.set_raw_output_mode_and_notify(enabled);
        } else {
            self.chat_widget.set_raw_output_mode(enabled);
        }
        let terminal_width = tui.terminal.last_known_screen_size.into();
        if let Err(err) = self.reflow_transcript_now(tui, terminal_width) {
            tracing::warn!(error = %err, "failed to reflow transcript after raw output mode toggle");
            self.chat_widget
                .add_error_message(format!("Failed to redraw transcript: {err}"));
        }
        tui.frame_requester().schedule_frame();
    }

    pub(super) async fn handle_key_event(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        key_event: KeyEvent,
    ) {
        if self.handle_realtime_voice_key(app_server, key_event).await {
            return;
        }

        // Some terminals, especially on macOS, encode Option+Left/Right as Option+b/f unless
        // enhanced keyboard reporting is available. We only treat those word-motion fallbacks as
        // agent-switch shortcuts when the composer is empty so we never steal the expected
        // editing behavior for moving across words inside a draft.
        let allow_agent_word_motion_fallback = !self.enhanced_keys_supported
            && self.chat_widget.composer_text_with_pending().is_empty();
        if self.overlay.is_none()
            && self.chat_widget.no_modal_or_popup_active()
            // Alt+Left/Right are also natural word-motion keys in the composer. Keep agent
            // fast-switch available only once the draft is empty so editing behavior wins whenever
            // there is text on screen.
            && self.chat_widget.composer_text_with_pending().is_empty()
            && previous_agent_shortcut_matches(key_event, allow_agent_word_motion_fallback)
        {
            if let Some(thread_id) = self
                .adjacent_thread_id_with_backfill(app_server, AgentNavigationDirection::Previous)
                .await
            {
                let _ = self
                    .select_agent_thread_and_discard_side(tui, app_server, thread_id)
                    .await;
            }
            return;
        }
        if self.overlay.is_none()
            && self.chat_widget.no_modal_or_popup_active()
            // Mirror the previous-agent rule above: empty drafts may use these keys for thread
            // switching, but non-empty drafts keep them for expected word-wise cursor motion.
            && self.chat_widget.composer_text_with_pending().is_empty()
            && next_agent_shortcut_matches(key_event, allow_agent_word_motion_fallback)
        {
            if let Some(thread_id) = self
                .adjacent_thread_id_with_backfill(app_server, AgentNavigationDirection::Next)
                .await
            {
                let _ = self
                    .select_agent_thread_and_discard_side(tui, app_server, thread_id)
                    .await;
            }
            return;
        }
        if side_return_shortcut_matches(key_event)
            && self.maybe_return_from_side(tui, app_server).await
        {
            return;
        }

        let app_keymap_shortcuts_available = self.app_keymap_shortcuts_available();

        let side_toggle_bindings = &self.keymap.app.toggle_side_conversation;
        if app_keymap_shortcuts_available
            && (side_toggle_bindings.is_pressed(key_event)
                || side_toggle_bindings.contains(&crate::key_hint::ctrl(KeyCode::Char('/')))
                    && crate::key_hint::ctrl(KeyCode::Char('7')).is_press(key_event))
        {
            if let Err(err) = self.toggle_side_conversation(tui, app_server).await {
                self.chat_widget
                    .add_error_message(format!("Failed to switch side conversation: {err}"));
            }
            return;
        }

        if app_keymap_shortcuts_available && self.keymap.app.toggle_vim_mode.is_pressed(key_event) {
            self.chat_widget.toggle_vim_mode_and_notify();
            return;
        }

        if app_keymap_shortcuts_available
            && self.keymap.app.toggle_fast_mode.is_pressed(key_event)
            && self.chat_widget.can_toggle_fast_mode_from_keybinding()
        {
            self.chat_widget.toggle_fast_mode_from_ui();
            return;
        }

        if app_keymap_shortcuts_available && self.keymap.app.toggle_raw_output.is_pressed(key_event)
        {
            let enabled = !self.chat_widget.raw_output_mode();
            self.apply_raw_output_mode(tui, enabled, /*notify*/ false);
            return;
        }

        if app_keymap_shortcuts_available && self.keymap.app.open_transcript.is_pressed(key_event) {
            self.scrollback_has_older_history = self
                .chat_widget
                .thread_id()
                .is_some_and(|thread_id| app_server.has_older_history(thread_id));
            self.open_transcript_overlay(tui);
            return;
        }

        if app_keymap_shortcuts_available
            && self.keymap.app.open_external_editor.is_pressed(key_event)
        {
            // Only launch the external editor if there is no overlay and the bottom pane is not in use.
            // Note that it can be launched while a task is running to enable editing while the previous turn is ongoing.
            if self.overlay.is_none()
                && self.chat_widget.can_launch_external_editor()
                && self.chat_widget.external_editor_state() == ExternalEditorState::Closed
            {
                self.request_external_editor_launch(tui);
            }
            return;
        }

        if matches!(key_event.code, KeyCode::Esc)
            && matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            // Esc primes/advances backtracking only in normal (not working) mode
            // with the composer focused and empty. In any other state, forward
            // Esc so the active UI (e.g. status indicator, modals, popups)
            // handles it.
            if self.should_handle_backtrack_esc(key_event) {
                self.handle_backtrack_esc_key(tui);
            } else if self.should_reject_side_backtrack_esc(key_event) {
                self.reject_side_backtrack_esc();
            } else {
                self.chat_widget.handle_key_event(key_event);
            }
            return;
        }

        match key_event {
            _ if app_keymap_shortcuts_available
                && self.keymap.app.clear_terminal.is_pressed(key_event) =>
            {
                if !self.chat_widget.can_run_ctrl_l_clear_now() {
                    return;
                }
                if let Err(err) = self.clear_terminal_ui(tui, /*redraw_header*/ false) {
                    tracing::warn!(error = %err, "failed to clear terminal UI");
                    self.chat_widget
                        .add_error_message(format!("Failed to clear terminal UI: {err}"));
                } else {
                    self.reset_app_ui_state_after_clear();
                    self.queue_clear_ui_header(tui);
                    tui.frame_requester().schedule_frame();
                }
            }
            // Enter confirms backtrack when primed + count > 0. Otherwise pass to widget.
            KeyEvent {
                code: KeyCode::Enter,
                kind: KeyEventKind::Press,
                ..
            } if self.backtrack.primed
                && self.backtrack.nth_user_message != usize::MAX
                && self.chat_widget.composer_is_empty() =>
            {
                if let Some(selection) = self.confirm_backtrack_from_main() {
                    self.apply_backtrack_selection(selection);
                    tui.frame_requester().schedule_frame();
                }
            }
            KeyEvent {
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                // Any non-Esc key press should cancel a primed backtrack.
                // This avoids stale "Esc-primed" state after the user starts typing
                // (even if they later backspace to empty).
                if key_event.code != KeyCode::Esc && self.backtrack.primed {
                    self.reset_backtrack_state();
                }
                self.chat_widget.handle_key_event(key_event);
            }
            _ => {
                self.chat_widget.handle_key_event(key_event);
            }
        };
    }

    async fn handle_realtime_voice_key(
        &mut self,
        app_server: &mut AppServerSession,
        key_event: KeyEvent,
    ) -> bool {
        if !self.config.realtime.enabled {
            return false;
        }

        if self.realtime_mic_mode == RealtimeMicMode::CaptureHotkey {
            if !matches!(
                key_event.kind,
                crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
            ) {
                return true;
            }
            let Some(hotkey) = realtime_hotkey_spec_from_event(key_event) else {
                self.chat_widget.add_error_message(
                    "That key cannot be used as a push-to-talk binding; try another key."
                        .to_string(),
                );
                return true;
            };
            match crate::config_update::write_config_batch(
                app_server.request_handle(),
                vec![crate::config_update::build_realtime_hotkey_edit(&hotkey)],
            )
            .await
            {
                Ok(_) => {
                    self.config.realtime.hotkey = Some(hotkey.clone());
                    self.realtime_mic_mode = RealtimeMicMode::PushToTalk;
                    self.chat_widget.add_info_message(
                        format!(
                            "Push-to-talk key set to `{hotkey}`. Hold it to speak; release it to send."
                        ),
                        None,
                    );
                }
                Err(err) => self.chat_widget.add_error_message(format!(
                    "Failed to save push-to-talk key `{hotkey}`: {err:#}"
                )),
            }
            return true;
        }

        if !is_realtime_voice_key(self.config.realtime.hotkey.as_deref(), key_event) {
            return false;
        }

        if self.realtime_mic_mode == RealtimeMicMode::Disabled {
            return false;
        }

        if self.realtime_mic_mode != RealtimeMicMode::PushToTalk {
            return true;
        }

        match key_event.kind {
            KeyEventKind::Press | KeyEventKind::Repeat => {
                if let Some(session) = &self.realtime_voice_session {
                    session.set_input_muted(false);
                    return true;
                }

                if let Err(err) = self.start_realtime_voice_session(app_server).await {
                    self.chat_widget
                        .add_error_message(format!("Failed to start live voice input: {err:#}"));
                    return true;
                }
                true
            }
            KeyEventKind::Release => {
                if let Some(session) = &self.realtime_voice_session {
                    session.set_input_muted(true);
                    session.play_acknowledgement_sound();
                }
                true
            }
        }
    }

    async fn start_realtime_voice_session(
        &mut self,
        app_server: &mut AppServerSession,
    ) -> Result<()> {
        if self.realtime_voice_session.is_some() {
            return Ok(());
        }

        let Some(thread_id) = self.active_thread_id.or(self.chat_widget.thread_id()) else {
            return Err(eyre!("Voice input is unavailable until a thread starts."));
        };

        let acknowledgement_sound = if self.config.realtime.acknowledgement_sound {
            match self.config.realtime.acknowledgement_sound_file.as_ref() {
                Some(path) => RealtimeAcknowledgementSound::File(path.as_path().to_path_buf()),
                None => RealtimeAcknowledgementSound::BuiltIn,
            }
        } else {
            RealtimeAcknowledgementSound::Disabled
        };
        let voice_effect = if let Some(profile_name) = self
            .realtime_voice_profile
            .as_ref()
            .map(|profile| profile.name.clone())
        {
            let profile = load_named_profile(self.config.codex_home.as_path(), &profile_name)
                .map_err(|err| {
                    eyre!(
                        "Failed to reload GPT-Live voice profile '{}': {err:#}",
                        profile_name
                    )
                })?;
            self.config.realtime.voice = Some(profile.voice);
            self.realtime_voice_profile = Some(profile.clone());
            load_named_preset(self.config.codex_home.as_path(), &profile.effect)
                .map(Some)
                .map_err(|err| {
                    eyre!(
                        "Failed to load the effect for GPT-Live voice profile '{}': {err:#}",
                        profile.name
                    )
                })?
        } else if self.realtime_voice_rotation_selected {
            None
        } else {
            load_active_preset(self.config.codex_home.as_path())
                .map_err(|err| eyre!("Failed to load the active GPT-Live voice effect: {err:#}"))?
        };
        let (session, sdp) = RealtimeVoiceSession::start(
            &self.config.realtime_audio,
            &acknowledgement_sound,
            voice_effect,
        )
        .await
        .map_err(|err| eyre!("{err:#}"))?;
        let params = ThreadRealtimeStartParams {
            thread_id: thread_id.to_string(),
            client_managed_handoffs: None,
            delegation_ack_filler: None,
            flush_transcript_tail_on_session_end: Some(true),
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
            prompt: realtime_start_prompt(self.config.realtime.enable_preambles),
            realtime_session_id: None,
            transport: Some(ThreadRealtimeStartTransport::Webrtc { sdp }),
            version: Some(RealtimeConversationVersion::V3),
            voice: Some(realtime_v3_voice(self.config.realtime.voice)),
        };
        if let Err(err) = app_server.thread_realtime_start_with_params(params).await {
            session.close().await;
            return Err(err);
        }
        self.realtime_voice_session = Some(session);
        Ok(())
    }

    fn show_realtime_device_picker(
        &mut self,
        picker: RealtimeDevicePicker,
        devices: Vec<String>,
        selected: Option<&str>,
    ) {
        let (title, subtitle) = match picker {
            RealtimeDevicePicker::Microphone => (
                "Select realtime microphone",
                "Choose the GPT-Live input device. Changes apply to the next voice session.",
            ),
            RealtimeDevicePicker::Speaker => (
                "Select realtime speaker",
                "Choose the GPT-Live output device. Changes apply to the next voice session.",
            ),
        };
        let aliases = match picker {
            RealtimeDevicePicker::Microphone => {
                self.config.realtime_audio.microphone_aliases.clone()
            }
            RealtimeDevicePicker::Speaker => self.config.realtime_audio.speaker_aliases.clone(),
        };
        let selected_device =
            selected.and_then(|selected| resolve_device_name(selected, &devices, &aliases));
        let initial_selected_idx = devices
            .iter()
            .position(|name| selected_device.as_deref() == Some(name.as_str()));
        let items = devices
            .into_iter()
            .map(|name| {
                let is_current = selected_device.as_deref() == Some(name.as_str());
                let command_name = name.clone();
                let display_name = display_device_name(&name, &aliases);
                let command = match picker {
                    RealtimeDevicePicker::Microphone => {
                        RealtimeMicCommand::SetMicrophone(command_name)
                    }
                    RealtimeDevicePicker::Speaker => RealtimeMicCommand::SetSpeaker(command_name),
                };
                SelectionItem {
                    name: display_name,
                    is_current,
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::RealtimeMicControl(command.clone()));
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some(title.to_string()),
            subtitle: Some(subtitle.to_string()),
            items,
            initial_selected_idx,
            ..Default::default()
        });
    }

    pub(super) async fn handle_realtime_mic_command(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        command: RealtimeMicCommand,
    ) {
        match &command {
            RealtimeMicCommand::CaptureHotkey => {
                if !self.config.realtime.enabled {
                    self.chat_widget.add_error_message(
                        "Microphone is disabled by config. Set `[realtime].enabled = true` before capturing a key."
                            .to_string(),
                    );
                } else {
                    self.realtime_mic_mode = RealtimeMicMode::CaptureHotkey;
                    if let Some(session) = &self.realtime_voice_session {
                        session.set_input_muted(true);
                    }
                    self.chat_widget.add_info_message(
                        "Press the key you want to use for push-to-talk. Capture is armed for the next key event."
                            .to_string(),
                        None,
                    );
                }
                tui.frame_requester().schedule_frame();
                return;
            }
            RealtimeMicCommand::ChangeMicrophone => {
                match crate::realtime_voice_audio::list_input_devices() {
                    Ok(devices) if devices.is_empty() => self.chat_widget.add_info_message(
                        "No realtime microphone devices were found.".to_string(),
                        None,
                    ),
                    Ok(devices) => {
                        let selected = self.config.realtime_audio.microphone.clone();
                        self.show_realtime_device_picker(
                            RealtimeDevicePicker::Microphone,
                            devices,
                            selected.as_deref(),
                        );
                    }
                    Err(err) => self
                        .chat_widget
                        .add_error_message(format!("Failed to list realtime microphones: {err:#}")),
                }
                tui.frame_requester().schedule_frame();
                return;
            }
            RealtimeMicCommand::ListDevices => {
                match crate::realtime_voice_audio::list_input_devices() {
                    Ok(devices) if devices.is_empty() => self.chat_widget.add_info_message(
                        "No realtime microphone devices were found.".to_string(),
                        None,
                    ),
                    Ok(devices) => {
                        let selected = self
                            .config
                            .realtime_audio
                            .microphone
                            .as_deref()
                            .unwrap_or("system default");
                        let aliases = &self.config.realtime_audio.microphone_aliases;
                        let devices = devices
                            .iter()
                            .map(|device| display_device_name(device, aliases))
                            .collect::<Vec<_>>();
                        let aliases_text = format_device_aliases(aliases);
                        let aliases_text = if aliases_text.is_empty() {
                            String::new()
                        } else {
                            format!("\nAliases:\n{aliases_text}")
                        };
                        self.chat_widget.add_info_message(
                            format!(
                                "Realtime microphones (selected: {selected}):\n{}{aliases_text}",
                                devices.join("\n")
                            ),
                            None,
                        );
                    }
                    Err(err) => self
                        .chat_widget
                        .add_error_message(format!("Failed to list realtime microphones: {err:#}")),
                }
                tui.frame_requester().schedule_frame();
                return;
            }
            RealtimeMicCommand::ChangeSpeaker => {
                match crate::realtime_voice_audio::list_output_devices() {
                    Ok(devices) if devices.is_empty() => self.chat_widget.add_info_message(
                        "No realtime speaker devices were found.".to_string(),
                        None,
                    ),
                    Ok(devices) => {
                        let selected = self.config.realtime_audio.speaker.clone();
                        self.show_realtime_device_picker(
                            RealtimeDevicePicker::Speaker,
                            devices,
                            selected.as_deref(),
                        );
                    }
                    Err(err) => self
                        .chat_widget
                        .add_error_message(format!("Failed to list realtime speakers: {err:#}")),
                }
                tui.frame_requester().schedule_frame();
                return;
            }
            RealtimeMicCommand::ListSpeakers => {
                match crate::realtime_voice_audio::list_output_devices() {
                    Ok(devices) if devices.is_empty() => self.chat_widget.add_info_message(
                        "No realtime speaker devices were found.".to_string(),
                        None,
                    ),
                    Ok(devices) => {
                        let selected = self
                            .config
                            .realtime_audio
                            .speaker
                            .as_deref()
                            .unwrap_or("system default");
                        let aliases = &self.config.realtime_audio.speaker_aliases;
                        let devices = devices
                            .iter()
                            .map(|device| display_device_name(device, aliases))
                            .collect::<Vec<_>>();
                        let aliases_text = format_device_aliases(aliases);
                        let aliases_text = if aliases_text.is_empty() {
                            String::new()
                        } else {
                            format!("\nAliases:\n{aliases_text}")
                        };
                        self.chat_widget.add_info_message(
                            format!(
                                "Realtime speakers (selected: {selected}):\n{}{aliases_text}",
                                devices.join("\n")
                            ),
                            None,
                        );
                    }
                    Err(err) => self
                        .chat_widget
                        .add_error_message(format!("Failed to list realtime speakers: {err:#}")),
                }
                tui.frame_requester().schedule_frame();
                return;
            }
            RealtimeMicCommand::ListMicrophoneAliases => {
                let aliases = format_device_aliases(&self.config.realtime_audio.microphone_aliases);
                let message = if aliases.is_empty() {
                    "No realtime microphone aliases are configured.".to_string()
                } else {
                    format!("Realtime microphone aliases:\n{aliases}")
                };
                self.chat_widget.add_info_message(message, None);
                tui.frame_requester().schedule_frame();
                return;
            }
            RealtimeMicCommand::ListSpeakerAliases => {
                let aliases = format_device_aliases(&self.config.realtime_audio.speaker_aliases);
                let message = if aliases.is_empty() {
                    "No realtime speaker aliases are configured.".to_string()
                } else {
                    format!("Realtime speaker aliases:\n{aliases}")
                };
                self.chat_widget.add_info_message(message, None);
                tui.frame_requester().schedule_frame();
                return;
            }
            RealtimeMicCommand::SetMicrophoneAlias { alias, device } => {
                let Some(alias) = normalize_device_alias(alias) else {
                    self.chat_widget.add_error_message(
                        "Microphone aliases must be a single non-empty word.".to_string(),
                    );
                    tui.frame_requester().schedule_frame();
                    return;
                };
                let requested =
                    device
                        .as_deref()
                        .or(self.config.realtime_audio.microphone.as_deref());
                let Some(requested) = requested else {
                    self.chat_widget.add_error_message(
                        "Choose a microphone first or provide its full device name.".to_string(),
                    );
                    tui.frame_requester().schedule_frame();
                    return;
                };
                let devices = match crate::realtime_voice_audio::list_input_devices() {
                    Ok(devices) => devices,
                    Err(err) => {
                        self.chat_widget.add_error_message(format!(
                            "Failed to list realtime microphones: {err:#}"
                        ));
                        tui.frame_requester().schedule_frame();
                        return;
                    }
                };
                let Some(device) = resolve_device_name(
                    requested,
                    &devices,
                    &self.config.realtime_audio.microphone_aliases,
                ) else {
                    self.chat_widget.add_error_message(format!(
                        "Cannot create microphone alias `{alias}`: device `{requested}` was not found."
                    ));
                    tui.frame_requester().schedule_frame();
                    return;
                };
                match crate::config_update::write_config_batch(
                    app_server.request_handle(),
                    vec![crate::config_update::build_realtime_microphone_alias_edit(
                        &alias, &device,
                    )],
                )
                .await
                {
                    Ok(_) => {
                        self.config
                            .realtime_audio
                            .microphone_aliases
                            .insert(alias.clone(), device.clone());
                        self.chat_widget.add_info_message(
                            format!("Realtime microphone alias `{alias}` now selects `{device}`."),
                            None,
                        );
                    }
                    Err(err) => self.chat_widget.add_error_message(format!(
                        "Failed to save realtime microphone alias `{alias}`: {err:#}"
                    )),
                }
                tui.frame_requester().schedule_frame();
                return;
            }
            RealtimeMicCommand::SetSpeakerAlias { alias, device } => {
                let Some(alias) = normalize_device_alias(alias) else {
                    self.chat_widget.add_error_message(
                        "Speaker aliases must be a single non-empty word.".to_string(),
                    );
                    tui.frame_requester().schedule_frame();
                    return;
                };
                let requested = device
                    .as_deref()
                    .or(self.config.realtime_audio.speaker.as_deref());
                let Some(requested) = requested else {
                    self.chat_widget.add_error_message(
                        "Choose a speaker first or provide its full device name.".to_string(),
                    );
                    tui.frame_requester().schedule_frame();
                    return;
                };
                let devices = match crate::realtime_voice_audio::list_output_devices() {
                    Ok(devices) => devices,
                    Err(err) => {
                        self.chat_widget.add_error_message(format!(
                            "Failed to list realtime speakers: {err:#}"
                        ));
                        tui.frame_requester().schedule_frame();
                        return;
                    }
                };
                let Some(device) = resolve_device_name(
                    requested,
                    &devices,
                    &self.config.realtime_audio.speaker_aliases,
                ) else {
                    self.chat_widget.add_error_message(format!(
                        "Cannot create speaker alias `{alias}`: device `{requested}` was not found."
                    ));
                    tui.frame_requester().schedule_frame();
                    return;
                };
                match crate::config_update::write_config_batch(
                    app_server.request_handle(),
                    vec![crate::config_update::build_realtime_speaker_alias_edit(
                        &alias, &device,
                    )],
                )
                .await
                {
                    Ok(_) => {
                        self.config
                            .realtime_audio
                            .speaker_aliases
                            .insert(alias.clone(), device.clone());
                        self.chat_widget.add_info_message(
                            format!("Realtime speaker alias `{alias}` now selects `{device}`."),
                            None,
                        );
                    }
                    Err(err) => self.chat_widget.add_error_message(format!(
                        "Failed to save realtime speaker alias `{alias}`: {err:#}"
                    )),
                }
                tui.frame_requester().schedule_frame();
                return;
            }
            RealtimeMicCommand::SetMicrophone(name) => {
                let requested = name.trim();
                if requested.is_empty() {
                    self.chat_widget.add_error_message(
                        "Usage: /mic [on|off|status|hot|push|hotkey|change|devices|aliases|alias <name> [device]|device <name>|speakers|speaker change|speaker aliases|speaker alias <name> [device]|speaker <name>]"
                            .to_string(),
                    );
                    tui.frame_requester().schedule_frame();
                    return;
                }
                let devices = match crate::realtime_voice_audio::list_input_devices() {
                    Ok(devices) => devices,
                    Err(err) => {
                        self.chat_widget.add_error_message(format!(
                            "Failed to list realtime microphones: {err:#}"
                        ));
                        tui.frame_requester().schedule_frame();
                        return;
                    }
                };
                let Some(name) = resolve_device_name(
                    requested,
                    &devices,
                    &self.config.realtime_audio.microphone_aliases,
                ) else {
                    self.chat_widget.add_error_message(format!(
                        "Cannot select realtime microphone `{requested}`: device or alias was not found."
                    ));
                    tui.frame_requester().schedule_frame();
                    return;
                };
                let host = cpal::default_host();
                if let Err(err) =
                    crate::realtime_voice_audio::select_input_device(&host, Some(&name))
                {
                    self.chat_widget.add_error_message(format!(
                        "Cannot select realtime microphone `{requested}`: {err:#}"
                    ));
                    tui.frame_requester().schedule_frame();
                    return;
                }
                match crate::config_update::write_config_batch(
                    app_server.request_handle(),
                    vec![crate::config_update::build_realtime_microphone_edit(&name)],
                )
                .await
                {
                    Ok(_) => {
                        self.config.realtime_audio.microphone = Some(name.clone());
                        self.chat_widget.add_info_message(
                            format!(
                                "Realtime microphone set to `{name}`. It will apply to the next voice session."
                            ),
                            None,
                        );
                    }
                    Err(err) => self.chat_widget.add_error_message(format!(
                        "Failed to save realtime microphone `{name}`: {err:#}"
                    )),
                }
                tui.frame_requester().schedule_frame();
                return;
            }
            RealtimeMicCommand::SetSpeaker(name) => {
                let requested = name.trim();
                if requested.is_empty() {
                    self.chat_widget.add_error_message(
                        "Usage: /mic [on|off|status|hot|push|hotkey|change|devices|aliases|alias <name> [device]|device <name>|speakers|speaker change|speaker aliases|speaker alias <name> [device]|speaker <name>]"
                            .to_string(),
                    );
                    tui.frame_requester().schedule_frame();
                    return;
                }
                let devices = match crate::realtime_voice_audio::list_output_devices() {
                    Ok(devices) => devices,
                    Err(err) => {
                        self.chat_widget.add_error_message(format!(
                            "Failed to list realtime speakers: {err:#}"
                        ));
                        tui.frame_requester().schedule_frame();
                        return;
                    }
                };
                let Some(name) = resolve_device_name(
                    requested,
                    &devices,
                    &self.config.realtime_audio.speaker_aliases,
                ) else {
                    self.chat_widget.add_error_message(format!(
                        "Cannot select realtime speaker `{requested}`: device or alias was not found."
                    ));
                    tui.frame_requester().schedule_frame();
                    return;
                };
                let host = cpal::default_host();
                if let Err(err) =
                    crate::realtime_voice_audio::select_output_device(&host, Some(&name))
                {
                    self.chat_widget.add_error_message(format!(
                        "Cannot select realtime speaker `{requested}`: {err:#}"
                    ));
                    tui.frame_requester().schedule_frame();
                    return;
                }
                match crate::config_update::write_config_batch(
                    app_server.request_handle(),
                    vec![crate::config_update::build_realtime_speaker_edit(&name)],
                )
                .await
                {
                    Ok(_) => {
                        self.config.realtime_audio.speaker = Some(name.clone());
                        self.chat_widget.add_info_message(
                            format!(
                                "Realtime speaker set to `{name}`. GPT-Live output will use it on the next voice session."
                            ),
                            None,
                        );
                    }
                    Err(err) => self.chat_widget.add_error_message(format!(
                        "Failed to save realtime speaker `{name}`: {err:#}"
                    )),
                }
                tui.frame_requester().schedule_frame();
                return;
            }
            RealtimeMicCommand::Status => {}
            RealtimeMicCommand::Toggle
            | RealtimeMicCommand::On
            | RealtimeMicCommand::Off
            | RealtimeMicCommand::Hot
            | RealtimeMicCommand::Push => {}
        }

        if matches!(&command, RealtimeMicCommand::Status) {
            let status = if self.config.realtime.enabled {
                self.realtime_mic_mode.status_label().to_string()
            } else {
                "disabled by config ([realtime].enabled = false)".to_string()
            };
            let microphone = self
                .config
                .realtime_audio
                .microphone
                .as_deref()
                .unwrap_or("system default");
            let speaker = self
                .config
                .realtime_audio
                .speaker
                .as_deref()
                .unwrap_or("system default");
            let hotkey = self
                .config
                .realtime
                .hotkey
                .as_deref()
                .unwrap_or(DEFAULT_REALTIME_HOTKEY);
            let preambles = if self.config.realtime.enable_preambles {
                "enabled"
            } else {
                "suppressed"
            };
            let acknowledgement_sound = if !self.config.realtime.acknowledgement_sound {
                "off"
            } else if self.config.realtime.acknowledgement_sound_file.is_some() {
                "custom WAV"
            } else {
                "built-in"
            };
            self.chat_widget
                .add_info_message(
                    format!(
                        "Microphone is {status}; input device: {microphone}; output device: {speaker}; push-to-talk key: {hotkey}; preambles: {preambles}; acknowledgement sound: {acknowledgement_sound}."
                    ),
                    None,
                );
            tui.frame_requester().schedule_frame();
            return;
        }

        if !self.config.realtime.enabled && !matches!(&command, RealtimeMicCommand::Off) {
            self.chat_widget.add_error_message(
                "Microphone is disabled by config. Set `[realtime].enabled = true` to use `/mic`."
                    .to_string(),
            );
            tui.frame_requester().schedule_frame();
            return;
        }

        let target_mode = match command {
            RealtimeMicCommand::Toggle => match self.realtime_mic_mode {
                RealtimeMicMode::Disabled => RealtimeMicMode::PushToTalk,
                RealtimeMicMode::PushToTalk
                | RealtimeMicMode::Hot
                | RealtimeMicMode::CaptureHotkey => RealtimeMicMode::Disabled,
            },
            RealtimeMicCommand::On => RealtimeMicMode::PushToTalk,
            RealtimeMicCommand::Off => RealtimeMicMode::Disabled,
            RealtimeMicCommand::Status
            | RealtimeMicCommand::CaptureHotkey
            | RealtimeMicCommand::ChangeMicrophone
            | RealtimeMicCommand::ListDevices
            | RealtimeMicCommand::ChangeSpeaker
            | RealtimeMicCommand::ListSpeakers
            | RealtimeMicCommand::SetMicrophone(_)
            | RealtimeMicCommand::SetSpeaker(_)
            | RealtimeMicCommand::ListMicrophoneAliases
            | RealtimeMicCommand::ListSpeakerAliases
            | RealtimeMicCommand::SetMicrophoneAlias { .. }
            | RealtimeMicCommand::SetSpeakerAlias { .. } => {
                unreachable!("non-mode command handled above")
            }
            RealtimeMicCommand::Hot => RealtimeMicMode::Hot,
            RealtimeMicCommand::Push => RealtimeMicMode::PushToTalk,
        };
        let previous_mode = self.realtime_mic_mode;
        self.realtime_mic_mode = target_mode;

        let result = match target_mode {
            RealtimeMicMode::Disabled => {
                self.stop_realtime_voice(app_server).await;
                Ok(())
            }
            RealtimeMicMode::PushToTalk => {
                if let Some(session) = &self.realtime_voice_session {
                    session.set_input_muted(true);
                }
                Ok(())
            }
            RealtimeMicMode::Hot => {
                if let Some(session) = &self.realtime_voice_session {
                    session.set_input_muted(false);
                    Ok(())
                } else {
                    self.start_realtime_voice_session(app_server).await
                }
            }
            RealtimeMicMode::CaptureHotkey => {
                unreachable!("hotkey capture is handled before mode changes")
            }
        };

        if let Err(err) = result {
            self.realtime_mic_mode = previous_mode;
            self.chat_widget
                .add_error_message(format!("Failed to enable hot microphone: {err:#}"));
        } else {
            self.chat_widget.add_info_message(
                format!("Microphone is now {}.", target_mode.status_label()),
                None,
            );
        }
        tui.frame_requester().schedule_frame();
    }

    pub(super) async fn handle_realtime_voice_command(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        command: RealtimeVoiceCommand,
    ) {
        match command {
            RealtimeVoiceCommand::On => {
                self.handle_realtime_mic_command(tui, app_server, RealtimeMicCommand::On)
                    .await;
                return;
            }
            RealtimeVoiceCommand::Off => {
                self.handle_realtime_mic_command(tui, app_server, RealtimeMicCommand::Off)
                    .await;
                return;
            }
            RealtimeVoiceCommand::Status => {
                let voice = realtime_v3_voice(self.config.realtime.voice);
                let microphone = if self.config.realtime.enabled {
                    self.realtime_mic_mode.status_label().to_string()
                } else {
                    "disabled by config ([realtime].enabled = false)".to_string()
                };
                let rotation = match self.config.realtime.voice_rotation.as_deref() {
                    Some(voices) if !voices.is_empty() => voices
                        .iter()
                        .map(|voice| voice.wire_name())
                        .collect::<Vec<_>>()
                        .join(", "),
                    _ => "off".to_string(),
                };
                let profile_rotation = self
                    .config
                    .realtime
                    .voice_profile_rotation
                    .as_deref()
                    .filter(|profiles| !profiles.is_empty())
                    .map_or_else(|| "off".to_string(), |profiles| profiles.join(", "));
                let profile = self
                    .realtime_voice_profile
                    .as_ref()
                    .map_or_else(|| "off".to_string(), |profile| profile.name.clone());
                self.chat_widget.add_info_message(
                    format!(
                        "Realtime voice is `{}` (GPT-Live WebRTC; applies to the next voice session); profile: {profile}; microphone is {microphone}; startup rotation: {rotation}; profile rotation: {profile_rotation}; handoff debug: {}.",
                        voice.wire_name(),
                        if self.realtime_voice_debug { "on" } else { "off" },
                    ),
                    None,
                );
            }
            RealtimeVoiceCommand::Debug(command) => {
                let message = match command {
                    RealtimeVoiceDebugCommand::Toggle => {
                        self.realtime_voice_debug = !self.realtime_voice_debug;
                        self.clear_realtime_debug_state();
                        format!(
                            "Realtime voice handoff debug is now {} (session-local).",
                            if self.realtime_voice_debug {
                                "on"
                            } else {
                                "off"
                            }
                        )
                    }
                    RealtimeVoiceDebugCommand::On => {
                        self.realtime_voice_debug = true;
                        self.clear_realtime_debug_state();
                        "Realtime voice handoff debug is on (session-local).".to_string()
                    }
                    RealtimeVoiceDebugCommand::Off => {
                        self.realtime_voice_debug = false;
                        self.clear_realtime_debug_state();
                        "Realtime voice handoff debug is off (session-local).".to_string()
                    }
                    RealtimeVoiceDebugCommand::Status => format!(
                        "Realtime voice handoff debug is {} (session-local; default off).",
                        if self.realtime_voice_debug {
                            "on"
                        } else {
                            "off"
                        }
                    ),
                };
                self.chat_widget.add_info_message(message, None);
            }
            RealtimeVoiceCommand::Profile(command) => {
                let codex_home = self.config.codex_home.as_path();
                match command {
                    RealtimeVoiceProfileCommand::List => match list_profile_names(codex_home) {
                        Ok(names) => {
                            let active = self
                                .realtime_voice_profile
                                .as_ref()
                                .map(|profile| profile.name.as_str())
                                .unwrap_or("off");
                            let names = names
                                .iter()
                                .map(|name| {
                                    if name == active {
                                        format!("{name} (selected)")
                                    } else {
                                        name.to_string()
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            self.chat_widget.add_info_message(
                                format!("GPT-Live voice profiles (selected: {active}): {names}"),
                                None,
                            );
                        }
                        Err(err) => self.chat_widget.add_error_message(format!(
                            "Failed to list GPT-Live voice profiles: {err:#}"
                        )),
                    },
                    RealtimeVoiceProfileCommand::Status => {
                        if let Some(profile) = &self.realtime_voice_profile {
                            let location = profile_file_path(codex_home, &profile.name)
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|_| "the Codex home".to_string());
                            self.chat_widget.add_info_message(
                                format!(
                                    "GPT-Live voice profile is '{}' on base voice `{}` (client-side; applies to the next voice session). Edit '{}' and its referenced effect preset to tune or share it.",
                                    profile.name,
                                    profile.voice.wire_name(),
                                    location,
                                ),
                                None,
                            );
                        } else {
                            self.chat_widget.add_info_message(
                            "GPT-Live voice profile is off. Use '/voice profile use jarvis' to try the built-in Arbor/Jarvis profile.".to_string(),
                            None,
                            );
                        }
                    }
                    RealtimeVoiceProfileCommand::Off => {
                        match deactivate_profile_and_preset(codex_home) {
                            Ok(()) => {
                                if let Some(session) = self.realtime_voice_session.as_ref()
                                    && let Err(err) = session.set_voice_effect(None)
                                {
                                    self.chat_widget.add_error_message(format!(
                                        "GPT-Live effects were persisted off, but the live session could not bypass them: {err:#}"
                                    ));
                                }
                                self.realtime_voice_profile = None;
                                self.realtime_voice_rotation_selected = false;
                                self.chat_widget.add_info_message(
                                "GPT-Live voice profile and its output effect are off. The current base voice remains selected.".to_string(),
                                None,
                            );
                            }
                            Err(err) => self.chat_widget.add_error_message(format!(
                                "Failed to disable GPT-Live voice profile: {err:#}"
                            )),
                        }
                    }
                    RealtimeVoiceProfileCommand::Use(name) => {
                        match activate_profile(codex_home, &name) {
                            Ok(profile) => {
                                let location = profile_file_path(codex_home, &name)
                                    .map(|path| path.display().to_string())
                                    .unwrap_or_else(|_| "the Codex home".to_string());
                                self.config.realtime.voice = Some(profile.voice);
                                self.realtime_voice_profile = Some(profile.clone());
                                self.realtime_voice_rotation_selected = false;
                                self.chat_widget.add_info_message(
                                    format!(
                                        "GPT-Live voice profile '{}' is active (base voice `{}`). It applies to the next voice session; edit '{location}' to tune or share it.",
                                        profile.name,
                                        profile.voice.wire_name()
                                    ),
                                    None,
                                );
                            }
                            Err(err) => self.chat_widget.add_error_message(format!(
                                "Failed to select GPT-Live voice profile '{name}': {err:#}"
                            )),
                        }
                    }
                }
            }
            RealtimeVoiceCommand::Effect(command) => {
                let codex_home = self.config.codex_home.as_path();
                match command {
                    RealtimeVoiceEffectCommand::List => {
                        let active = if let Some(profile) = &self.realtime_voice_profile {
                            Ok(Some(profile.effect.clone()))
                        } else if self.realtime_voice_rotation_selected {
                            Ok(None)
                        } else {
                            active_preset_name(codex_home)
                        };
                        match (list_preset_names(codex_home), active) {
                            (Ok(names), Ok(active)) => {
                                let active = active.as_deref().unwrap_or("off");
                                let names = names
                                    .iter()
                                    .map(|name| {
                                        if name == active {
                                            format!("{name} (selected)")
                                        } else {
                                            name.to_string()
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                self.chat_widget.add_info_message(
                                    format!(
                                        "GPT-Live output effects (selected: {active}): {names}"
                                    ),
                                    None,
                                );
                            }
                            (Err(err), _) | (_, Err(err)) => self.chat_widget.add_error_message(
                                format!("Failed to list GPT-Live voice effects: {err:#}"),
                            ),
                        }
                    }
                    RealtimeVoiceEffectCommand::Status => {
                        if let Some(profile) = &self.realtime_voice_profile {
                            let location = preset_file_path(codex_home, &profile.effect)
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|_| "the Codex home".to_string());
                            self.chat_widget.add_info_message(
                                format!(
                                    "GPT-Live output effect is '{}' through voice profile '{}' (client-side; applies to the next voice session). Edit '{location}' to tune it.",
                                    profile.effect,
                                    profile.name,
                                ),
                                None,
                            );
                        } else if self.realtime_voice_rotation_selected {
                            self.chat_widget.add_info_message(
                                "GPT-Live output effect is off for the rotation-selected base voice (client-side; applies to the next voice session). Use '/voice effect use jarvis' to override the rotation.".to_string(),
                                None,
                            );
                        } else {
                            match (active_preset_name(codex_home), load_active_preset(codex_home)) {
                                (Ok(active), Ok(Some(preset))) => {
                                    let name = active.unwrap_or(preset.name);
                                    let location = preset_file_path(codex_home, &name)
                                        .map(|path| path.display().to_string())
                                        .unwrap_or_else(|_| "the Codex home".to_string());
                                    self.chat_widget.add_info_message(
                                        format!(
                                            "GPT-Live output effect is '{name}' (client-side; applies to the next voice session). Edit '{location}' to tune it."
                                        ),
                                        None,
                                    );
                                }
                                (Ok(Some(_)), Ok(None)) | (Ok(None), Ok(None)) => self.chat_widget.add_info_message(
                                    "GPT-Live output effect is off (client-side; applies to the next voice session). Use '/voice effect use jarvis' to try the built-in profile.".to_string(),
                                    None,
                                ),
                                (Err(err), _) | (_, Err(err)) => self.chat_widget.add_error_message(format!(
                                    "Failed to read the active GPT-Live voice effect: {err:#}"
                                )),
                            }
                        }
                    }
                    RealtimeVoiceEffectCommand::Off => {
                        match deactivate_profile_and_preset(codex_home) {
                            Ok(()) => {
                                if let Some(session) = self.realtime_voice_session.as_ref()
                                    && let Err(err) = session.set_voice_effect(None)
                                {
                                    self.chat_widget.add_error_message(format!(
                                        "GPT-Live effects were persisted off, but the live session could not bypass them: {err:#}"
                                    ));
                                }
                                self.realtime_voice_profile = None;
                                self.realtime_voice_rotation_selected = false;
                                self.chat_widget.add_info_message(
                                "GPT-Live output effects are off for the current and next voice session."
                                    .to_string(),
                                None,
                            );
                            }
                            Err(err) => self.chat_widget.add_error_message(format!(
                                "Failed to disable GPT-Live voice effects: {err:#}"
                            )),
                        }
                    }
                    RealtimeVoiceEffectCommand::Use(name) => {
                        match activate_preset_and_deactivate_profile(codex_home, &name) {
                            Ok(preset) => {
                                if let Some(session) = self.realtime_voice_session.as_ref()
                                    && let Err(err) = session.set_voice_effect(Some(&preset))
                                {
                                    self.chat_widget.add_error_message(format!(
                                        "GPT-Live effect was persisted, but the live session could not update: {err:#}"
                                    ));
                                }
                                self.realtime_voice_profile = None;
                                self.realtime_voice_rotation_selected = false;
                                let location = preset_file_path(codex_home, &name)
                                    .map(|path| path.display().to_string())
                                    .unwrap_or_else(|_| "the Codex home".to_string());
                                self.chat_widget.add_info_message(
                                    format!(
                                        "GPT-Live output effect set to '{name}' for the current and next voice session; edit '{location}' to tune or share the preset."
                                    ),
                                    None,
                                );
                            }
                            Err(err) => self.chat_widget.add_error_message(format!(
                                "Failed to select GPT-Live voice effect '{name}': {err:#}"
                            )),
                        }
                    }
                }
            }
            RealtimeVoiceCommand::List => match app_server.thread_realtime_list_voices().await {
                Ok(voices) => {
                    let selected = realtime_v3_voice(self.config.realtime.voice);
                    let names = voices
                        .v1
                        .iter()
                        .map(|voice| {
                            if *voice == selected {
                                format!("{} (selected)", voice.wire_name())
                            } else {
                                voice.wire_name().to_string()
                            }
                        })
                        .collect::<Vec<_>>();
                    self.chat_widget.add_info_message(
                        format!(
                            "GPT-Live V3 voices (selected: {}):\n{}",
                            selected.wire_name(),
                            names.join("\n")
                        ),
                        None,
                    );
                }
                Err(err) => self
                    .chat_widget
                    .add_error_message(format!("Failed to list GPT-Live voices: {err:#}")),
            },
            RealtimeVoiceCommand::Set(voice) => {
                let voices = match app_server.thread_realtime_list_voices().await {
                    Ok(voices) => voices,
                    Err(err) => {
                        self.chat_widget.add_error_message(format!(
                            "Failed to validate GPT-Live voice `{}`: {err:#}",
                            voice.wire_name()
                        ));
                        tui.frame_requester().schedule_frame();
                        return;
                    }
                };
                if !voices.v1.contains(&voice) {
                    self.chat_widget.add_error_message(format!(
                        "GPT-Live V3 does not support voice `{}`. Use `/voice list`.",
                        voice.wire_name()
                    ));
                    tui.frame_requester().schedule_frame();
                    return;
                }
                match crate::config_update::write_config_batch(
                    app_server.request_handle(),
                    vec![crate::config_update::build_realtime_voice_edit(
                        voice.wire_name(),
                    )],
                )
                .await
                {
                    Ok(_) => {
                        if let Err(err) = deactivate_profile(self.config.codex_home.as_path()) {
                            self.chat_widget.add_error_message(format!(
                                "Realtime voice config was saved, but the active GPT-Live profile remains in control of the next session: {err:#}"
                            ));
                        } else {
                            self.config.realtime.voice = Some(voice);
                            self.realtime_voice_profile = None;
                            self.realtime_voice_rotation_selected = false;
                            self.chat_widget.add_info_message(
                                format!(
                                    "Realtime voice set to `{}`. It will apply to the next voice session.",
                                    voice.wire_name()
                                ),
                                None,
                            );
                        }
                    }
                    Err(err) => self.chat_widget.add_error_message(format!(
                        "Failed to save GPT-Live voice `{}`: {err:#}",
                        voice.wire_name()
                    )),
                }
            }
        }
        tui.frame_requester().schedule_frame();
    }

    pub(super) async fn stop_realtime_voice(&mut self, app_server: &mut AppServerSession) {
        let Some(session) = self.realtime_voice_session.take() else {
            return;
        };
        if let Some(thread_id) = self.active_thread_id.or(self.chat_widget.thread_id()) {
            let _ = app_server.thread_realtime_stop(thread_id).await;
        }
        session.close().await;
    }

    pub(super) fn handle_realtime_voice_notification(&mut self, notification: &ServerNotification) {
        match notification {
            ServerNotification::ThreadRealtimeStarted(_) => {
                if let Some(session) = &self.realtime_voice_session {
                    session.set_output_muted(false);
                }
                self.clear_realtime_debug_state();
            }
            ServerNotification::ThreadRealtimeItemAdded(notification)
                if !self.config.realtime.enable_preambles
                    && notification
                        .item
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        == Some("handoff_request") =>
            {
                if let Some(session) = &self.realtime_voice_session {
                    session.set_output_muted(true);
                }
            }
            ServerNotification::TurnCompleted(_) => {
                if let Some(session) = &self.realtime_voice_session {
                    session.set_output_muted(false);
                }
            }
            ServerNotification::ThreadRealtimeSdp(notification) => {
                if let Some(session) = &self.realtime_voice_session {
                    session.apply_remote_sdp(notification.sdp.clone());
                }
            }
            ServerNotification::ThreadRealtimeError(_)
            | ServerNotification::ThreadRealtimeClosed(_) => {
                if let Some(session) = &self.realtime_voice_session {
                    session.set_output_muted(false);
                }
                self.clear_realtime_debug_state();
                self.realtime_voice_session.take();
            }
            _ => {}
        }
    }

    fn clear_realtime_debug_state(&mut self) {
        self.realtime_handoff_debug_ids.clear();
        self.realtime_output_debug_item_id = None;
        self.realtime_output_debug_response_id = None;
        self.realtime_output_debug_handoff_id = None;
        self.realtime_output_debug_audio_chunk_count = 0;
        self.realtime_output_debug_transcript_delta_count = 0;
        self.realtime_output_debug_message_count = 0;
    }

    pub(super) fn realtime_handoff_debug_message(
        &mut self,
        notification: &codex_app_server_protocol::ThreadRealtimeItemAddedNotification,
    ) -> Option<String> {
        if !self.realtime_voice_debug {
            return None;
        }
        let item_type = notification
            .item
            .get("type")
            .and_then(serde_json::Value::as_str)?;
        if item_type != "handoff_request" {
            return None;
        }
        let handoff_id = notification
            .item
            .get("handoff_id")
            .and_then(serde_json::Value::as_str)
            .filter(|handoff_id| !handoff_id.is_empty());
        let item_id = notification
            .item
            .get("item_id")
            .and_then(serde_json::Value::as_str)
            .filter(|item_id| !item_id.is_empty());
        let debug_id = handoff_id.or(item_id);
        let identity = match handoff_id {
            Some(handoff_id) => format!(
                "handoff_id `{}`",
                realtime_handoff_debug_preview(handoff_id)
            ),
            None => match item_id {
                Some(item_id) => {
                    format!("item_id `{}`", realtime_handoff_debug_preview(item_id))
                }
                None => "handoff_id `<missing>`".to_string(),
            },
        };
        let input = notification
            .item
            .get("input_transcript")
            .and_then(serde_json::Value::as_str)
            .filter(|input| !input.trim().is_empty());
        if input.is_some() {
            let debug_key = debug_id.map(realtime_handoff_debug_id_key);
            if debug_key.is_some_and(|debug_key| {
                self.realtime_handoff_debug_ids
                    .iter()
                    .any(|seen_id| *seen_id == debug_key)
            }) {
                return None;
            }
            if let Some(debug_key) = debug_key {
                self.realtime_handoff_debug_ids.push_back(debug_key);
                if self.realtime_handoff_debug_ids.len() > REALTIME_HANDOFF_DEBUG_DEDUPE_CAPACITY {
                    self.realtime_handoff_debug_ids.pop_front();
                }
            }
        }
        self.clear_realtime_output_response_state();
        self.realtime_output_debug_handoff_id = handoff_id.map(realtime_handoff_debug_preview);
        let Some(input) = input else {
            return Some(format!(
                "GPT-Live handoff debug: {identity}; no handoff input was available; inherited the session effort."
            ));
        };
        if let Some(routing) = notification
            .item
            .get("routing")
            .and_then(serde_json::Value::as_object)
        {
            let classifier = routing
                .get("classifier")
                .and_then(serde_json::Value::as_object);
            let classifier_kind = classifier
                .and_then(|classifier| classifier.get("kind"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("text");
            let classifier_model = classifier
                .and_then(|classifier| classifier.get("model"))
                .and_then(serde_json::Value::as_str);
            let classifier_reasoning_effort = classifier
                .and_then(|classifier| classifier.get("reasoning_effort"))
                .and_then(serde_json::Value::as_str);
            let classifier_fallback = classifier
                .and_then(|classifier| classifier.get("fallback"))
                .and_then(serde_json::Value::as_str);
            let classification = routing
                .get("classification")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("substantive");
            let selected_effort = routing
                .get("selected_effort")
                .and_then(serde_json::Value::as_str);
            let session_effort = self.chat_widget.current_reasoning_effort();
            let selected = selected_effort.map(str::to_string).unwrap_or_else(|| {
                session_effort
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "session default".to_string())
            });
            let reason = if selected_effort.is_some() {
                "read-only override"
            } else {
                "inherited session effort"
            };
            let classifier_name = match (classifier_kind, classifier_model) {
                ("model", Some(model)) => format!("model `{model}`"),
                ("model", None) => "model `<unknown>`".to_string(),
                ("text", Some(model)) if classifier_fallback == Some("input_too_long") => {
                    format!("text (model `{model}` not attempted)")
                }
                ("text", Some(model)) => format!("text (fallback from model `{model}`)"),
                _ => "text".to_string(),
            };
            let classifier_reasoning = format!(
                "; reasoning `{}`",
                classifier_reasoning_effort.map_or_else(|| "default".to_string(), str::to_string)
            );
            let fallback = classifier_fallback
                .map(|fallback| format!("; fallback `{fallback}`"))
                .unwrap_or_default();
            let input_chars = input.chars().count();
            return Some(format!(
                "GPT-Live handoff debug: {identity}; classifier {classifier_name}{classifier_reasoning}{fallback}; classification `{classification}`; selected `{selected}` ({reason}); input_chars `{input_chars}`."
            ));
        }

        let configured_effort = self
            .config
            .realtime
            .non_substantive_reasoning_effort
            .as_ref();
        let session_effort = self.chat_widget.current_reasoning_effort();
        let (selected, reason) = if let Some(effort) =
            codex_protocol::realtime_handoff::configured_read_only_effort(input, configured_effort)
        {
            (effort.to_string(), "read-only override")
        } else {
            (
                session_effort
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "session default".to_string()),
                "inherited session effort",
            )
        };
        let input_chars = input.chars().count();
        Some(format!(
            "GPT-Live handoff debug: {identity}; selected `{selected}` ({reason}); input_chars `{input_chars}`."
        ))
    }

    pub(super) fn realtime_output_item_debug_message(
        &mut self,
        notification: &codex_app_server_protocol::ThreadRealtimeItemAddedNotification,
    ) -> Option<String> {
        if !self.realtime_voice_debug {
            return None;
        }
        let item = &notification.item;
        let item_type = item.get("type").and_then(serde_json::Value::as_str)?;
        if item_type == "handoff_request" || item_type.starts_with("input_") {
            return None;
        }
        let is_response_lifecycle = matches!(
            item_type,
            "response.created" | "response.cancelled" | "response.done"
        );
        let role = item.get("role").and_then(serde_json::Value::as_str);
        if !is_response_lifecycle && role != Some("assistant") {
            return None;
        }

        let source = if is_response_lifecycle {
            "server response lifecycle"
        } else {
            "server item"
        };
        let item_id = item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(realtime_handoff_debug_preview);
        let response_id = item
            .get("response_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(realtime_handoff_debug_preview)
            .or_else(|| {
                item.get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(realtime_handoff_debug_preview)
            });
        let item_type_preview = realtime_handoff_debug_preview(item_type);
        let role = role.map_or_else(|| "<missing>".to_string(), realtime_handoff_debug_preview);
        let phase = item
            .get("phase")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "<missing>".to_string(), realtime_handoff_debug_preview);
        let text = if is_response_lifecycle {
            "<none>".to_string()
        } else {
            realtime_item_text_preview(item).unwrap_or_else(|| "<none>".to_string())
        };
        // Transcript/audio notifications do not carry a handoff ID. This is only the current
        // handoff context, so label it as such rather than implying event-level provenance.
        let handoff_context_id = self
            .realtime_output_debug_handoff_id
            .as_deref()
            .unwrap_or("<missing>");
        let message = format!(
            "GPT-Live output debug: source `{source}`; type `{item_type_preview}`; item_id `{}`; response_id `{}`; handoff_context_id `{handoff_context_id}`; phase `{phase}`; role `{role}`; text `{text}`.",
            item_id.as_deref().unwrap_or("<missing>"),
            response_id.as_deref().unwrap_or("<missing>"),
        );
        if item_type == "response.created" {
            self.realtime_output_debug_response_id = response_id;
            self.realtime_output_debug_item_id = None;
            self.realtime_output_debug_audio_chunk_count = 0;
            self.realtime_output_debug_transcript_delta_count = 0;
        } else if item_type == "response.cancelled" {
            self.clear_realtime_output_response_state();
            self.realtime_output_debug_handoff_id = None;
        } else if item_type == "response.done" {
            self.clear_realtime_output_response_state();
        } else {
            let is_new_item = item_id.as_deref() != self.realtime_output_debug_item_id.as_deref();
            self.realtime_output_debug_item_id = item_id;
            if response_id.is_some() {
                self.realtime_output_debug_response_id = response_id;
            }
            if is_new_item {
                self.realtime_output_debug_audio_chunk_count = 0;
                self.realtime_output_debug_transcript_delta_count = 0;
            }
        }

        Some(message)
    }

    pub(super) fn realtime_output_transcript_debug_message(
        &mut self,
        notification: &codex_app_server_protocol::ThreadRealtimeTranscriptDoneNotification,
    ) -> Option<String> {
        if !self.realtime_voice_debug || notification.role != "assistant" {
            return None;
        }
        let item_id = "<missing>";
        let response_id = "<missing>";
        let handoff_context_id = self
            .realtime_output_debug_handoff_id
            .as_deref()
            .unwrap_or("<missing>");
        let delta_count = self.realtime_output_debug_transcript_delta_count;
        let text = realtime_handoff_debug_preview(&notification.text);
        let message = format!(
            "GPT-Live output debug: source `assistant transcript`; item_id `{item_id}`; response_id `{response_id}`; handoff_context_id `{handoff_context_id}`; delta_count `{delta_count}`; text `{text}`."
        );
        self.clear_realtime_output_response_state();
        Some(message)
    }

    pub(super) fn realtime_main_agent_output_debug_message(
        &self,
        notification: &ServerNotification,
    ) -> Option<String> {
        if !self.realtime_voice_debug {
            return None;
        }
        // Main-agent notifications do not carry the originating realtime handoff ID either; this
        // value is the current context used to correlate the notification in the TUI.
        let handoff_context_id = self.realtime_output_debug_handoff_id.as_deref()?;
        let (source, item_id, phase, text) = match notification {
            ServerNotification::ItemStarted(notification) => match &notification.item {
                ThreadItem::AgentMessage {
                    id, phase, text, ..
                } => (
                    "main agent item started",
                    id.as_str(),
                    phase.as_ref(),
                    text.as_str(),
                ),
                _ => return None,
            },
            ServerNotification::ItemCompleted(notification) => match &notification.item {
                ThreadItem::AgentMessage {
                    id, phase, text, ..
                } => (
                    "main agent item completed",
                    id.as_str(),
                    phase.as_ref(),
                    text.as_str(),
                ),
                _ => return None,
            },
            ServerNotification::AgentMessageDelta(notification) => (
                "main agent item delta",
                notification.item_id.as_str(),
                None,
                notification.delta.as_str(),
            ),
            _ => return None,
        };
        let phase = match phase {
            Some(MessagePhase::Commentary) => "commentary",
            Some(MessagePhase::FinalAnswer) => "final_answer",
            None => "<missing>",
        };
        let text = if text.trim().is_empty() {
            "<none>".to_string()
        } else {
            realtime_handoff_debug_preview(text)
        };
        Some(format!(
            "GPT-Live output debug: source `{source}`; item_id `{}`; response_id `{}`; handoff_context_id `{}`; phase `{phase}`; text `{}`.",
            realtime_handoff_debug_preview(item_id),
            self.realtime_output_debug_response_id
                .as_deref()
                .unwrap_or("<missing>"),
            realtime_handoff_debug_preview(handoff_context_id),
            text,
        ))
    }

    pub(super) fn realtime_output_transcript_delta_debug_message(
        &mut self,
        notification: &codex_app_server_protocol::ThreadRealtimeTranscriptDeltaNotification,
    ) -> Option<String> {
        if !self.realtime_voice_debug
            || notification.role != "assistant"
            || notification.delta.trim().is_empty()
            || self.realtime_output_debug_transcript_delta_count
                >= REALTIME_OUTPUT_DEBUG_TRANSCRIPT_DELTA_LIMIT
        {
            return None;
        }
        self.realtime_output_debug_transcript_delta_count += 1;
        None
    }

    pub(super) fn realtime_output_audio_debug_message(
        &mut self,
        notification: &codex_app_server_protocol::ThreadRealtimeOutputAudioDeltaNotification,
    ) -> Option<String> {
        if !self.realtime_voice_debug {
            return None;
        }
        let item_id = notification
            .audio
            .item_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .map_or_else(|| "<missing>".to_string(), realtime_handoff_debug_preview);
        let is_new_item = item_id != "<missing>"
            && Some(item_id.as_str()) != self.realtime_output_debug_item_id.as_deref();
        if item_id != "<missing>" {
            self.realtime_output_debug_item_id = Some(item_id.clone());
        }
        if is_new_item {
            self.realtime_output_debug_audio_chunk_count = 0;
            self.realtime_output_debug_transcript_delta_count = 0;
        }
        if self.realtime_output_debug_audio_chunk_count >= REALTIME_OUTPUT_DEBUG_AUDIO_CHUNK_LIMIT {
            return None;
        }
        self.realtime_output_debug_audio_chunk_count += 1;
        Some(format!(
            "GPT-Live output debug: source `audio`; chunk {}; item_id `{item_id}`; response_id `{}`; handoff_context_id `{}`; sample_rate {}; channels {}; samples_per_channel `{}`.",
            self.realtime_output_debug_audio_chunk_count,
            self.realtime_output_debug_response_id
                .as_deref()
                .unwrap_or("<missing>"),
            self.realtime_output_debug_handoff_id
                .as_deref()
                .unwrap_or("<missing>"),
            notification.audio.sample_rate,
            notification.audio.num_channels,
            notification
                .audio
                .samples_per_channel
                .map_or_else(|| "<missing>".to_string(), |value| value.to_string()),
        ))
    }

    fn clear_realtime_output_response_state(&mut self) {
        self.realtime_output_debug_item_id = None;
        self.realtime_output_debug_response_id = None;
        self.realtime_output_debug_audio_chunk_count = 0;
        self.realtime_output_debug_transcript_delta_count = 0;
    }

    pub(super) fn should_handle_backtrack_esc(&self, key_event: KeyEvent) -> bool {
        !self.chat_widget.side_conversation_active()
            && self.chat_widget.is_normal_backtrack_mode()
            && self.chat_widget.composer_is_empty()
            && !self.chat_widget.should_handle_vim_insert_escape(key_event)
    }

    pub(super) fn should_reject_side_backtrack_esc(&self, key_event: KeyEvent) -> bool {
        self.chat_widget.side_conversation_active()
            && self.chat_widget.is_normal_backtrack_mode()
            && self.chat_widget.composer_is_empty()
            && !self.chat_widget.should_handle_vim_insert_escape(key_event)
    }

    pub(super) fn reject_side_backtrack_esc(&mut self) {
        self.reset_backtrack_state();
        self.chat_widget
            .add_error_message(SIDE_EDIT_PREVIOUS_UNAVAILABLE_MESSAGE.to_string());
    }

    fn app_keymap_shortcuts_available(&self) -> bool {
        self.overlay.is_none() && self.chat_widget.no_modal_or_popup_active()
    }

    pub(super) fn refresh_status_line(&mut self) {
        self.chat_widget.refresh_status_line();
    }
}

fn realtime_item_text_preview(item: &serde_json::Value) -> Option<String> {
    let direct_text = item
        .get("text")
        .and_then(serde_json::Value::as_str)
        .or_else(|| item.get("transcript").and_then(serde_json::Value::as_str));
    if let Some(direct_text) = direct_text {
        return Some(realtime_handoff_debug_preview(direct_text));
    }

    let content = item.get("content").and_then(serde_json::Value::as_array)?;
    let mut text = String::new();
    let mut character_count = 0;
    let mut truncated = false;
    for part in content {
        let Some(part_text) = part
            .get("text")
            .and_then(serde_json::Value::as_str)
            .or_else(|| part.get("transcript").and_then(serde_json::Value::as_str))
        else {
            continue;
        };
        if !text.is_empty() {
            if character_count == REALTIME_HANDOFF_DEBUG_VALUE_LIMIT {
                truncated = true;
                break;
            }
            text.push(' ');
            character_count += 1;
        }
        for character in part_text.chars() {
            if character_count == REALTIME_HANDOFF_DEBUG_VALUE_LIMIT {
                truncated = true;
                break;
            }
            text.push(character);
            character_count += 1;
        }
        if truncated {
            break;
        }
    }
    if text.is_empty() {
        None
    } else if truncated {
        Some(format!("{text}…"))
    } else {
        Some(realtime_handoff_debug_preview(&text))
    }
}

fn is_realtime_voice_key(spec: Option<&str>, key_event: KeyEvent) -> bool {
    realtime_hotkey_matches(spec, key_event)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::make_test_app;
    use super::RealtimeDevicePicker;
    use super::is_realtime_voice_key;
    use crate::chatwidget::tests::helpers::render_bottom_popup;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyEventKind;
    use crossterm::event::KeyModifiers;
    use crossterm::event::ModifierKeyCode;

    #[tokio::test]
    async fn app_keymap_shortcuts_are_disabled_while_keymap_view_is_active() {
        let mut app = make_test_app().await;
        assert!(app.app_keymap_shortcuts_available());

        let keymap = app.keymap.clone();
        app.chat_widget.open_keymap_debug(&keymap);

        assert!(!app.app_keymap_shortcuts_available());
    }

    #[test]
    fn realtime_voice_uses_right_alt_modifier_key() {
        let right_alt = KeyEvent {
            code: KeyCode::Modifier(ModifierKeyCode::RightAlt),
            kind: KeyEventKind::Press,
            ..KeyEvent::new(KeyCode::Null, KeyModifiers::NONE)
        };
        let character = KeyEvent {
            kind: KeyEventKind::Press,
            ..KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)
        };

        assert!(is_realtime_voice_key(None, right_alt));
        assert!(!is_realtime_voice_key(None, character));
        assert!(is_realtime_voice_key(
            Some("f13"),
            KeyEvent::new(KeyCode::F(13), KeyModifiers::NONE)
        ));
    }

    #[tokio::test]
    async fn realtime_microphone_picker_renders_current_device() {
        let mut app = make_test_app().await;
        app.show_realtime_device_picker(
            RealtimeDevicePicker::Microphone,
            vec!["Built-in Microphone".to_string(), "Clip-On Mic".to_string()],
            Some("Clip-On Mic"),
        );

        let rendered = render_bottom_popup(&app.chat_widget, /*width*/ 80);
        insta::with_settings!({snapshot_path => "../snapshots"}, {
            insta::assert_snapshot!("realtime_microphone_picker", rendered);
        });
    }
}
