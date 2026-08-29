//! Slash-command dispatch and local-recall handoff for `ChatWidget`.
//!
//! `ChatComposer` parses slash input and stages recognized command text for local
//! Up-arrow recall before returning an input result. This module owns the app-level
//! dispatch step and records the staged entry once the command has been handled, so
//! slash-command recall follows the same submitted-input rule as ordinary text.

use super::*;
use crate::app_event::ThreadGoalSetMode;
use crate::bottom_pane::prompt_args::parse_slash_name;
use crate::bottom_pane::slash_commands::BuiltinCommandFlags;
use crate::bottom_pane::slash_commands::ServiceTierCommand;
use crate::bottom_pane::slash_commands::SlashCommandItem;
use crate::bottom_pane::slash_commands::find_slash_command;
use crate::goal_display::GOAL_USAGE;
use crate::goal_files::GoalDraft;
use crate::realtime_voice::RealtimeMicCommand;
use crate::realtime_voice::RealtimeVoiceCommand;
use crate::realtime_voice::RealtimeVoiceDebugCommand;
use crate::realtime_voice::RealtimeVoiceEffectCommand;
use crate::realtime_voice::RealtimeVoiceProfileCommand;
use crate::realtime_voice::realtime_voice_from_name;
use crate::realtime_voice_effects::load_active_preset;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlashCommandDispatchSource {
    Live,
    Queued,
}

struct PreparedSlashCommandArgs {
    args: String,
    text_elements: Vec<TextElement>,
    pending_pastes: Vec<(String, String)>,
    local_images: Vec<LocalImageAttachment>,
    remote_image_urls: Vec<String>,
    mention_bindings: Vec<MentionBinding>,
    source: SlashCommandDispatchSource,
}

const SIDE_STARTING_CONTEXT_LABEL: &str = "Side starting...";
const SIDE_SLASH_COMMAND_UNAVAILABLE_HINT: &str =
    "Press Ctrl+C to return to the main thread first.";
const GOAL_USAGE_HINT: &str = "Example: /goal improve benchmark coverage";
const CONTINUOUS_USAGE: &str = "Usage: /continuous [on|off|status]";
const OUTCOMES_USAGE: &str = "Usage: /outcomes [on|off|status|report]";
const SPEND_USAGE: &str = "Usage: /spend [days|YYYY-MM|YYYY-MM-DD..YYYY-MM-DD]";
const SESSION_TMP_REAP_USAGE: &str = "Usage: /tmp reap [days]";
const SCRATCHPAD_ABSORB_USAGE: &str = "Usage: /scratchpad-absorb <scratchpad_id> [--exclude-pending] [--exclude-blocked] [--exclude-notes] [--exclude-outcomes] [--exclude-delegations] [--exclude-artifacts] [--exclude-worktrees] [--exclude-completed] [--exclude-next-steps] [--exclude-git-refs]";
const RAW_USAGE: &str = "Usage: /raw [on|off]";
const MIC_USAGE: &str = "Usage: /mic [help|on|off|status|hot|push|hotkey|change|devices|aliases|alias <name> [device]|device <name>|speakers|speaker change|speaker aliases|speaker alias <name> [device]|speaker <name>]";
const VOICE_USAGE: &str = "Usage: /voice [help|on|off|status|debug [on|off|status]|list|history [count]|calibrate <audio-path>|effect [list|status|off|use <name>]|profile [list|status|off|use <name>]|tune|<voice>]";
const VOICE_HISTORY_USAGE: &str = "Usage: /voice history [count] (count: 1-20)";
const VOICE_CALIBRATION_USAGE: &str = "Usage: /voice calibrate <audio-path> (wav, mp3, ogg/vorbis, mp4/m4a, or a supported audio track in a video file; max 50 MB)";
const VOICE_EFFECT_USAGE: &str = "Usage: /voice effect [list|status|off|use <name>]";
const VOICE_PROFILE_USAGE: &str = "Usage: /voice profile [list|status|off|use <name>]";
const USAGE_CHATGPT_LOGIN_REQUIRED: &str = "Sign in with ChatGPT to use /usage.";

fn realtime_alias_args(value: &str) -> Option<(String, Option<String>)> {
    let mut parts = value.trim().splitn(2, char::is_whitespace);
    let alias = parts.next()?.trim();
    if alias.is_empty() || alias.eq_ignore_ascii_case("help") || alias == "?" {
        return None;
    }
    let device = parts
        .next()
        .map(str::trim)
        .filter(|device| !device.is_empty())
        .map(str::to_string);
    Some((alias.to_string(), device))
}

fn realtime_calibration_path(value: &str) -> Option<PathBuf> {
    let (_, path) = value.split_once(char::is_whitespace)?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    if path.len() < 2 {
        return (!matches!(path.as_bytes().first(), Some(b'\'' | b'"')))
            .then(|| PathBuf::from(path));
    }
    let quoted = path
        .as_bytes()
        .first()
        .copied()
        .zip(path.as_bytes().last().copied());
    match quoted {
        Some((first, last)) if first == last && (first == b'\'' || first == b'"') => {
            Some(PathBuf::from(&path[1..path.len() - 1]))
        }
        Some((b'\'' | b'"', _)) | Some((_, b'\'' | b'"')) => None,
        _ => Some(PathBuf::from(path)),
    }
}

fn scratchpad_update_event_from_value(value: &serde_json::Value) -> Option<ScratchpadUpdateEvent> {
    Some(ScratchpadUpdateEvent {
        scratchpad_id: value.get("scratchpad_id")?.as_str()?.to_string(),
        objective: value
            .get("objective")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        status: value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        continuous_enabled: value
            .get("run_policy")
            .and_then(|policy| policy.get("continuous"))
            .and_then(|continuous| continuous.get("enabled"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        completed: string_array_value(value.get("completed")),
        next_steps: string_array_value(value.get("next_steps")),
        pending_waits: value
            .get("pending_waits")
            .and_then(serde_json::Value::as_array)
            .map(|waits| waits.iter().map(format_pending_wait).collect())
            .unwrap_or_default(),
        blocked: value
            .get("blocked")
            .and_then(serde_json::Value::as_array)
            .map(|blocked| blocked.iter().map(format_blocked_item).collect())
            .unwrap_or_default(),
        updated_at: value
            .get("updated_at")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        archived_at: value
            .get("archived_at")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
    })
}

fn scratchpad_value_matches_thread(value: &serde_json::Value, thread_id: &ThreadId) -> bool {
    let thread_id = thread_id.to_string();
    if value
        .get("scratchpad_id")
        .and_then(serde_json::Value::as_str)
        != Some(thread_id.as_str())
    {
        return false;
    }
    if value
        .get("origin_thread_id")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|origin_thread_id| origin_thread_id != thread_id)
    {
        return false;
    }
    true
}

fn string_array_value(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn format_pending_wait(wait: &serde_json::Value) -> String {
    if let Some(text) = wait.as_str() {
        return text.to_string();
    }
    let Some(object) = wait.as_object() else {
        return wait.to_string();
    };
    let title = [
        "summary",
        "description",
        "reason",
        "target",
        "wait_id",
        "id",
        "next_check_at",
    ]
    .iter()
    .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
    .unwrap_or("pending wait");

    let mut details = Vec::new();
    for key in [
        "id",
        "status",
        "owner",
        "wait_type",
        "target",
        "check_method",
        "next_check_at",
        "reuse_session_id",
        "details",
    ] {
        if let Some(value) = object.get(key).and_then(serde_json::Value::as_str)
            && value != title
        {
            details.push(format!("{key}: {value}"));
        }
    }

    if details.is_empty() {
        title.to_string()
    } else {
        format!("{title} ({})", details.join("; "))
    }
}

fn format_blocked_item(blocked: &serde_json::Value) -> String {
    if let Some(text) = blocked.as_str() {
        return text.to_string();
    }
    let Some(object) = blocked.as_object() else {
        return blocked.to_string();
    };
    [
        "summary",
        "blocked_on",
        "required_user_action",
        "blocker_id",
        "reason",
    ]
    .iter()
    .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
    .unwrap_or("blocked item")
    .to_string()
}

impl ChatWidget {
    /// Dispatch a bare slash command and record its staged local-history entry.
    ///
    /// The composer stages history before returning `InputResult::Command`; this wrapper commits
    /// that staged entry after dispatch so slash-command recall follows the same "submitted input"
    /// rule as normal text.
    pub(super) fn handle_slash_command_dispatch(&mut self, cmd: SlashCommand) {
        self.dispatch_command(cmd);
        if cmd == SlashCommand::Goal {
            self.bottom_pane.drain_pending_submission_state();
        }
        self.bottom_pane.record_pending_slash_command_history();
    }

    pub(super) fn handle_service_tier_command_dispatch(&mut self, command: ServiceTierCommand) {
        if self.active_side_conversation {
            self.add_error_message(format!(
                "'/{}' is unavailable in side conversations. {SIDE_SLASH_COMMAND_UNAVAILABLE_HINT}",
                command.name
            ));
            self.bottom_pane.drain_pending_submission_state();
            self.bottom_pane.record_pending_slash_command_history();
            return;
        }
        self.toggle_service_tier_from_ui(command);
        self.bottom_pane.record_pending_slash_command_history();
    }

    /// Dispatch an inline slash command and record its staged local-history entry.
    ///
    /// Inline command arguments may later be prepared through the normal submission pipeline, but
    /// local command recall still tracks the original command invocation. Treating this wrapper as
    /// the only input-result entry point avoids double-recording commands with inline args.
    pub(super) fn handle_slash_command_with_args_dispatch(
        &mut self,
        cmd: SlashCommand,
        args: String,
        text_elements: Vec<TextElement>,
    ) {
        self.dispatch_command_with_args(cmd, args, text_elements);
        self.bottom_pane.record_pending_slash_command_history();
    }

    fn apply_plan_slash_command(&mut self) -> bool {
        if !self.collaboration_modes_enabled() {
            self.add_info_message(
                "Collaboration modes are disabled.".to_string(),
                Some("Enable collaboration modes to use /plan.".to_string()),
            );
            return false;
        }
        if let Some(mask) = collaboration_modes::plan_mask_with_config(
            self.model_catalog.as_ref(),
            self.config.collaboration_modes_config(),
        ) {
            self.set_collaboration_mask(mask);
            true
        } else {
            self.add_info_message(
                "Plan mode unavailable right now.".to_string(),
                /*hint*/ None,
            );
            false
        }
    }

    fn request_side_conversation(
        &mut self,
        parent_thread_id: ThreadId,
        user_message: Option<UserMessage>,
    ) {
        self.set_side_conversation_context_label(Some(SIDE_STARTING_CONTEXT_LABEL.to_string()));
        self.request_redraw();
        self.app_event_tx.send(AppEvent::StartSide {
            parent_thread_id,
            user_message,
        });
    }

    fn request_empty_side_conversation(&mut self, cmd: SlashCommand) {
        let Some(parent_thread_id) = self.thread_id else {
            self.add_error_message(format!(
                "'/{}' is unavailable before the session starts.",
                cmd.command()
            ));
            return;
        };

        self.request_side_conversation(parent_thread_id, /*user_message*/ None);
    }

    fn add_current_scratchpad_output(&mut self) {
        let Some(thread_id) = self.thread_id else {
            self.add_error_message(
                "'/scratchpad' is unavailable before the session starts.".to_string(),
            );
            return;
        };

        let scratchpad_id = thread_id.to_string();
        let path = self
            .config
            .codex_home
            .join("scratchpad")
            .join("entries")
            .join(format!("{scratchpad_id}.json"));
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.add_info_message(
                    format!(
                        "No built-in scratchpad exists for this session yet (id: {scratchpad_id})."
                    ),
                    Some(
                        "Ask Codex to open/update the scratchpad, or use the built-in scratchpad tools during ongoing work."
                            .to_string(),
                    ),
                );
                return;
            }
            Err(err) => {
                self.add_error_message(format!(
                    "Could not read built-in scratchpad `{scratchpad_id}`: {err}"
                ));
                return;
            }
        };
        let value = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => value,
            Err(err) => {
                self.add_error_message(format!(
                    "Built-in scratchpad `{scratchpad_id}` is invalid JSON: {err}"
                ));
                return;
            }
        };
        if !scratchpad_value_matches_thread(&value, &thread_id) {
            self.add_error_message(format!(
                "Built-in scratchpad `{scratchpad_id}` is owned by another thread and cannot be displayed."
            ));
            return;
        }
        let Some(update) = scratchpad_update_event_from_value(&value) else {
            self.add_error_message(format!(
                "Built-in scratchpad `{scratchpad_id}` is missing required fields."
            ));
            return;
        };
        self.on_scratchpad_update_verbose(update);
    }

    fn add_spend_output(&mut self, args: &str) {
        match self.daily_spend.render_report(args) {
            Ok(lines) => self.add_plain_history_lines(lines),
            Err(err) => self.add_error_message(format!("{SPEND_USAGE}\n{err}")),
        }
    }

    fn dispatch_scratchpad_absorb_command(&mut self, args: &str) {
        let Some(thread_id) = self.thread_id else {
            self.add_error_message(
                "'/scratchpad-absorb' is unavailable before the session starts.".to_string(),
            );
            return;
        };
        let (source_scratchpad_id, options) = match parse_scratchpad_absorb_args(args) {
            Ok(parsed) => parsed,
            Err(err) => {
                self.add_error_message(err);
                return;
            }
        };
        match crate::legacy_core::absorb_thread_scratchpad_context(
            &self.config.codex_home,
            &thread_id.to_string(),
            &source_scratchpad_id,
            options,
        ) {
            Ok(result) => {
                self.add_info_message(
                    format!(
                        "Absorbed scratchpad `{}` into current session scratchpad `{}`.",
                        result.source_scratchpad_id, result.target_scratchpad_id
                    ),
                    Some(format!(
                        "Imported as contextual history only: {}. Live control fields stayed excluded.",
                        format_absorb_counts(&result.counts)
                    )),
                );
            }
            Err(err) => {
                self.add_error_message(format!("Could not absorb scratchpad: {err}"));
            }
        }
    }

    fn unarchive_current_scratchpad(&mut self) {
        let Some(thread_id) = self.thread_id else {
            self.add_error_message(
                "'/scratchpad-unarchive' is unavailable before the session starts.".to_string(),
            );
            return;
        };
        match crate::legacy_core::unarchive_thread_scratchpad(
            &self.config.codex_home,
            &thread_id.to_string(),
        ) {
            Ok(()) => {
                self.add_info_message(
                    format!("Scratchpad `{thread_id}` unarchived."),
                    Some("It remains owned by this session and is no longer deletion-eligible as an archived pad.".to_string()),
                );
            }
            Err(err) => {
                self.add_error_message(format!(
                    "Could not unarchive scratchpad `{thread_id}`: {err}"
                ));
            }
        }
    }

    fn read_current_outcomes(&mut self) -> Option<(ThreadId, String, Vec<serde_json::Value>)> {
        let Some((thread_id, path)) = self.current_scratchpad_path() else {
            self.add_error_message(
                "'/outcomes' is unavailable before the session starts.".to_string(),
            );
            return None;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.add_info_message(
                    format!("No outcomes are available because scratchpad `{thread_id}` does not exist yet."),
                    Some("Record outcomes with the built-in scratchpad record_outcome tool during measurable work.".to_string()),
                );
                return None;
            }
            Err(err) => {
                self.add_error_message(format!(
                    "Could not read built-in scratchpad `{thread_id}`: {err}"
                ));
                return None;
            }
        };
        let value = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => value,
            Err(err) => {
                self.add_error_message(format!(
                    "Built-in scratchpad `{thread_id}` is invalid JSON: {err}"
                ));
                return None;
            }
        };
        if !scratchpad_value_matches_thread(&value, &thread_id) {
            self.add_error_message(format!(
                "Built-in scratchpad `{thread_id}` is owned by another thread and cannot be exported."
            ));
            return None;
        }
        let objective = value
            .get("objective")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Scratchpad outcomes")
            .to_string();
        let outcomes = value
            .get("outcomes")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        Some((thread_id, objective, outcomes))
    }

    fn add_current_outcomes_output(&mut self) {
        let Some((thread_id, objective, outcomes)) = self.read_current_outcomes() else {
            return;
        };
        let markdown = crate::outcomes_report::markdown(thread_id, &objective, &outcomes);
        self.add_to_history(history_cell::new_outcomes_export(markdown));
    }

    fn write_current_outcomes_report(&mut self) {
        let Some((thread_id, objective, outcomes)) = self.read_current_outcomes() else {
            return;
        };
        match crate::outcomes_report::write_html_report(
            &self.config.codex_home,
            thread_id,
            &objective,
            &outcomes,
        ) {
            Ok(path) => {
                self.add_info_message(
                    format!("Outcomes report written to {}", path.display()),
                    Some(
                        "The report is a static local HTML file with embedded SVG charts."
                            .to_string(),
                    ),
                );
            }
            Err(err) => {
                self.add_error_message(format!("Could not write outcomes report: {err}"));
            }
        }
    }

    fn set_outcomes_tracking_enabled(&mut self, enabled: bool) {
        match persist_outcomes_tracking_enabled(&self.config.codex_home, enabled) {
            Ok(()) => {
                self.config.scratchpad.outcomes_enabled = enabled;
                self.submit_op(AppCommand::ReloadUserConfig);
                let state = if enabled { "enabled" } else { "disabled" };
                self.add_info_message(
                    format!("Scratchpad outcome tracking {state}."),
                    Some("Persisted in config.toml for future sessions.".to_string()),
                );
            }
            Err(err) => {
                self.add_error_message(format!(
                    "Could not update scratchpad outcome tracking config: {err}"
                ));
            }
        }
    }

    fn add_outcomes_tracking_status(&mut self) {
        let state = if self.config.scratchpad.outcomes_enabled {
            "enabled"
        } else {
            "disabled"
        };
        self.add_info_message(
            format!("Scratchpad outcome tracking is {state}."),
            Some(OUTCOMES_USAGE.to_string()),
        );
    }

    fn dispatch_session_tmp_command(&mut self, args: &str) {
        let Some(thread_id) = self.thread_id else {
            self.add_error_message("'/tmp' is unavailable before the session starts.".to_string());
            return;
        };
        let config = crate::chatwidget::session_tmp_command::config(&self.config);
        let manager = match codex_session_tmp::SessionTmpManager::open_for_user(
            &config,
            self.config.codex_home.as_path(),
            &thread_id.to_string(),
            &thread_id.to_string(),
        ) {
            Ok(Some(manager)) => manager,
            Ok(None) => {
                self.add_info_message(
                    "Session temporary storage is disabled. Add `[session_tmp].enabled = true` to config.toml to enable it.".to_string(),
                    Some(crate::chatwidget::session_tmp_command::USAGE.to_string()),
                );
                return;
            }
            Err(error) => {
                self.add_error_message(format!(
                    "Could not open managed session temporary storage: {error}"
                ));
                return;
            }
        };
        match args.split_whitespace().collect::<Vec<_>>().as_slice() {
            [] | ["status"] | ["list"] => match manager.list() {
                Ok(listing) => self.add_info_message(
                    crate::chatwidget::session_tmp_command::status_message(&listing),
                    Some(crate::chatwidget::session_tmp_command::USAGE.to_string()),
                ),
                Err(error) => {
                    self.add_error_message(format!("Could not list session temporary files: {error}"));
                }
            },
            ["clean"] => match manager.clean() {
                Ok(report) => self.add_info_message(
                    crate::chatwidget::session_tmp_command::cleanup_message("cleanup", &report),
                    Some("Session-retained and expired entries were removed; manual entries were preserved.".to_string()),
                ),
                Err(error) => {
                    self.add_error_message(format!("Could not clean session temporary files: {error}"));
                }
            },
            ["clear"] => match manager.clear() {
                Ok(report) => self.add_info_message(
                    crate::chatwidget::session_tmp_command::cleanup_message("clear", &report),
                    Some("All entries in the current session were removed, including manual-retention entries.".to_string()),
                ),
                Err(error) => {
                    self.add_error_message(format!("Could not clear session temporary files: {error}"));
                }
            },
            ["reap"] => self.reap_session_tmp(&manager, config.stale_after),
            ["reap", days] => match days.parse::<u64>() {
                Ok(days) => self.reap_session_tmp(
                    &manager,
                    Duration::from_secs(days.saturating_mul(24 * 60 * 60)),
                ),
                Err(_) => self.add_error_message(SESSION_TMP_REAP_USAGE.to_string()),
            },
            _ => self.add_error_message(crate::chatwidget::session_tmp_command::USAGE.to_string()),
        }
    }

    fn reap_session_tmp(
        &mut self,
        manager: &codex_session_tmp::SessionTmpManager,
        max_age: Duration,
    ) {
        match manager.reap(max_age) {
            Ok(report) => self.add_info_message(
                crate::chatwidget::session_tmp_command::cleanup_message("stale-session reap", &report),
                Some("Only sessions older than the selected age were force-removed; the current session was protected.".to_string()),
            ),
            Err(error) => {
                self.add_error_message(format!("Could not reap stale session temporary files: {error}"));
            }
        }
    }

    fn dispatch_outcomes_command(&mut self, args: Option<&str>) {
        match args.map(str::trim).filter(|value| !value.is_empty()) {
            Some("on") => self.set_outcomes_tracking_enabled(/*enabled*/ true),
            Some("off") => self.set_outcomes_tracking_enabled(/*enabled*/ false),
            Some("status") => self.add_outcomes_tracking_status(),
            Some("report") => self.write_current_outcomes_report(),
            Some(_) => self.add_error_message(OUTCOMES_USAGE.to_string()),
            None => self.add_current_outcomes_output(),
        }
    }

    fn current_scratchpad_path(&self) -> Option<(ThreadId, PathBuf)> {
        let thread_id = self.thread_id?;
        Some((
            thread_id,
            self.config
                .codex_home
                .join("scratchpad")
                .join("entries")
                .join(format!("{thread_id}.json"))
                .to_path_buf(),
        ))
    }

    fn read_current_scratchpad_value(&mut self) -> Option<serde_json::Value> {
        let Some((thread_id, path)) = self.current_scratchpad_path() else {
            self.add_error_message(
                "'/continuous' is unavailable before the session starts.".to_string(),
            );
            return None;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(value) => {
                    if !scratchpad_value_matches_thread(&value, &thread_id) {
                        self.add_error_message(format!(
                            "Built-in scratchpad `{thread_id}` is owned by another thread and cannot be used for continuous policy."
                        ));
                        return None;
                    }
                    Some(value)
                }
                Err(err) => {
                    self.add_error_message(format!(
                        "Built-in scratchpad `{thread_id}` is invalid JSON: {err}"
                    ));
                    None
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let default_continuous = self
                    .config
                    .scratchpad
                    .for_mode(self.active_mode_kind())
                    .default_continuous;
                Some(serde_json::json!({
                    "scratchpad_id": thread_id.to_string(),
                    "origin_thread_id": thread_id.to_string(),
                    "objective": "Session continuous run policy",
                    "status": "active",
                    "completed": [],
                    "next_steps": [],
                    "pending_waits": [],
                    "run_policy": {
                        "continuous": {
                            "enabled": default_continuous
                        }
                    },
                    "communication_policy": {
                        "fallback": {
                            "final_response_on_channel_failure": false
                        }
                    },
                    "created_at": chrono::Utc::now().to_rfc3339(),
                    "updated_at": chrono::Utc::now().to_rfc3339(),
                    "archived_at": null
                }))
            }
            Err(err) => {
                self.add_error_message(format!(
                    "Could not read built-in scratchpad `{thread_id}`: {err}"
                ));
                None
            }
        }
    }

    fn set_current_scratchpad_continuous_policy(&mut self, enabled: bool) {
        let Some(thread_id) = self.thread_id else {
            self.add_error_message(
                "'/continuous' is unavailable before the session starts.".to_string(),
            );
            return;
        };
        if !self.submit_op(AppCommand::set_scratchpad_continuous_policy(enabled)) {
            self.add_error_message(format!(
                "Could not submit continuous policy update for scratchpad `{thread_id}`."
            ));
            return;
        }
        let state = if enabled { "enable" } else { "disable" };
        self.add_info_message(
            format!("Continuous run policy {state} requested for scratchpad `{thread_id}`."),
            Some(CONTINUOUS_USAGE.to_string()),
        );
    }

    fn current_scratchpad_continuous_enabled(&mut self) -> Option<bool> {
        let value = self.read_current_scratchpad_value()?;
        Some(
            value
                .get("run_policy")
                .and_then(|policy| policy.get("continuous"))
                .and_then(|continuous| continuous.get("enabled"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        )
    }

    fn add_current_continuous_status(&mut self) {
        let Some((thread_id, _)) = self.current_scratchpad_path() else {
            self.add_error_message(
                "'/continuous' is unavailable before the session starts.".to_string(),
            );
            return;
        };
        let Some(enabled) = self.current_scratchpad_continuous_enabled() else {
            return;
        };
        let state = if enabled { "enabled" } else { "disabled" };
        self.add_info_message(
            format!("Continuous run policy is {state} for scratchpad `{thread_id}`."),
            Some(CONTINUOUS_USAGE.to_string()),
        );
    }

    fn dispatch_continuous_command(&mut self, args: Option<&str>) {
        match args.map(str::trim).filter(|args| !args.is_empty()) {
            None => {
                if let Some(enabled) = self.current_scratchpad_continuous_enabled() {
                    self.set_current_scratchpad_continuous_policy(!enabled);
                }
            }
            Some("on" | "enable" | "enabled") => {
                self.set_current_scratchpad_continuous_policy(/*enabled*/ true);
            }
            Some("off" | "disable" | "disabled") => {
                self.set_current_scratchpad_continuous_policy(/*enabled*/ false);
            }
            Some("status") => self.add_current_continuous_status(),
            Some(_) => self.add_error_message(CONTINUOUS_USAGE.to_string()),
        }
    }

    fn emit_raw_output_mode_changed(&self, enabled: bool) {
        self.app_event_tx
            .send(AppEvent::RawOutputModeChanged { enabled });
    }

    fn slash_command_blocked_by_active_task(&self, cmd: SlashCommand) -> bool {
        (!cmd.available_during_task()
            && (self.turn_lifecycle.agent_turn_running
                || self.review.is_review_mode
                || (self.bottom_pane.is_task_running()
                    && (self.mcp_startup_status.is_none()
                        || self.input_queue.user_turn_pending_start))))
            || (matches!(cmd, SlashCommand::Resume | SlashCommand::Cd)
                && (self.input_queue.user_turn_pending_start
                    || self.turn_lifecycle.agent_turn_running))
            || (cmd == SlashCommand::Export && self.input_queue.suppress_queue_autosend)
    }

    fn dispatch_decision_provenance_command(
        &self,
        family: decision_provenance_commands::CommandFamily,
        args: &str,
    ) {
        decision_provenance_commands::spawn_command(
            if self.provenance_commands_enabled {
                decision_provenance_commands::ProvenanceCommandAccess::Local
            } else {
                decision_provenance_commands::ProvenanceCommandAccess::RemoteUnavailable
            },
            self.state_db.clone(),
            family,
            args,
            self.thread_id(),
            self.app_event_tx.clone(),
        );
    }

    pub(super) fn dispatch_command(&mut self, cmd: SlashCommand) {
        self.flush_completed_command_activity();
        if !self.ensure_slash_command_allowed_in_side_conversation(cmd) {
            return;
        }
        if !self.ensure_side_command_allowed_outside_review(cmd) {
            return;
        }
        if self.slash_command_blocked_by_active_task(cmd) {
            let message = format!(
                "'/{}' is disabled while a task is in progress.",
                cmd.command()
            );
            self.add_to_history(history_cell::new_error_event(message));
            self.bottom_pane.drain_pending_submission_state();
            self.request_redraw();
            return;
        }

        match cmd {
            SlashCommand::Feedback => {
                if !self.config.feedback_enabled {
                    let params = crate::bottom_pane::feedback_disabled_params();
                    self.bottom_pane.show_selection_view(params);
                    self.request_redraw();
                    return;
                }
                // Step 1: pick a category (UI built in feedback_view)
                let params =
                    crate::bottom_pane::feedback_selection_params(self.app_event_tx.clone());
                self.bottom_pane.show_selection_view(params);
                self.request_redraw();
            }
            SlashCommand::New => {
                self.app_event_tx.send(AppEvent::NewSession { name: None });
            }
            SlashCommand::Archive => {
                self.bottom_pane.show_selection_view(SelectionViewParams {
                    title: Some("Archive this session?".to_string()),
                    subtitle: Some(
                        "Are you sure? This will archive the current session and exit Codex"
                            .to_string(),
                    ),
                    footer_hint: Some(standard_popup_hint_line()),
                    items: vec![
                        SelectionItem {
                            name: "No, don't archive".to_string(),
                            description: Some("Return to the current session".to_string()),
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                        SelectionItem {
                            name: "Yes, archive and exit".to_string(),
                            description: Some("Archive this session now".to_string()),
                            actions: vec![Box::new(|tx| {
                                tx.send(AppEvent::ArchiveCurrentThread);
                            })],
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                });
                self.request_redraw();
            }
            SlashCommand::Delete => {
                self.bottom_pane.show_selection_view(SelectionViewParams {
                    title: Some("Delete this session?".to_string()),
                    subtitle: Some(
                        "Cannot be undone. Subagent threads will also be deleted.".to_string(),
                    ),
                    footer_hint: Some(standard_popup_hint_line()),
                    items: vec![
                        SelectionItem {
                            name: "No, keep this session".to_string(),
                            description: Some("Return to the current session".to_string()),
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                        SelectionItem {
                            name: "Yes, delete and exit".to_string(),
                            description: Some("Permanently delete this session now".to_string()),
                            actions: vec![Box::new(|tx| {
                                tx.send(AppEvent::DeleteCurrentThread);
                            })],
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                });
                self.request_redraw();
            }
            SlashCommand::Clear => {
                self.app_event_tx.send(AppEvent::ClearUi { name: None });
            }
            SlashCommand::Resume => {
                self.app_event_tx.send(AppEvent::OpenResumePicker);
            }
            SlashCommand::Fork => {
                self.app_event_tx
                    .send(AppEvent::ForkCurrentSession { name: None });
            }
            SlashCommand::App => {
                let Some(thread_id) = self.thread_id else {
                    self.add_error_message(
                        "Session is still starting; try /app again in a moment.".to_string(),
                    );
                    return;
                };
                self.app_event_tx
                    .send(AppEvent::OpenDesktopThread { thread_id });
            }
            SlashCommand::Init => {
                const INIT_PROMPT: &str = include_str!("../../prompt_for_init_command.md");
                self.submit_user_message(INIT_PROMPT.to_string().into());
            }
            SlashCommand::Compact => {
                if self.blocks_direct_input {
                    self.add_error_message(PARENT_OWNED_INPUT_MESSAGE.to_string());
                    return;
                }
                self.clear_token_usage();
                if !self.bottom_pane.is_task_running() {
                    self.bottom_pane.set_task_running(/*running*/ true);
                }
                self.input_queue.user_turn_pending_start = true;
                self.app_event_tx.compact();
            }
            SlashCommand::Review => {
                self.open_review_popup();
                if self.mcp_startup_status.is_some() {
                    self.defer_input_until_settings_applied();
                }
            }
            SlashCommand::Rename => {
                self.session_telemetry
                    .counter("codex.thread.rename", /*inc*/ 1, &[]);
                self.show_rename_prompt();
            }
            SlashCommand::Model => {
                self.open_model_popup();
                self.defer_input_until_settings_applied();
            }
            SlashCommand::Personality => {
                self.open_personality_popup();
                self.defer_input_until_settings_applied();
            }
            SlashCommand::Plan => {
                self.apply_plan_slash_command();
            }
            SlashCommand::Goal => {
                if !self.config.features.enabled(Feature::Goals) {
                    return;
                }
                if let Some(thread_id) = self.thread_id {
                    self.app_event_tx
                        .send(AppEvent::OpenThreadGoalMenu { thread_id });
                    self.append_message_history_entry("/goal".to_string());
                } else {
                    self.add_info_message(
                        GOAL_USAGE.to_string(),
                        Some(GOAL_USAGE_HINT.to_string()),
                    );
                }
            }
            cmd @ (SlashCommand::Side | SlashCommand::Btw) => {
                self.request_empty_side_conversation(cmd);
            }
            SlashCommand::Agent => {
                self.app_event_tx.send(AppEvent::OpenAgentPicker);
            }
            SlashCommand::Agents => {
                self.app_event_tx.send(AppEvent::OpenAgentsOverview);
            }
            SlashCommand::MultiAgents => {
                self.app_event_tx.send(AppEvent::OpenAgentPicker);
            }
            SlashCommand::AgentsPrune => {
                self.add_info_message(
                    "Pruning idle agents for this session.".to_string(),
                    Some(
                        "Running agents and agents with running descendants will remain active."
                            .to_string(),
                    ),
                );
                if !self.submit_op(AppCommand::prune_idle_agents()) {
                    self.add_error_message(
                        "Could not submit idle-agent prune request.".to_string(),
                    );
                }
            }
            SlashCommand::Permissions => {
                self.open_permissions_popup();
                self.defer_input_until_settings_applied();
            }
            SlashCommand::Vim => {
                self.toggle_vim_mode_and_notify();
            }
            SlashCommand::Keymap => {
                self.open_keymap_picker();
            }
            SlashCommand::ElevateSandbox => {
                #[cfg(target_os = "windows")]
                {
                    let windows_sandbox_level =
                        crate::windows_sandbox::level_from_config(&self.config);
                    let windows_degraded_sandbox_enabled =
                        matches!(windows_sandbox_level, WindowsSandboxLevel::RestrictedToken);
                    if !windows_degraded_sandbox_enabled {
                        // This command should not be visible/recognized outside degraded mode,
                        // but guard anyway in case something dispatches it directly.
                        return;
                    }

                    let Some(preset) = builtin_approval_presets()
                        .into_iter()
                        .find(|preset| preset.id == "auto")
                    else {
                        // Avoid panicking in interactive UI; treat this as a recoverable
                        // internal error.
                        self.add_error_message(
                            "Internal error: missing the 'auto' approval preset.".to_string(),
                        );
                        return;
                    };

                    if let Err(err) = self
                        .config
                        .permissions
                        .approval_policy
                        .can_set(&preset.approval)
                    {
                        self.add_error_message(err.to_string());
                        return;
                    }

                    self.session_telemetry.counter(
                        "codex.windows_sandbox.setup_elevated_sandbox_command",
                        /*inc*/ 1,
                        &[],
                    );
                    self.app_event_tx
                        .send(AppEvent::BeginWindowsSandboxElevatedSetup {
                            preset,
                            profile_selection: None,
                        });
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = &self.session_telemetry;
                    // Not supported; on non-Windows this command should never be reachable.
                }
            }
            SlashCommand::SandboxReadRoot => {
                self.add_error_message(
                    "Usage: /sandbox-add-read-dir <absolute-directory-path>".to_string(),
                );
            }
            SlashCommand::Experimental => {
                self.open_experimental_popup();
            }
            SlashCommand::AutoReview => {
                self.open_auto_review_denials_popup();
            }
            SlashCommand::Memories => {
                self.open_memories_popup();
            }
            SlashCommand::Quit | SlashCommand::Exit => {
                self.request_quit_without_confirmation();
            }
            SlashCommand::Logout => {
                self.app_event_tx.send(AppEvent::Logout);
            }
            SlashCommand::Copy => {
                self.show_copy_picker();
            }
            SlashCommand::Export => {
                self.show_transcript_export_popup();
            }
            SlashCommand::Raw => {
                let enabled = self.toggle_raw_output_mode_and_notify();
                self.emit_raw_output_mode_changed(enabled);
            }
            SlashCommand::Diff => {
                self.add_diff_in_progress();
                let tx = self.app_event_tx.clone();
                let runner = self.workspace_command_runner.clone();
                let cwd = self
                    .current_cwd
                    .clone()
                    .unwrap_or_else(|| self.config.cwd.to_path_buf());
                tokio::spawn(async move {
                    let text = match runner {
                        Some(runner) => match get_git_diff(runner.as_ref(), &cwd).await {
                            Ok((is_git_repo, diff_text)) => {
                                if is_git_repo {
                                    diff_text
                                } else {
                                    "`/diff` — _not inside a git repository_".to_string()
                                }
                            }
                            Err(e) => format!("Failed to compute diff: {e}"),
                        },
                        None => "Failed to compute diff: workspace command runner unavailable"
                            .to_string(),
                    };
                    tx.send(AppEvent::DiffResult(cwd, text));
                });
            }
            SlashCommand::Mention => {
                self.insert_str("@");
            }
            SlashCommand::Skills => {
                self.open_skills_menu();
            }
            SlashCommand::Import => {
                self.app_event_tx
                    .send(AppEvent::OpenExternalAgentConfigMigration);
            }
            SlashCommand::Hooks => {
                self.add_hooks_output();
            }
            SlashCommand::Status => {
                if self.should_prefetch_rate_limits() {
                    let request_id = self.next_status_refresh_request_id;
                    self.next_status_refresh_request_id =
                        self.next_status_refresh_request_id.wrapping_add(1);
                    self.add_status_output(/*refreshing_rate_limits*/ true, Some(request_id));
                    self.app_event_tx.send(AppEvent::RefreshRateLimits {
                        origin: RateLimitRefreshOrigin::StatusCommand { request_id },
                    });
                } else {
                    self.add_status_output(
                        /*refreshing_rate_limits*/ false, /*request_id*/ None,
                    );
                }
            }
            SlashCommand::Spend => {
                self.add_spend_output("");
            }
            SlashCommand::Mic => {
                self.app_event_tx
                    .send(AppEvent::RealtimeMicControl(RealtimeMicCommand::Toggle));
            }
            SlashCommand::Voice => {
                self.app_event_tx
                    .send(AppEvent::RealtimeVoiceControl(RealtimeVoiceCommand::Status));
            }
            SlashCommand::Cd => {
                self.dispatch_command_with_args(SlashCommand::Cd, "~".to_string(), Vec::new());
            }
            SlashCommand::Pwd => {
                self.add_info_message(
                    format!("Current working directory: {}", self.config.cwd.display()),
                    /*hint*/ None,
                );
            }
            SlashCommand::Usage => {
                if self.ensure_usage_command_available() {
                    self.open_usage_menu();
                }
            }
            SlashCommand::Ide => {
                self.handle_ide_command();
            }
            SlashCommand::DebugConfig => {
                self.add_debug_config_output();
            }
            SlashCommand::Title => {
                self.open_terminal_title_setup();
            }
            SlashCommand::Statusline => {
                self.open_status_line_setup();
            }
            SlashCommand::Theme => {
                self.open_theme_picker();
            }
            SlashCommand::Pets => {
                self.open_pets_picker();
            }
            SlashCommand::Ps => {
                self.add_ps_output();
            }
            SlashCommand::Stop => {
                self.clean_background_terminals();
            }
            SlashCommand::MemoryDrop => {
                self.add_app_server_stub_message("Memory maintenance");
            }
            SlashCommand::MemoryUpdate => {
                self.add_app_server_stub_message("Memory maintenance");
            }
            SlashCommand::Mcp => {
                self.add_mcp_output(McpServerStatusDetail::ToolsAndAuthOnly);
            }
            SlashCommand::OrchestratorMemoryForget => {
                self.add_error_message("Usage: /orchestrator-memory-forget <needle>".to_string());
            }
            SlashCommand::OrchestratorMemoryConsolidate => {
                self.add_info_message(
                    "Orchestrator memory consolidation started.".to_string(),
                    Some(
                        "This runs the configured cleanup path now, including model-assisted semantic consolidation when enabled."
                            .to_string(),
                    ),
                );
                self.submit_op(AppCommand::ConsolidateOrchestratorMemory);
            }
            SlashCommand::UserPreferencesMemoryMigrate => {
                self.add_info_message(
                    "User preferences memory migration started.".to_string(),
                    Some(
                        "This copies missing files from orchestrator_memory into memories/extensions/user_preferences."
                            .to_string(),
                    ),
                );
                self.submit_op(AppCommand::UserPreferencesMemoryMigrate);
            }
            SlashCommand::Scratchpad => {
                self.add_current_scratchpad_output();
            }
            SlashCommand::SessionTmp => {
                self.dispatch_session_tmp_command("");
            }
            SlashCommand::ScratchpadAbsorb => {
                self.add_error_message(SCRATCHPAD_ABSORB_USAGE.to_string());
            }
            SlashCommand::ScratchpadUnarchive => {
                self.unarchive_current_scratchpad();
            }
            SlashCommand::Outcomes => {
                self.dispatch_outcomes_command(/*args*/ None);
            }
            SlashCommand::Continuous => {
                self.dispatch_continuous_command(/*args*/ None);
            }
            SlashCommand::Decisions => {
                self.dispatch_decision_provenance_command(
                    decision_provenance_commands::CommandFamily::Decisions,
                    "",
                );
            }
            SlashCommand::PreferenceBoundaries => {
                self.dispatch_decision_provenance_command(
                    decision_provenance_commands::CommandFamily::PreferenceBoundaries,
                    "",
                );
            }
            SlashCommand::Apps => {
                self.add_connectors_output();
            }
            SlashCommand::Plugins => {
                self.add_plugins_output();
            }
            SlashCommand::Account => {
                let label = self
                    .config
                    .active_account_alias()
                    .unwrap_or("default")
                    .to_string();
                self.add_info_message(
                    format!("Current session account alias: {label}"),
                    Some(
                        "Use `/account <alias>` to switch this session, or `/account default` to return to the root auth store."
                            .to_string(),
                    ),
                );
            }
            SlashCommand::Rollout => {
                if let Some(path) = self.rollout_path() {
                    self.add_info_message(
                        format!("Current rollout path: {}", path.display()),
                        /*hint*/ None,
                    );
                } else {
                    self.add_info_message(
                        "Rollout path is not available yet.".to_string(),
                        /*hint*/ None,
                    );
                }
            }
            SlashCommand::TestApproval => {
                use std::collections::HashMap;

                use crate::approval_events::ApplyPatchApprovalRequestEvent;
                use crate::diff_model::FileChange;

                self.on_apply_patch_approval_request(
                    "1".to_string(),
                    ApplyPatchApprovalRequestEvent {
                        call_id: "1".to_string(),
                        turn_id: "turn-1".to_string(),
                        changes: HashMap::from([
                            (
                                PathBuf::from("/tmp/test.txt"),
                                FileChange::Add {
                                    content: "test".to_string(),
                                },
                            ),
                            (
                                PathBuf::from("/tmp/test2.txt"),
                                FileChange::Update {
                                    unified_diff: "+test\n-test2".to_string(),
                                    move_path: None,
                                },
                            ),
                        ]),
                        reason: None,
                        grant_root: Some(PathBuf::from("/tmp")),
                    },
                );
            }
        }
    }

    /// Run an inline slash command.
    ///
    /// Branches that prepare arguments should pass `record_history: false` to the composer because
    /// the staged slash-command entry is the recall record; using the normal submission-history
    /// path as well would make a single command appear twice during Up-arrow navigation.
    pub(super) fn dispatch_command_with_args(
        &mut self,
        cmd: SlashCommand,
        args: String,
        text_elements: Vec<TextElement>,
    ) {
        if !self.ensure_slash_command_allowed_in_side_conversation(cmd) {
            return;
        }
        if !self.ensure_side_command_allowed_outside_review(cmd) {
            return;
        }
        if !cmd.supports_inline_args() {
            self.dispatch_command(cmd);
            return;
        }
        if self.slash_command_blocked_by_active_task(cmd) {
            let message = format!(
                "'/{}' is disabled while a task is in progress.",
                cmd.command()
            );
            self.add_to_history(history_cell::new_error_event(message));
            self.request_redraw();
            return;
        }

        let trimmed = args.trim();
        if trimmed.is_empty() {
            self.dispatch_command(cmd);
            return;
        }

        if cmd == SlashCommand::Goal {
            self.dispatch_prepared_command_with_args(
                cmd,
                PreparedSlashCommandArgs {
                    args,
                    text_elements,
                    pending_pastes: self.bottom_pane.composer_pending_pastes(),
                    local_images: self.bottom_pane.composer_local_images(),
                    remote_image_urls: self.bottom_pane.remote_image_urls(),
                    mention_bindings: Vec::new(),
                    source: SlashCommandDispatchSource::Live,
                },
            );
            return;
        }

        let Some((prepared_args, prepared_elements)) =
            self.prepare_live_inline_args(args, text_elements)
        else {
            return;
        };
        self.dispatch_prepared_command_with_args(
            cmd,
            PreparedSlashCommandArgs {
                args: prepared_args,
                text_elements: prepared_elements,
                pending_pastes: Vec::new(),
                local_images: Vec::new(),
                remote_image_urls: Vec::new(),
                mention_bindings: Vec::new(),
                source: SlashCommandDispatchSource::Live,
            },
        );
    }

    fn prepare_live_inline_args(
        &mut self,
        args: String,
        text_elements: Vec<TextElement>,
    ) -> Option<(String, Vec<TextElement>)> {
        if self.bottom_pane.composer_text().is_empty() {
            Some((args, text_elements))
        } else {
            self.bottom_pane
                .prepare_inline_args_submission(/*record_history*/ false)
        }
    }

    fn clear_live_goal_submission(&mut self) {
        self.bottom_pane
            .set_composer_text(String::new(), Vec::new(), Vec::new());
        self.bottom_pane.set_composer_pending_pastes(Vec::new());
        self.bottom_pane.drain_pending_submission_state();
    }

    fn prepared_inline_user_message(
        &mut self,
        args: String,
        text_elements: Vec<TextElement>,
        mut local_images: Vec<LocalImageAttachment>,
        mut remote_image_urls: Vec<String>,
        mut mention_bindings: Vec<MentionBinding>,
        source: SlashCommandDispatchSource,
    ) -> UserMessage {
        if source == SlashCommandDispatchSource::Live {
            local_images = self
                .bottom_pane
                .take_recent_submission_images_with_placeholders();
            remote_image_urls = self.take_remote_image_urls();
            mention_bindings = self.bottom_pane.take_recent_submission_mention_bindings();
        }
        UserMessage {
            text: args,
            local_images,
            remote_image_urls,
            text_elements,
            mention_bindings,
        }
    }

    fn dispatch_prepared_command_with_args(
        &mut self,
        cmd: SlashCommand,
        prepared: PreparedSlashCommandArgs,
    ) {
        let PreparedSlashCommandArgs {
            args,
            text_elements,
            pending_pastes,
            local_images,
            remote_image_urls,
            mention_bindings,
            source,
        } = prepared;
        let trimmed = args.trim();
        match cmd {
            SlashCommand::Export if trimmed.is_empty() => self.show_transcript_export_popup(),
            SlashCommand::Export => {
                self.set_queue_autosend_suppressed(/*suppressed*/ true);
                self.app_event_tx.send(AppEvent::ExportTranscript {
                    destination: crate::app_event::TranscriptExportDestination::File(
                        PathBuf::from(trimmed),
                    ),
                });
            }
            SlashCommand::Cd => self.request_working_directory_change(trimmed),
            SlashCommand::Pwd => {
                self.add_error_message("Usage: /pwd".to_string());
            }
            SlashCommand::Usage => {
                if self.ensure_usage_command_available() {
                    match tokens::TokenActivityView::parse(trimmed) {
                        Some(view) => self.add_token_activity_output(view),
                        None => self.add_error_message(
                            "Usage: /usage [daily|weekly|cumulative]".to_string(),
                        ),
                    }
                }
            }
            SlashCommand::Spend => {
                self.add_spend_output(trimmed);
            }
            SlashCommand::Ide => {
                self.handle_ide_command_args(trimmed);
            }
            SlashCommand::Mcp => match trimmed.to_ascii_lowercase().as_str() {
                "verbose" => self.add_mcp_output(McpServerStatusDetail::Full),
                _ => self.add_error_message("Usage: /mcp [verbose]".to_string()),
            },
            SlashCommand::Continuous => {
                self.dispatch_continuous_command(Some(trimmed));
            }
            SlashCommand::Decisions => {
                self.dispatch_decision_provenance_command(
                    decision_provenance_commands::CommandFamily::Decisions,
                    trimmed,
                );
            }
            SlashCommand::PreferenceBoundaries => {
                self.dispatch_decision_provenance_command(
                    decision_provenance_commands::CommandFamily::PreferenceBoundaries,
                    trimmed,
                );
            }
            SlashCommand::Outcomes => {
                self.dispatch_outcomes_command(Some(trimmed));
            }
            SlashCommand::SessionTmp => {
                self.dispatch_session_tmp_command(trimmed);
            }
            SlashCommand::ScratchpadAbsorb => {
                self.dispatch_scratchpad_absorb_command(trimmed);
            }
            SlashCommand::OrchestratorMemoryForget if !trimmed.is_empty() => {
                self.submit_op(AppCommand::OrchestratorMemoryForget { needle: args });
            }
            SlashCommand::Account if !trimmed.is_empty() => {
                let alias = if trimmed.eq_ignore_ascii_case("default") {
                    None
                } else {
                    Some(trimmed.to_string())
                };
                self.app_event_tx.send(AppEvent::SwitchAccount {
                    alias,
                    reason: crate::app_event::AccountSwitchReason::User,
                });
            }
            SlashCommand::Keymap => match trimmed.to_ascii_lowercase().as_str() {
                "" => self.open_keymap_picker(),
                "debug" => {
                    match crate::keymap::RuntimeKeymap::from_config(&self.config.tui_keymap) {
                        Ok(runtime_keymap) => self.open_keymap_debug(&runtime_keymap),
                        Err(err) => {
                            self.add_error_message(format!(
                                "Invalid `tui.keymap` configuration: {err}"
                            ));
                        }
                    }
                }
                _ => self.add_error_message("Usage: /keymap [debug]".to_string()),
            },
            SlashCommand::Raw => match trimmed.to_ascii_lowercase().as_str() {
                "on" => {
                    self.set_raw_output_mode_and_notify(/*enabled*/ true);
                    self.emit_raw_output_mode_changed(/*enabled*/ true);
                }
                "off" => {
                    self.set_raw_output_mode_and_notify(/*enabled*/ false);
                    self.emit_raw_output_mode_changed(/*enabled*/ false);
                }
                _ => self.add_error_message(RAW_USAGE.to_string()),
            },
            SlashCommand::Mic => {
                let normalized = trimmed.to_ascii_lowercase();
                match normalized.as_str() {
                    "help" | "?" => self.add_info_message(MIC_USAGE.to_string(), None),
                    "on" => self
                        .app_event_tx
                        .send(AppEvent::RealtimeMicControl(RealtimeMicCommand::On)),
                    "off" => self
                        .app_event_tx
                        .send(AppEvent::RealtimeMicControl(RealtimeMicCommand::Off)),
                    "status" => self
                        .app_event_tx
                        .send(AppEvent::RealtimeMicControl(RealtimeMicCommand::Status)),
                    "hot" => self
                        .app_event_tx
                        .send(AppEvent::RealtimeMicControl(RealtimeMicCommand::Hot)),
                    "push" => self
                        .app_event_tx
                        .send(AppEvent::RealtimeMicControl(RealtimeMicCommand::Push)),
                    "hotkey" => self.app_event_tx.send(AppEvent::RealtimeMicControl(
                        RealtimeMicCommand::CaptureHotkey,
                    )),
                    "change" => self.app_event_tx.send(AppEvent::RealtimeMicControl(
                        RealtimeMicCommand::ChangeMicrophone,
                    )),
                    "devices" | "list" => self.app_event_tx.send(AppEvent::RealtimeMicControl(
                        RealtimeMicCommand::ListDevices,
                    )),
                    "aliases" | "alias list" => self.app_event_tx.send(
                        AppEvent::RealtimeMicControl(RealtimeMicCommand::ListMicrophoneAliases),
                    ),
                    "speaker change" | "output change" => self.app_event_tx.send(
                        AppEvent::RealtimeMicControl(RealtimeMicCommand::ChangeSpeaker),
                    ),
                    "speakers" | "outputs" | "speaker list" | "output list" => {
                        self.app_event_tx.send(AppEvent::RealtimeMicControl(
                            RealtimeMicCommand::ListSpeakers,
                        ))
                    }
                    "speaker aliases" | "output aliases" | "speaker alias list"
                    | "output alias list" => self.app_event_tx.send(AppEvent::RealtimeMicControl(
                        RealtimeMicCommand::ListSpeakerAliases,
                    )),
                    _ if normalized.starts_with("speaker alias ")
                        || normalized.starts_with("output alias ") =>
                    {
                        let prefix = if normalized.starts_with("speaker alias ") {
                            "speaker alias "
                        } else {
                            "output alias "
                        };
                        match realtime_alias_args(&trimmed[prefix.len()..]) {
                            Some((alias, device)) => {
                                self.app_event_tx.send(AppEvent::RealtimeMicControl(
                                    RealtimeMicCommand::SetSpeakerAlias { alias, device },
                                ))
                            }
                            None => self.add_error_message(MIC_USAGE.to_string()),
                        }
                    }
                    _ if normalized.starts_with("alias ") => {
                        match realtime_alias_args(&trimmed["alias ".len()..]) {
                            Some((alias, device)) => {
                                self.app_event_tx.send(AppEvent::RealtimeMicControl(
                                    RealtimeMicCommand::SetMicrophoneAlias { alias, device },
                                ))
                            }
                            None => self.add_error_message(MIC_USAGE.to_string()),
                        }
                    }
                    _ if normalized.starts_with("device ") || normalized.starts_with("use ") => {
                        let name = trimmed
                            .split_once(char::is_whitespace)
                            .map(|(_, name)| name.trim())
                            .filter(|name| !name.is_empty());
                        match name {
                            Some(name) => self.app_event_tx.send(AppEvent::RealtimeMicControl(
                                RealtimeMicCommand::SetMicrophone(name.to_string()),
                            )),
                            None => self.add_error_message(MIC_USAGE.to_string()),
                        }
                    }
                    _ if normalized.starts_with("speaker ")
                        || normalized.starts_with("output ") =>
                    {
                        let name = trimmed
                            .split_once(char::is_whitespace)
                            .map(|(_, name)| name.trim())
                            .filter(|name| !name.is_empty());
                        match name {
                            Some(name) => self.app_event_tx.send(AppEvent::RealtimeMicControl(
                                RealtimeMicCommand::SetSpeaker(name.to_string()),
                            )),
                            None => self.add_error_message(MIC_USAGE.to_string()),
                        }
                    }
                    _ if !trimmed.is_empty() => {
                        self.app_event_tx.send(AppEvent::RealtimeMicControl(
                            RealtimeMicCommand::SetMicrophone(trimmed.to_string()),
                        ))
                    }
                    _ => self.add_error_message(MIC_USAGE.to_string()),
                }
            }
            SlashCommand::Voice => {
                let normalized = trimmed.to_ascii_lowercase();
                match normalized.as_str() {
                    "" | "status" => self
                        .app_event_tx
                        .send(AppEvent::RealtimeVoiceControl(RealtimeVoiceCommand::Status)),
                    "help" | "?" => self.add_info_message(VOICE_USAGE.to_string(), None),
                    "on" => self
                        .app_event_tx
                        .send(AppEvent::RealtimeVoiceControl(RealtimeVoiceCommand::On)),
                    "off" => self
                        .app_event_tx
                        .send(AppEvent::RealtimeVoiceControl(RealtimeVoiceCommand::Off)),
                    "debug" => self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                        RealtimeVoiceCommand::Debug(RealtimeVoiceDebugCommand::Toggle),
                    )),
                    "debug on" => self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                        RealtimeVoiceCommand::Debug(RealtimeVoiceDebugCommand::On),
                    )),
                    "debug off" => self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                        RealtimeVoiceCommand::Debug(RealtimeVoiceDebugCommand::Off),
                    )),
                    "debug status" => self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                        RealtimeVoiceCommand::Debug(RealtimeVoiceDebugCommand::Status),
                    )),
                    "calibrate" => self.add_error_message(VOICE_CALIBRATION_USAGE.to_string()),
                    _ if normalized.starts_with("calibrate ") => {
                        match realtime_calibration_path(trimmed) {
                            Some(path) => self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                                RealtimeVoiceCommand::Calibrate(path),
                            )),
                            None => self.add_error_message(VOICE_CALIBRATION_USAGE.to_string()),
                        }
                    }
                    "list" | "voices" => self
                        .app_event_tx
                        .send(AppEvent::RealtimeVoiceControl(RealtimeVoiceCommand::List)),
                    "tune" => match load_active_preset(self.config.codex_home.as_path()) {
                        Ok(Some(preset)) => {
                            self.bottom_pane.show_view(Box::new(
                                crate::bottom_pane::RealtimeVoiceTuner::new(
                                    preset,
                                    self.app_event_tx.clone(),
                                ),
                            ));
                            self.request_redraw();
                        }
                        Ok(None) => {
                            self.add_error_message(
                                "No active GPT-Live effect; use /voice effect use jarvis first."
                                    .to_string(),
                            );
                        }
                        Err(err) => {
                            self.add_error_message(format!(
                                "Failed to load the active GPT-Live effect: {err:#}"
                            ));
                        }
                    },
                    "effect" | "effects" | "effect status" | "effects status" => {
                        self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                            RealtimeVoiceCommand::Effect(RealtimeVoiceEffectCommand::Status),
                        ))
                    }
                    "effect list" | "effects list" => {
                        self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                            RealtimeVoiceCommand::Effect(RealtimeVoiceEffectCommand::List),
                        ))
                    }
                    "effect off" | "effects off" => {
                        self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                            RealtimeVoiceCommand::Effect(RealtimeVoiceEffectCommand::Off),
                        ))
                    }
                    _ if normalized.starts_with("effect use ")
                        || normalized.starts_with("effects use ") =>
                    {
                        let mut parts = trimmed.split_whitespace();
                        let _command = parts.next();
                        let _subcommand = parts.next();
                        match (parts.next(), parts.next()) {
                            (Some(name), None) if !name.is_empty() => self.app_event_tx.send(
                                AppEvent::RealtimeVoiceControl(RealtimeVoiceCommand::Effect(
                                    RealtimeVoiceEffectCommand::Use(name.to_string()),
                                )),
                            ),
                            _ => self.add_error_message(VOICE_EFFECT_USAGE.to_string()),
                        }
                    }
                    _ if normalized.starts_with("effect") || normalized.starts_with("effects") => {
                        self.add_error_message(VOICE_EFFECT_USAGE.to_string())
                    }
                    "profile" | "profiles" | "profile status" | "profiles status" => {
                        self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                            RealtimeVoiceCommand::Profile(RealtimeVoiceProfileCommand::Status),
                        ))
                    }
                    "profile list" | "profiles list" => {
                        self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                            RealtimeVoiceCommand::Profile(RealtimeVoiceProfileCommand::List),
                        ))
                    }
                    "profile off" | "profiles off" => {
                        self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                            RealtimeVoiceCommand::Profile(RealtimeVoiceProfileCommand::Off),
                        ))
                    }
                    _ if normalized.starts_with("profile use ")
                        || normalized.starts_with("profiles use ") =>
                    {
                        let mut parts = trimmed.split_whitespace();
                        let _command = parts.next();
                        let _subcommand = parts.next();
                        match (parts.next(), parts.next()) {
                            (Some(name), None) if !name.is_empty() => self.app_event_tx.send(
                                AppEvent::RealtimeVoiceControl(RealtimeVoiceCommand::Profile(
                                    RealtimeVoiceProfileCommand::Use(name.to_string()),
                                )),
                            ),
                            _ => self.add_error_message(VOICE_PROFILE_USAGE.to_string()),
                        }
                    }
                    _ if normalized.starts_with("profile")
                        || normalized.starts_with("profiles") =>
                    {
                        self.add_error_message(VOICE_PROFILE_USAGE.to_string())
                    }
                    "history" | "recent" => self.add_realtime_history_output(None),
                    _ if normalized.starts_with("history ")
                        || normalized.starts_with("recent ") =>
                    {
                        let mut parts = trimmed.split_whitespace();
                        let _command = parts.next();
                        match (parts.next(), parts.next()) {
                            (Some(count), None) => match count.parse::<usize>() {
                                Ok(count @ 1..=20) => {
                                    self.add_realtime_history_output(Some(count));
                                }
                                _ => self.add_error_message(VOICE_HISTORY_USAGE.to_string()),
                            },
                            _ => self.add_error_message(VOICE_HISTORY_USAGE.to_string()),
                        }
                    }
                    _ => match realtime_voice_from_name(trimmed) {
                        Some(voice) => self.app_event_tx.send(AppEvent::RealtimeVoiceControl(
                            RealtimeVoiceCommand::Set(voice),
                        )),
                        None => self.add_error_message(VOICE_USAGE.to_string()),
                    },
                }
            }
            SlashCommand::Rename if !trimmed.is_empty() => {
                if !self.ensure_thread_rename_allowed() {
                    return;
                }
                self.session_telemetry
                    .counter("codex.thread.rename", /*inc*/ 1, &[]);
                let Some(name) = normalize_thread_name(&args) else {
                    self.add_error_message("Thread name cannot be empty.".to_string());
                    return;
                };
                self.app_event_tx.set_thread_name(name);
            }
            SlashCommand::New if !trimmed.is_empty() => {
                self.app_event_tx.send(AppEvent::NewSession {
                    name: Some(trimmed.to_string()),
                });
            }
            SlashCommand::Clear if !trimmed.is_empty() => {
                self.app_event_tx.send(AppEvent::ClearUi {
                    name: Some(trimmed.to_string()),
                });
            }
            SlashCommand::Fork if !trimmed.is_empty() => {
                self.app_event_tx.send(AppEvent::ForkCurrentSession {
                    name: Some(trimmed.to_string()),
                });
            }
            SlashCommand::Plan if !trimmed.is_empty() => {
                if !self.apply_plan_slash_command() {
                    return;
                }
                let mut user_message = self.prepared_inline_user_message(
                    args,
                    text_elements,
                    local_images,
                    remote_image_urls,
                    mention_bindings,
                    source,
                );
                if !self.is_session_configured()
                    || self.current_model().trim().is_empty()
                    || (!self.current_model_supports_images()
                        && (!user_message.local_images.is_empty()
                            || !user_message.remote_image_urls.is_empty()))
                {
                    const PLAN_PREFIX: &str = "/plan ";
                    user_message.text.insert_str(0, PLAN_PREFIX);
                    for element in &mut user_message.text_elements {
                        element.byte_range.start += PLAN_PREFIX.len();
                        element.byte_range.end += PLAN_PREFIX.len();
                    }
                }
                if self.is_session_configured() {
                    self.reasoning_buffer.clear();
                    self.reasoning_header = None;
                    self.reasoning_summary_parts.clear();
                    self.set_status_header(String::from("Working"));
                    self.submit_user_message_with_shell_escape_policy(
                        user_message,
                        ShellEscapePolicy::Disallow,
                    );
                } else {
                    self.queue_user_message_with_options(
                        user_message,
                        QueuedInputAction::ParseSlash,
                        Vec::new(),
                    );
                }
            }
            SlashCommand::Goal if !trimmed.is_empty() => {
                if !self.config.features.enabled(Feature::Goals) {
                    if source == SlashCommandDispatchSource::Live {
                        self.clear_live_goal_submission();
                    }
                    return;
                }
                enum GoalControlCommand {
                    Clear,
                    SetStatus(AppThreadGoalStatus),
                }
                let control_command = match trimmed.to_ascii_lowercase().as_str() {
                    "clear" => Some(GoalControlCommand::Clear),
                    "edit" => {
                        self.app_event_tx.send(AppEvent::OpenThreadGoalEditor {
                            thread_id: self.thread_id,
                        });
                        if source == SlashCommandDispatchSource::Live {
                            self.clear_live_goal_submission();
                        }
                        return;
                    }
                    "pause" => Some(GoalControlCommand::SetStatus(AppThreadGoalStatus::Paused)),
                    "resume" => Some(GoalControlCommand::SetStatus(AppThreadGoalStatus::Active)),
                    _ => None,
                };
                if let Some(command) = control_command {
                    let Some(thread_id) = self.thread_id else {
                        self.add_info_message(
                            GOAL_USAGE.to_string(),
                            Some(
                                "The session must start before you can change a goal.".to_string(),
                            ),
                        );
                        if source == SlashCommandDispatchSource::Live {
                            self.clear_live_goal_submission();
                        }
                        return;
                    };
                    match command {
                        GoalControlCommand::Clear => {
                            self.app_event_tx
                                .send(AppEvent::ClearThreadGoal { thread_id });
                        }
                        GoalControlCommand::SetStatus(status) => {
                            self.app_event_tx
                                .send(AppEvent::SetThreadGoalStatus { thread_id, status });
                        }
                    }
                    self.append_message_history_entry(format!("/goal {trimmed}"));
                    if source == SlashCommandDispatchSource::Live {
                        self.clear_live_goal_submission();
                    }
                    return;
                }
                let draft = GoalDraft {
                    objective: args,
                    text_elements,
                    pending_pastes,
                    local_images,
                    remote_image_urls,
                };
                let Some(thread_id) = self.thread_id else {
                    if source == SlashCommandDispatchSource::Live {
                        const GOAL_PREFIX: &str = "/goal ";
                        let text_elements = draft
                            .text_elements
                            .into_iter()
                            .map(|element| {
                                element.map_range(|range| ByteRange {
                                    start: range.start + GOAL_PREFIX.len(),
                                    end: range.end + GOAL_PREFIX.len(),
                                })
                            })
                            .collect();
                        self.queue_user_message_with_options(
                            UserMessage {
                                text: format!("{GOAL_PREFIX}{}", draft.objective),
                                local_images: draft.local_images,
                                remote_image_urls: draft.remote_image_urls,
                                text_elements,
                                mention_bindings: Vec::new(),
                            },
                            QueuedInputAction::ParseSlash,
                            draft.pending_pastes,
                        );
                        self.clear_live_goal_submission();
                    } else {
                        self.add_info_message(
                            GOAL_USAGE.to_string(),
                            Some("The session must start before you can set a goal.".to_string()),
                        );
                    }
                    return;
                };
                let history_objective = draft.objective.clone();
                self.app_event_tx.send(AppEvent::SetThreadGoalDraft {
                    thread_id,
                    draft,
                    mode: ThreadGoalSetMode::ConfirmIfExists,
                });
                self.append_message_history_entry(format!("/goal {history_objective}"));
                if source == SlashCommandDispatchSource::Live {
                    self.clear_live_goal_submission();
                }
            }
            SlashCommand::Side | SlashCommand::Btw if !trimmed.is_empty() => {
                let Some(parent_thread_id) = self.thread_id else {
                    self.add_error_message(format!(
                        "'/{}' is unavailable before the session starts.",
                        cmd.command()
                    ));
                    return;
                };
                let user_message = self.prepared_inline_user_message(
                    args,
                    text_elements,
                    local_images,
                    remote_image_urls,
                    mention_bindings,
                    source,
                );
                self.request_side_conversation(parent_thread_id, Some(user_message));
            }
            SlashCommand::Review if !trimmed.is_empty() => {
                self.submit_op(AppCommand::review(ReviewTarget::Custom {
                    instructions: args,
                }));
            }
            SlashCommand::Resume if !trimmed.is_empty() => {
                self.app_event_tx
                    .send(AppEvent::ResumeSessionByIdOrName(args));
            }
            SlashCommand::SandboxReadRoot if !trimmed.is_empty() => {
                self.app_event_tx
                    .send(AppEvent::BeginWindowsSandboxGrantReadRoot { path: args });
            }
            SlashCommand::Pets
                if matches!(
                    args.trim().to_ascii_lowercase().as_str(),
                    "disable" | "disabled" | "hide" | "hidden" | "off" | "none"
                ) =>
            {
                self.app_event_tx.send(AppEvent::PetDisabled);
            }
            SlashCommand::Pets if !trimmed.is_empty() => {
                self.select_pet_by_id(args);
            }
            _ => self.dispatch_command(cmd),
        }
        if source == SlashCommandDispatchSource::Live && cmd != SlashCommand::Goal {
            self.bottom_pane.drain_pending_submission_state();
        }
    }

    pub(super) fn submit_queued_slash_prompt(
        &mut self,
        queued_message: QueuedUserMessage,
    ) -> QueueDrain {
        let QueuedUserMessage {
            user_message,
            pending_pastes,
            ..
        } = queued_message;
        let UserMessage {
            text,
            local_images,
            remote_image_urls,
            text_elements,
            mention_bindings,
        } = user_message;
        let Some((name, rest, rest_offset)) = parse_slash_name(&text) else {
            self.submit_user_message(UserMessage {
                text,
                local_images,
                remote_image_urls,
                text_elements,
                mention_bindings,
            });
            return QueueDrain::Stop;
        };

        if name.contains('/') {
            self.submit_user_message(UserMessage {
                text,
                local_images,
                remote_image_urls,
                text_elements,
                mention_bindings,
            });
            return QueueDrain::Stop;
        }

        let service_tier_commands = self.current_model_service_tier_commands();
        let Some(command) =
            find_slash_command(name, self.builtin_command_flags(), &service_tier_commands)
        else {
            self.add_info_message(
                format!(
                    r#"Unrecognized command '/{name}'. Type "/" for a list of supported commands."#
                ),
                /*hint*/ None,
            );
            return QueueDrain::Continue;
        };

        if rest.is_empty() {
            return match command {
                SlashCommandItem::Builtin(cmd) => {
                    self.dispatch_command(cmd);
                    self.queued_command_drain_result(cmd)
                }
                SlashCommandItem::ServiceTier(command) => {
                    self.handle_service_tier_command_dispatch(command);
                    QueueDrain::Continue
                }
            };
        }

        if !command.supports_inline_args() {
            self.submit_user_message(UserMessage {
                text,
                local_images,
                remote_image_urls,
                text_elements,
                mention_bindings,
            });
            return QueueDrain::Stop;
        }
        let SlashCommandItem::Builtin(cmd) = command else {
            self.submit_user_message(UserMessage {
                text,
                local_images,
                remote_image_urls,
                text_elements,
                mention_bindings,
            });
            return QueueDrain::Stop;
        };

        let trimmed_start = rest.trim_start();
        let leading_trimmed = rest.len().saturating_sub(trimmed_start.len());
        let trimmed_rest = trimmed_start.trim_end();
        let args_elements = Self::slash_command_args_elements(
            trimmed_rest,
            rest_offset + leading_trimmed,
            &text_elements,
        );
        self.dispatch_prepared_command_with_args(
            cmd,
            PreparedSlashCommandArgs {
                args: trimmed_rest.to_string(),
                text_elements: args_elements,
                pending_pastes,
                local_images,
                remote_image_urls,
                mention_bindings,
                source: SlashCommandDispatchSource::Queued,
            },
        );
        self.queued_command_drain_result(cmd)
    }

    fn builtin_command_flags(&self) -> BuiltinCommandFlags {
        #[cfg(target_os = "windows")]
        let allow_elevate_sandbox = {
            let windows_sandbox_level = crate::windows_sandbox::level_from_config(&self.config);
            matches!(windows_sandbox_level, WindowsSandboxLevel::RestrictedToken)
        };
        #[cfg(not(target_os = "windows"))]
        let allow_elevate_sandbox = false;

        BuiltinCommandFlags {
            collaboration_modes_enabled: self.collaboration_modes_enabled(),
            connectors_enabled: self.connectors_enabled(),
            plugins_command_enabled: self.config.features.enabled(Feature::Plugins),
            token_activity_command_enabled: self.has_codex_backend_auth,
            goal_command_enabled: self.config.features.enabled(Feature::Goals),
            service_tier_commands_enabled: self.fast_mode_enabled(),
            personality_command_enabled: self.config.features.enabled(Feature::Personality),
            allow_elevate_sandbox,
            side_conversation_active: self.active_side_conversation,
        }
    }

    fn ensure_usage_command_available(&mut self) -> bool {
        if self.has_codex_backend_auth {
            return true;
        }
        self.add_error_message(USAGE_CHATGPT_LOGIN_REQUIRED.to_string());
        false
    }

    fn queued_command_drain_result(&self, cmd: SlashCommand) -> QueueDrain {
        if self.is_user_turn_pending_or_running() || !self.bottom_pane.no_modal_or_popup_active() {
            return QueueDrain::Stop;
        }
        match cmd {
            SlashCommand::Ide
            | SlashCommand::Status
            | SlashCommand::Spend
            | SlashCommand::Mic
            | SlashCommand::Voice
            | SlashCommand::Pwd
            | SlashCommand::Usage
            | SlashCommand::DebugConfig
            | SlashCommand::Ps
            | SlashCommand::Stop
            | SlashCommand::MemoryDrop
            | SlashCommand::MemoryUpdate
            | SlashCommand::Mcp
            | SlashCommand::OrchestratorMemoryForget
            | SlashCommand::OrchestratorMemoryConsolidate
            | SlashCommand::UserPreferencesMemoryMigrate
            | SlashCommand::Scratchpad
            | SlashCommand::SessionTmp
            | SlashCommand::ScratchpadAbsorb
            | SlashCommand::ScratchpadUnarchive
            | SlashCommand::Outcomes
            | SlashCommand::Continuous
            | SlashCommand::Decisions
            | SlashCommand::PreferenceBoundaries
            | SlashCommand::Account
            | SlashCommand::AgentsPrune
            | SlashCommand::Apps
            | SlashCommand::Plugins
            | SlashCommand::Rollout
            | SlashCommand::Copy
            | SlashCommand::Raw
            | SlashCommand::Vim
            | SlashCommand::Diff
            | SlashCommand::App
            | SlashCommand::Rename
            | SlashCommand::TestApproval => QueueDrain::Continue,
            SlashCommand::Cd => match self.thread_id {
                Some(thread_id) if self.can_change_working_directory(thread_id) => QueueDrain::Stop,
                _ => QueueDrain::Continue,
            },
            SlashCommand::Feedback
            | SlashCommand::Export
            | SlashCommand::New
            | SlashCommand::Archive
            | SlashCommand::Delete
            | SlashCommand::Clear
            | SlashCommand::Resume
            | SlashCommand::Fork
            | SlashCommand::Init
            | SlashCommand::Compact
            | SlashCommand::Review
            | SlashCommand::Model
            | SlashCommand::Personality
            | SlashCommand::Plan
            | SlashCommand::Goal
            | SlashCommand::Side
            | SlashCommand::Btw
            | SlashCommand::Keymap
            | SlashCommand::Agent
            | SlashCommand::Agents
            | SlashCommand::MultiAgents
            | SlashCommand::Permissions
            | SlashCommand::ElevateSandbox
            | SlashCommand::SandboxReadRoot
            | SlashCommand::Experimental
            | SlashCommand::AutoReview
            | SlashCommand::Memories
            | SlashCommand::Quit
            | SlashCommand::Exit
            | SlashCommand::Logout
            | SlashCommand::Mention
            | SlashCommand::Skills
            | SlashCommand::Import
            | SlashCommand::Hooks
            | SlashCommand::Title
            | SlashCommand::Statusline
            | SlashCommand::Theme
            | SlashCommand::Pets => QueueDrain::Stop,
        }
    }

    fn slash_command_args_elements(
        rest: &str,
        rest_offset: usize,
        text_elements: &[TextElement],
    ) -> Vec<TextElement> {
        if rest.is_empty() || text_elements.is_empty() {
            return Vec::new();
        }
        text_elements
            .iter()
            .filter_map(|elem| {
                if elem.byte_range.end <= rest_offset {
                    return None;
                }
                let start = elem.byte_range.start.saturating_sub(rest_offset);
                let mut end = elem.byte_range.end.saturating_sub(rest_offset);
                if start >= rest.len() {
                    return None;
                }
                end = end.min(rest.len());
                (start < end).then_some(elem.map_range(|_| ByteRange { start, end }))
            })
            .collect()
    }

    fn ensure_slash_command_allowed_in_side_conversation(&mut self, cmd: SlashCommand) -> bool {
        if !self.active_side_conversation || cmd.available_in_side_conversation() {
            return true;
        }
        self.add_error_message(format!(
            "'/{}' is unavailable in side conversations. {SIDE_SLASH_COMMAND_UNAVAILABLE_HINT}",
            cmd.command()
        ));
        self.bottom_pane.drain_pending_submission_state();
        false
    }

    fn ensure_side_command_allowed_outside_review(&mut self, cmd: SlashCommand) -> bool {
        if !matches!(cmd, SlashCommand::Side | SlashCommand::Btw) || !self.review.is_review_mode {
            return true;
        }

        self.add_error_message(format!(
            "'/{}' is unavailable while code review is running.",
            cmd.command()
        ));
        self.bottom_pane.drain_pending_submission_state();
        false
    }
}

fn persist_outcomes_tracking_enabled(
    codex_home: &std::path::Path,
    enabled: bool,
) -> std::io::Result<()> {
    use std::io;
    use toml_edit::DocumentMut;

    let path = codex_home.join(codex_config::CONFIG_TOML_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    let mut document = text.parse::<DocumentMut>().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config.toml is invalid TOML: {err}"),
        )
    })?;
    document["scratchpad"]["outcomes_enabled"] = toml_edit::value(enabled);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(codex_config::CONFIG_TOML_FILE);
    let temp_path = path.with_file_name(format!(
        ".{file_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&temp_path, document.to_string())?;
    if cfg!(windows) && path.exists() {
        std::fs::remove_file(&path)?;
    }
    std::fs::rename(temp_path, path)
}

fn parse_scratchpad_absorb_args(
    args: &str,
) -> Result<(String, crate::legacy_core::ScratchpadAbsorbOptions), String> {
    let tokens = shlex::split(args)
        .filter(|tokens| !tokens.is_empty())
        .ok_or_else(|| SCRATCHPAD_ABSORB_USAGE.to_string())?;
    let mut iter = tokens.into_iter();
    let source_scratchpad_id = iter
        .next()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| SCRATCHPAD_ABSORB_USAGE.to_string())?;
    let mut options = crate::legacy_core::ScratchpadAbsorbOptions::default();
    for flag in iter {
        match flag.as_str() {
            "--exclude-completed" => options.include_completed = false,
            "--exclude-next-steps" => options.include_next_steps = false,
            "--exclude-pending" => options.include_pending_waits = false,
            "--exclude-blocked" => options.include_blocked = false,
            "--exclude-notes" => options.include_notes = false,
            "--exclude-git-refs" => options.include_git_refs = false,
            "--exclude-artifacts" => options.include_artifacts = false,
            "--exclude-outcomes" => options.include_outcomes = false,
            "--exclude-delegations" => options.include_delegations = false,
            "--exclude-worktrees" => options.include_worktrees = false,
            "--help" | "-h" => return Err(SCRATCHPAD_ABSORB_USAGE.to_string()),
            _ => {
                return Err(format!(
                    "Unknown /scratchpad-absorb option `{flag}`. {SCRATCHPAD_ABSORB_USAGE}"
                ));
            }
        }
    }
    Ok((source_scratchpad_id, options))
}

fn format_absorb_counts(counts: &std::collections::BTreeMap<String, usize>) -> String {
    if counts.is_empty() {
        return "no fields selected".to_string();
    }
    counts
        .iter()
        .map(|(field, count)| format!("{field} {count}"))
        .collect::<Vec<_>>()
        .join(", ")
}
