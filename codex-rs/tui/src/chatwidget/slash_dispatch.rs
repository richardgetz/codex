//! Slash-command dispatch and local-recall handoff for `ChatWidget`.
//!
//! `ChatComposer` parses slash input and stages recognized command text for local
//! Up-arrow recall before returning an input result. This module owns the app-level
//! dispatch step and records the staged entry once the command has been handled, so
//! slash-command recall follows the same submitted-input rule as ordinary text.

use super::*;
use crate::app_event::ThreadGoalSetMode;
use crate::bottom_pane::prompt_args::parse_slash_name;
use crate::bottom_pane::slash_commands;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlashCommandDispatchSource {
    Live,
    Queued,
}

struct PreparedSlashCommandArgs {
    args: String,
    text_elements: Vec<TextElement>,
    local_images: Vec<LocalImageAttachment>,
    remote_image_urls: Vec<String>,
    mention_bindings: Vec<MentionBinding>,
    source: SlashCommandDispatchSource,
}

const SIDE_STARTING_CONTEXT_LABEL: &str = "Side starting...";
const SIDE_REVIEW_UNAVAILABLE_MESSAGE: &str =
    "'/side' is unavailable while code review is running.";
const SIDE_SLASH_COMMAND_UNAVAILABLE_HINT: &str = "Press Esc to return to the main thread first.";
const GOAL_USAGE: &str = "Usage: /goal <objective>";
const GOAL_USAGE_HINT: &str = "Example: /goal improve benchmark coverage";
const CONTINUOUS_USAGE: &str = "Usage: /continuous [on|off|status]";
const OUTCOMES_USAGE: &str = "Usage: /outcomes [on|off|status]";

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
        completed: string_array_value(value.get("completed")),
        next_steps: string_array_value(value.get("next_steps")),
        pending_waits: value
            .get("pending_waits")
            .and_then(serde_json::Value::as_array)
            .map(|waits| waits.iter().map(format_pending_wait).collect())
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
    ["summary", "target", "wait_id", "reason", "next_check_at"]
        .iter()
        .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
        .unwrap_or("pending wait")
        .to_string()
}

fn outcomes_markdown(
    thread_id: ThreadId,
    objective: &str,
    outcomes: &[serde_json::Value],
) -> String {
    let mut lines = vec![
        format!("# Outcomes for {objective}"),
        String::new(),
        format!("Scratchpad: `{thread_id}`"),
        String::new(),
    ];
    if outcomes.is_empty() {
        lines.push("No outcomes recorded.".to_string());
        return lines.join("\n");
    }

    for outcome in outcomes {
        let object = outcome.as_object();
        let scope = object
            .and_then(|object| object.get("scope"))
            .map(format_outcome_value)
            .unwrap_or_else(|| "general".to_string());
        let metric = object
            .and_then(|object| object.get("metric"))
            .map(format_outcome_value)
            .unwrap_or_else(|| "metric".to_string());
        lines.push(format!("## {scope} - {metric}"));
        for key in [
            "baseline",
            "current",
            "delta",
            "unit",
            "summary",
            "tradeoffs",
            "commit",
            "pr",
            "recorded_at",
        ] {
            if let Some(value) = object.and_then(|object| object.get(key)) {
                lines.push(format!("- {key}: {}", format_outcome_value(value)));
            }
        }
        if let Some(provenance) = object.and_then(|object| object.get("provenance")) {
            lines.push(format!(
                "- provenance: {}",
                format_outcome_value(provenance)
            ));
        }
        lines.push(String::new());
    }

    lines.join("\n")
}

fn format_outcome_value(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| value.to_string())
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

    fn request_empty_side_conversation(&mut self) {
        let Some(parent_thread_id) = self.thread_id else {
            self.add_error_message("'/side' is unavailable before the session starts.".to_string());
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

    fn add_current_outcomes_output(&mut self) {
        let Some((thread_id, path)) = self.current_scratchpad_path() else {
            self.add_error_message(
                "'/outcomes' is unavailable before the session starts.".to_string(),
            );
            return;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.add_info_message(
                    format!("No outcomes are available because scratchpad `{thread_id}` does not exist yet."),
                    Some("Record outcomes with the built-in scratchpad record_outcome tool during measurable work.".to_string()),
                );
                return;
            }
            Err(err) => {
                self.add_error_message(format!(
                    "Could not read built-in scratchpad `{thread_id}`: {err}"
                ));
                return;
            }
        };
        let value = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => value,
            Err(err) => {
                self.add_error_message(format!(
                    "Built-in scratchpad `{thread_id}` is invalid JSON: {err}"
                ));
                return;
            }
        };
        if !scratchpad_value_matches_thread(&value, &thread_id) {
            self.add_error_message(format!(
                "Built-in scratchpad `{thread_id}` is owned by another thread and cannot be exported."
            ));
            return;
        }
        let objective = value
            .get("objective")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Scratchpad outcomes");
        let outcomes = value
            .get("outcomes")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let markdown = outcomes_markdown(thread_id, objective, &outcomes);
        self.add_to_history(history_cell::new_outcomes_export(markdown));
    }

    fn set_outcomes_tracking_enabled(&mut self, enabled: bool) {
        match persist_outcomes_tracking_enabled(&self.config.codex_home, enabled) {
            Ok(()) => {
                self.config.scratchpad.outcomes_enabled = enabled;
                self.submit_op(Op::ReloadUserConfig);
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

    fn dispatch_outcomes_command(&mut self, args: Option<&str>) {
        match args.map(str::trim).filter(|value| !value.is_empty()) {
            Some("on") => self.set_outcomes_tracking_enabled(/*enabled*/ true),
            Some("off") => self.set_outcomes_tracking_enabled(/*enabled*/ false),
            Some("status") => self.add_outcomes_tracking_status(),
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
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(serde_json::json!({
                "scratchpad_id": thread_id.to_string(),
                "origin_thread_id": thread_id.to_string(),
                "objective": "Session continuous run policy",
                "status": "active",
                "completed": [],
                "next_steps": [],
                "pending_waits": [],
                "run_policy": {},
                "communication_policy": {
                    "fallback": {
                        "final_response_on_channel_failure": false
                    }
                },
                "created_at": chrono::Utc::now().to_rfc3339(),
                "updated_at": chrono::Utc::now().to_rfc3339(),
                "archived_at": null
            })),
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
        if !self.submit_op(Op::SetScratchpadContinuousPolicy { enabled }) {
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

    pub(super) fn dispatch_command(&mut self, cmd: SlashCommand) {
        if !self.ensure_slash_command_allowed_in_side_conversation(cmd) {
            return;
        }
        if !self.ensure_side_command_allowed_outside_review(cmd) {
            return;
        }
        if !cmd.available_during_task() && self.bottom_pane.slash_command_task_running() {
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
                self.app_event_tx.send(AppEvent::NewSession);
            }
            SlashCommand::Clear => {
                self.app_event_tx.send(AppEvent::ClearUi);
            }
            SlashCommand::Resume => {
                self.app_event_tx.send(AppEvent::OpenResumePicker);
            }
            SlashCommand::Fork => {
                self.app_event_tx.send(AppEvent::ForkCurrentSession);
            }
            SlashCommand::Init => {
                let init_target = self.config.cwd.join(DEFAULT_AGENTS_MD_FILENAME);
                if init_target.exists() {
                    let message = format!(
                        "{DEFAULT_AGENTS_MD_FILENAME} already exists here. Skipping /init to avoid overwriting it."
                    );
                    self.add_info_message(message, /*hint*/ None);
                    return;
                }
                const INIT_PROMPT: &str = include_str!("../../prompt_for_init_command.md");
                self.submit_user_message(INIT_PROMPT.to_string().into());
            }
            SlashCommand::Compact => {
                self.clear_token_usage();
                if !self.bottom_pane.is_task_running() {
                    self.bottom_pane.set_task_running(/*running*/ true);
                }
                self.app_event_tx.compact();
            }
            SlashCommand::Review => {
                self.open_review_popup();
            }
            SlashCommand::Rename => {
                self.session_telemetry
                    .counter("codex.thread.rename", /*inc*/ 1, &[]);
                self.show_rename_prompt();
            }
            SlashCommand::Model => {
                self.open_model_popup();
            }
            SlashCommand::Fast => {
                let next_tier = if matches!(self.current_service_tier(), Some(ServiceTier::Fast)) {
                    None
                } else {
                    Some(ServiceTier::Fast)
                };
                self.set_service_tier_selection(next_tier);
            }
            SlashCommand::Realtime => {
                if !self.realtime_conversation_enabled() {
                    return;
                }
                if self.realtime_conversation.is_live() {
                    self.stop_realtime_conversation_from_ui();
                } else {
                    self.start_realtime_conversation();
                }
            }
            SlashCommand::Settings => {
                if !self.realtime_audio_device_selection_enabled() {
                    return;
                }
                self.open_realtime_audio_popup();
            }
            SlashCommand::Personality => {
                self.open_personality_popup();
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
                } else {
                    self.add_info_message(
                        GOAL_USAGE.to_string(),
                        Some(GOAL_USAGE_HINT.to_string()),
                    );
                }
            }
            SlashCommand::Collab => {
                if !self.collaboration_modes_enabled() {
                    self.add_info_message(
                        "Collaboration modes are disabled.".to_string(),
                        Some("Enable collaboration modes to use /collab.".to_string()),
                    );
                    return;
                }
                self.open_collaboration_modes_popup();
            }
            SlashCommand::Side => {
                self.request_empty_side_conversation();
            }
            SlashCommand::Agent | SlashCommand::MultiAgents => {
                self.app_event_tx.send(AppEvent::OpenAgentPicker);
            }
            SlashCommand::Approvals => {
                self.open_permissions_popup();
            }
            SlashCommand::Permissions => {
                self.open_permissions_popup();
            }
            SlashCommand::Keymap => {
                self.open_keymap_picker();
            }
            SlashCommand::ElevateSandbox => {
                #[cfg(target_os = "windows")]
                {
                    let windows_sandbox_level = WindowsSandboxLevel::from_config(&self.config);
                    let windows_degraded_sandbox_enabled =
                        matches!(windows_sandbox_level, WindowsSandboxLevel::RestrictedToken);
                    if !windows_degraded_sandbox_enabled
                        || !crate::legacy_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
                    {
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
                        .send(AppEvent::BeginWindowsSandboxElevatedSetup { preset });
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
            // SlashCommand::Undo => {
            //     self.app_event_tx.send(AppEvent::CodexOp(Op::Undo));
            // }
            SlashCommand::Copy => {
                self.copy_last_agent_markdown();
            }
            SlashCommand::Diff => {
                self.add_diff_in_progress();
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    let text = match get_git_diff().await {
                        Ok((is_git_repo, diff_text)) => {
                            if is_git_repo {
                                diff_text
                            } else {
                                "`/diff` — _not inside a git repository_".to_string()
                            }
                        }
                        Err(e) => format!("Failed to compute diff: {e}"),
                    };
                    tx.send(AppEvent::DiffResult(text));
                });
            }
            SlashCommand::Mention => {
                self.insert_str("@");
            }
            SlashCommand::Skills => {
                self.open_skills_menu();
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
                self.submit_op(Op::ConsolidateOrchestratorMemory);
            }
            SlashCommand::Scratchpad => {
                self.add_current_scratchpad_output();
            }
            SlashCommand::Outcomes => {
                self.dispatch_outcomes_command(/*args*/ None);
            }
            SlashCommand::Continuous => {
                self.dispatch_continuous_command(/*args*/ None);
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

                use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
                use codex_protocol::protocol::FileChange;

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
        if !cmd.available_during_task() && self.bottom_pane.slash_command_task_running() {
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
            local_images,
            remote_image_urls,
            mention_bindings,
            source,
        } = prepared;
        let trimmed = args.trim();
        match cmd {
            SlashCommand::Fast => {
                match trimmed.to_ascii_lowercase().as_str() {
                    "on" => self.set_service_tier_selection(Some(ServiceTier::Fast)),
                    "off" => self.set_service_tier_selection(/*service_tier*/ None),
                    "status" => {
                        let status =
                            if matches!(self.current_service_tier(), Some(ServiceTier::Fast)) {
                                "on"
                            } else {
                                "off"
                            };
                        self.add_info_message(
                            format!("Fast mode is {status}."),
                            /*hint*/ None,
                        );
                    }
                    _ => {
                        self.add_error_message("Usage: /fast [on|off|status]".to_string());
                    }
                }
            }
            SlashCommand::Mcp => match trimmed.to_ascii_lowercase().as_str() {
                "verbose" => self.add_mcp_output(McpServerStatusDetail::Full),
                _ => self.add_error_message("Usage: /mcp [verbose]".to_string()),
            },
            SlashCommand::Continuous => {
                self.dispatch_continuous_command(Some(trimmed));
            }
            SlashCommand::Outcomes => {
                self.dispatch_outcomes_command(Some(trimmed));
            }
            SlashCommand::OrchestratorMemoryForget if !trimmed.is_empty() => {
                let tx = self.app_event_tx.clone();
                let needle = args.clone();
                let codex_home = self.config.codex_home.clone();
                let memory_config = self.config.orchestrator_memory.clone();
                tokio::spawn(async move {
                    let result =
                        crate::legacy_core::prune_orchestrator_memory_entries_matching_needle(
                            &codex_home,
                            &memory_config,
                            &needle,
                        )
                        .await
                        .map_err(|err| err.to_string());
                    tx.send(AppEvent::OrchestratorMemoryForgetResult { needle, result });
                });
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
            SlashCommand::Rename if !trimmed.is_empty() => {
                if !self.ensure_thread_rename_allowed() {
                    return;
                }
                self.session_telemetry
                    .counter("codex.thread.rename", /*inc*/ 1, &[]);
                let Some(name) = crate::legacy_core::util::normalize_thread_name(&args) else {
                    self.add_error_message("Thread name cannot be empty.".to_string());
                    return;
                };
                self.app_event_tx.set_thread_name(name);
            }
            SlashCommand::Plan if !trimmed.is_empty() => {
                if !self.apply_plan_slash_command() {
                    return;
                }
                let user_message = self.prepared_inline_user_message(
                    args,
                    text_elements,
                    local_images,
                    remote_image_urls,
                    mention_bindings,
                    source,
                );
                if self.is_session_configured() {
                    self.reasoning_buffer.clear();
                    self.full_reasoning_buffer.clear();
                    self.set_status_header(String::from("Working"));
                    self.submit_user_message(user_message);
                } else {
                    self.queue_user_message(user_message);
                }
            }
            SlashCommand::Goal if !trimmed.is_empty() => {
                if !self.config.features.enabled(Feature::Goals) {
                    return;
                }
                enum GoalControlCommand {
                    Clear,
                    SetStatus(AppThreadGoalStatus),
                }
                let control_command = match trimmed.to_ascii_lowercase().as_str() {
                    "clear" => Some(GoalControlCommand::Clear),
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
                    if source == SlashCommandDispatchSource::Live {
                        self.bottom_pane.drain_pending_submission_state();
                    }
                    return;
                }
                let objective = args.trim();
                if objective.is_empty() {
                    self.add_error_message("Goal objective must not be empty.".to_string());
                    self.add_info_message(
                        GOAL_USAGE.to_string(),
                        Some(GOAL_USAGE_HINT.to_string()),
                    );
                    if source == SlashCommandDispatchSource::Live {
                        self.bottom_pane.drain_pending_submission_state();
                    }
                    return;
                }
                let Some(thread_id) = self.thread_id else {
                    if source == SlashCommandDispatchSource::Live {
                        self.queue_user_message_with_options(
                            UserMessage {
                                text: format!("/goal {args}"),
                                local_images: Vec::new(),
                                remote_image_urls: Vec::new(),
                                text_elements: Vec::new(),
                                mention_bindings: Vec::new(),
                            },
                            QueuedInputAction::ParseSlash,
                        );
                        self.bottom_pane.drain_pending_submission_state();
                    } else {
                        self.add_info_message(
                            GOAL_USAGE.to_string(),
                            Some("The session must start before you can set a goal.".to_string()),
                        );
                    }
                    return;
                };
                self.app_event_tx.send(AppEvent::SetThreadGoalObjective {
                    thread_id,
                    objective: objective.to_string(),
                    mode: ThreadGoalSetMode::ConfirmIfExists,
                });
                if source == SlashCommandDispatchSource::Live {
                    self.bottom_pane.drain_pending_submission_state();
                }
            }
            SlashCommand::Side if !trimmed.is_empty() => {
                let Some(parent_thread_id) = self.thread_id else {
                    self.add_error_message(
                        "'/side' is unavailable before the session starts.".to_string(),
                    );
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
                self.submit_op(AppCommand::review(ReviewRequest {
                    target: ReviewTarget::Custom { instructions: args },
                    user_facing_hint: None,
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
            _ => self.dispatch_command(cmd),
        }
        if source == SlashCommandDispatchSource::Live && cmd != SlashCommand::Goal {
            self.bottom_pane.drain_pending_submission_state();
        }
    }

    pub(super) fn submit_queued_slash_prompt(&mut self, user_message: UserMessage) -> QueueDrain {
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

        let Some(cmd) = slash_commands::find_builtin_command(name, self.builtin_command_flags())
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
            self.dispatch_command(cmd);
            return self.queued_command_drain_result(cmd);
        }

        if !cmd.supports_inline_args() {
            self.submit_user_message(UserMessage {
                text,
                local_images,
                remote_image_urls,
                text_elements,
                mention_bindings,
            });
            return QueueDrain::Stop;
        }

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
                local_images,
                remote_image_urls,
                mention_bindings,
                source: SlashCommandDispatchSource::Queued,
            },
        );
        self.queued_command_drain_result(cmd)
    }

    fn builtin_command_flags(&self) -> slash_commands::BuiltinCommandFlags {
        #[cfg(target_os = "windows")]
        let allow_elevate_sandbox = {
            let windows_sandbox_level = WindowsSandboxLevel::from_config(&self.config);
            matches!(windows_sandbox_level, WindowsSandboxLevel::RestrictedToken)
        };
        #[cfg(not(target_os = "windows"))]
        let allow_elevate_sandbox = false;

        slash_commands::BuiltinCommandFlags {
            collaboration_modes_enabled: self.collaboration_modes_enabled(),
            connectors_enabled: self.connectors_enabled(),
            plugins_command_enabled: self.config.features.enabled(Feature::Plugins),
            goal_command_enabled: self.config.features.enabled(Feature::Goals),
            fast_command_enabled: self.fast_mode_enabled(),
            personality_command_enabled: self.config.features.enabled(Feature::Personality),
            realtime_conversation_enabled: self.realtime_conversation_enabled(),
            audio_device_selection_enabled: self.realtime_audio_device_selection_enabled(),
            allow_elevate_sandbox,
            side_conversation_active: self.active_side_conversation,
        }
    }

    fn queued_command_drain_result(&self, cmd: SlashCommand) -> QueueDrain {
        if self.is_user_turn_pending_or_running() || !self.bottom_pane.no_modal_or_popup_active() {
            return QueueDrain::Stop;
        }
        match cmd {
            SlashCommand::Fast
            | SlashCommand::Status
            | SlashCommand::DebugConfig
            | SlashCommand::Ps
            | SlashCommand::Stop
            | SlashCommand::MemoryDrop
            | SlashCommand::MemoryUpdate
            | SlashCommand::Mcp
            | SlashCommand::OrchestratorMemoryForget
            | SlashCommand::OrchestratorMemoryConsolidate
            | SlashCommand::Scratchpad
            | SlashCommand::Outcomes
            | SlashCommand::Continuous
            | SlashCommand::Account
            | SlashCommand::Apps
            | SlashCommand::Plugins
            | SlashCommand::Rollout
            | SlashCommand::Copy
            | SlashCommand::Diff
            | SlashCommand::Rename
            | SlashCommand::TestApproval => QueueDrain::Continue,
            SlashCommand::Feedback
            | SlashCommand::New
            | SlashCommand::Clear
            | SlashCommand::Resume
            | SlashCommand::Fork
            | SlashCommand::Init
            | SlashCommand::Compact
            | SlashCommand::Review
            | SlashCommand::Model
            | SlashCommand::Realtime
            | SlashCommand::Settings
            | SlashCommand::Personality
            | SlashCommand::Plan
            | SlashCommand::Goal
            | SlashCommand::Collab
            | SlashCommand::Side
            | SlashCommand::Keymap
            | SlashCommand::Agent
            | SlashCommand::MultiAgents
            | SlashCommand::Approvals
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
            | SlashCommand::Title
            | SlashCommand::Statusline
            | SlashCommand::Theme => QueueDrain::Stop,
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
        if cmd != SlashCommand::Side || !self.is_review_mode {
            return true;
        }

        self.add_error_message(SIDE_REVIEW_UNAVAILABLE_MESSAGE.to_string());
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
