//! Background app-server requests launched by the TUI app.
//!
//! This module owns fire-and-forget fetch/write helpers for MCP inventory, skills, plugins, rate
//! limits, add-credit nudges, and feedback uploads. Results are routed back through `AppEvent` so
//! the main event loop remains single-threaded.

use chrono::Datelike;
use chrono::Local;
use chrono::Timelike;
use chrono::Weekday;
use codex_config::types::OrchestratorPrimaryContactScheduleToml;

use super::*;

impl App {
    pub(super) fn ensure_primary_contact_polling(&mut self, app_server: &AppServerSession) {
        let Some((thread_id, poll_config)) = self.primary_contact_polling_candidate() else {
            self.stop_primary_contact_polling();
            return;
        };

        if self
            .primary_contact_polling
            .as_ref()
            .is_some_and(|state| state.thread_id == thread_id)
        {
            self.chat_widget
                .set_primary_contact_waiting(/*waiting*/ true);
            return;
        }

        self.stop_primary_contact_polling();
        self.start_primary_contact_polling(app_server, thread_id, poll_config);
    }

    pub(super) fn primary_contact_polling_candidate(
        &self,
    ) -> Option<(ThreadId, PrimaryContactPollConfig)> {
        if self.chat_widget.active_collaboration_mode_kind() != ModeKind::Orchestrator {
            return None;
        }
        let poll_config = PrimaryContactPollConfig::from_config(&self.config)?;
        let thread_id = self.primary_thread_id?;
        Some((thread_id, poll_config))
    }

    fn stop_primary_contact_polling(&mut self) {
        if let Some(state) = self.primary_contact_polling.take() {
            state.task.abort();
        }
        self.chat_widget
            .set_primary_contact_waiting(/*waiting*/ false);
    }

    fn start_primary_contact_polling(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        poll_config: PrimaryContactPollConfig,
    ) {
        self.chat_widget
            .set_primary_contact_waiting(/*waiting*/ true);
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        let task = tokio::spawn(async move {
            loop {
                match poll_primary_contact_once(&request_handle, thread_id, &poll_config).await {
                    Ok(Some(text)) => {
                        app_event_tx.send(AppEvent::PrimaryContactMessageReceived { text });
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(
                            mcp = %poll_config.mcp,
                            tool = %poll_config.tool,
                            "primary contact poll failed: {err:#}"
                        );
                    }
                }

                tokio::time::sleep(Duration::from_secs(u64::from(
                    poll_config.interval_seconds_now(),
                )))
                .await;
            }
        });
        self.primary_contact_polling = Some(PrimaryContactPollingState { thread_id, task });
    }

    pub(super) fn fetch_mcp_inventory(
        &mut self,
        app_server: &AppServerSession,
        detail: McpServerStatusDetail,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = fetch_all_mcp_server_statuses(request_handle, detail)
                .await
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::McpInventoryLoaded { result, detail });
        });
    }

    /// Spawns a background task to fetch account rate limits and deliver the
    /// result as a `RateLimitsLoaded` event.
    ///
    /// The `origin` is forwarded to the completion handler so it can distinguish
    /// a startup prefetch (which only updates cached snapshots and schedules a
    /// frame) from a `/status`-triggered refresh (which must finalize the
    /// corresponding status card).
    pub(super) fn refresh_rate_limits(
        &mut self,
        app_server: &AppServerSession,
        origin: RateLimitRefreshOrigin,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = fetch_account_rate_limits(request_handle)
                .await
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::RateLimitsLoaded { origin, result });
        });
    }

    pub(super) fn send_add_credits_nudge_email(
        &mut self,
        app_server: &AppServerSession,
        credit_type: AddCreditsNudgeCreditType,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = send_add_credits_nudge_email(request_handle, credit_type)
                .await
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::AddCreditsNudgeEmailFinished { result });
        });
    }

    /// Starts the initial skills refresh without delaying the first interactive frame.
    ///
    /// Startup only needs skill metadata to populate skill mentions and the skills UI; the prompt can be
    /// rendered before that metadata arrives. The result is routed through the normal app event queue so
    /// the same response handler updates the chat widget and emits invalid `SKILL.md` warnings once the
    /// app-server RPC finishes. User-initiated skills refreshes still use the blocking app command path so
    /// callers that explicitly asked for fresh skill state do not race ahead of their own refresh.
    pub(super) fn refresh_startup_skills(&mut self, app_server: &AppServerSession) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        let cwd = self.config.cwd.to_path_buf();
        tokio::spawn(async move {
            let result = fetch_skills_list(request_handle, cwd)
                .await
                .map_err(|err| format!("{err:#}"));
            app_event_tx.send(AppEvent::SkillsListLoaded { result });
        });
    }

    pub(super) fn fetch_plugins_list(&mut self, app_server: &AppServerSession, cwd: PathBuf) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = fetch_plugins_list(request_handle, cwd.clone())
                .await
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::PluginsLoaded { cwd, result });
        });
    }

    pub(super) fn fetch_plugin_detail(
        &mut self,
        app_server: &AppServerSession,
        cwd: PathBuf,
        params: PluginReadParams,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = fetch_plugin_detail(request_handle, params)
                .await
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::PluginDetailLoaded { cwd, result });
        });
    }

    pub(super) fn fetch_plugin_install(
        &mut self,
        app_server: &AppServerSession,
        cwd: PathBuf,
        marketplace_path: AbsolutePathBuf,
        plugin_name: String,
        plugin_display_name: String,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let cwd_for_event = cwd.clone();
            let marketplace_path_for_event = marketplace_path.clone();
            let plugin_name_for_event = plugin_name.clone();
            let result = fetch_plugin_install(request_handle, marketplace_path, plugin_name)
                .await
                .map_err(|err| format!("Failed to install plugin: {err}"));
            app_event_tx.send(AppEvent::PluginInstallLoaded {
                cwd: cwd_for_event,
                marketplace_path: marketplace_path_for_event,
                plugin_name: plugin_name_for_event,
                plugin_display_name,
                result,
            });
        });
    }

    pub(super) fn fetch_plugin_uninstall(
        &mut self,
        app_server: &AppServerSession,
        cwd: PathBuf,
        plugin_id: String,
        plugin_display_name: String,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let cwd_for_event = cwd.clone();
            let plugin_id_for_event = plugin_id.clone();
            let result = fetch_plugin_uninstall(request_handle, plugin_id)
                .await
                .map_err(|err| format!("Failed to uninstall plugin: {err}"));
            app_event_tx.send(AppEvent::PluginUninstallLoaded {
                cwd: cwd_for_event,
                plugin_id: plugin_id_for_event,
                plugin_display_name,
                result,
            });
        });
    }

    pub(super) fn set_plugin_enabled(
        &mut self,
        app_server: &AppServerSession,
        cwd: PathBuf,
        plugin_id: String,
        enabled: bool,
    ) {
        if let Some(queued_enabled) = self.pending_plugin_enabled_writes.get_mut(&plugin_id) {
            *queued_enabled = Some(enabled);
            return;
        }

        self.pending_plugin_enabled_writes
            .insert(plugin_id.clone(), None);
        self.spawn_plugin_enabled_write(app_server, cwd, plugin_id, enabled);
    }

    pub(super) fn spawn_plugin_enabled_write(
        &mut self,
        app_server: &AppServerSession,
        cwd: PathBuf,
        plugin_id: String,
        enabled: bool,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let cwd_for_event = cwd.clone();
            let plugin_id_for_event = plugin_id.clone();
            let result = write_plugin_enabled(request_handle, plugin_id, enabled)
                .await
                .map(|_| ())
                .map_err(|err| format!("Failed to update plugin config: {err}"));
            app_event_tx.send(AppEvent::PluginEnabledSet {
                cwd: cwd_for_event,
                plugin_id: plugin_id_for_event,
                enabled,
                result,
            });
        });
    }

    pub(super) fn refresh_plugin_mentions(&mut self) {
        let config = self.config.clone();
        let app_event_tx = self.app_event_tx.clone();
        if !config.features.enabled(Feature::Plugins) {
            app_event_tx.send(AppEvent::PluginMentionsLoaded { plugins: None });
            return;
        }

        tokio::spawn(async move {
            let plugins = PluginsManager::new(config.codex_home.to_path_buf())
                .plugins_for_config(&config)
                .await
                .capability_summaries()
                .to_vec();
            app_event_tx.send(AppEvent::PluginMentionsLoaded {
                plugins: Some(plugins),
            });
        });
    }

    pub(super) fn submit_feedback(
        &mut self,
        app_server: &AppServerSession,
        category: FeedbackCategory,
        reason: Option<String>,
        turn_id: Option<String>,
        include_logs: bool,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        let origin_thread_id = self.chat_widget.thread_id();
        let rollout_path = if include_logs {
            self.chat_widget.rollout_path()
        } else {
            None
        };
        let params = build_feedback_upload_params(
            origin_thread_id,
            rollout_path,
            category,
            reason,
            turn_id,
            include_logs,
        );
        tokio::spawn(async move {
            let result = fetch_feedback_upload(request_handle, params)
                .await
                .map(|response| response.thread_id)
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::FeedbackSubmitted {
                origin_thread_id,
                category,
                include_logs,
                result,
            });
        });
    }

    pub(super) fn handle_feedback_thread_event(&mut self, event: FeedbackThreadEvent) {
        match event.result {
            Ok(thread_id) => {
                self.chat_widget
                    .add_to_history(crate::bottom_pane::feedback_success_cell(
                        event.category,
                        event.include_logs,
                        &thread_id,
                        event.feedback_audience,
                    ))
            }
            Err(err) => self
                .chat_widget
                .add_to_history(history_cell::new_error_event(format!(
                    "Failed to upload feedback: {err}"
                ))),
        }
    }

    pub(super) async fn enqueue_thread_feedback_event(
        &mut self,
        thread_id: ThreadId,
        event: FeedbackThreadEvent,
    ) {
        let (sender, store) = {
            let channel = self.ensure_thread_channel(thread_id);
            (channel.sender.clone(), Arc::clone(&channel.store))
        };

        let should_send = {
            let mut guard = store.lock().await;
            guard
                .buffer
                .push_back(ThreadBufferedEvent::FeedbackSubmission(event.clone()));
            if guard.buffer.len() > guard.capacity
                && let Some(removed) = guard.buffer.pop_front()
                && let ThreadBufferedEvent::Request(request) = &removed
            {
                guard
                    .pending_interactive_replay
                    .note_evicted_server_request(request);
            }
            guard.active
        };

        if should_send {
            match sender.try_send(ThreadBufferedEvent::FeedbackSubmission(event)) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    tokio::spawn(async move {
                        if let Err(err) = sender.send(event).await {
                            tracing::warn!("thread {thread_id} event channel closed: {err}");
                        }
                    });
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::warn!("thread {thread_id} event channel closed");
                }
            }
        }
    }

    pub(super) async fn handle_feedback_submitted(
        &mut self,
        origin_thread_id: Option<ThreadId>,
        category: FeedbackCategory,
        include_logs: bool,
        result: Result<String, String>,
    ) {
        let event = FeedbackThreadEvent {
            category,
            include_logs,
            feedback_audience: self.feedback_audience,
            result,
        };
        if let Some(thread_id) = origin_thread_id {
            self.enqueue_thread_feedback_event(thread_id, event).await;
        } else {
            self.handle_feedback_thread_event(event);
        }
    }

    /// Process the completed MCP inventory fetch: clear the loading spinner, then
    /// render either the full tool/resource listing or an error into chat history.
    ///
    /// When both the local config and the app-server report zero servers, a special
    /// "empty" cell is shown instead of the full table.
    pub(super) fn handle_mcp_inventory_result(
        &mut self,
        result: Result<Vec<McpServerStatus>, String>,
        detail: McpServerStatusDetail,
    ) {
        let config = self.chat_widget.config_ref().clone();
        self.chat_widget.clear_mcp_inventory_loading();
        self.clear_committed_mcp_inventory_loading();

        let statuses = match result {
            Ok(statuses) => statuses,
            Err(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to load MCP inventory: {err}"));
                return;
            }
        };

        if config.mcp_servers.get().is_empty() && statuses.is_empty() {
            self.chat_widget
                .add_to_history(history_cell::empty_mcp_output());
            return;
        }

        self.chat_widget
            .add_to_history(history_cell::new_mcp_tools_output_from_statuses(
                &config,
                &statuses,
                detail,
                self.chat_widget.active_collaboration_mode_kind(),
            ));
    }

    pub(super) fn clear_committed_mcp_inventory_loading(&mut self) {
        let Some(index) = self
            .transcript_cells
            .iter()
            .rposition(|cell| cell.as_any().is::<history_cell::McpInventoryLoadingCell>())
        else {
            return;
        };

        self.transcript_cells.remove(index);
        if let Some(Overlay::Transcript(overlay)) = &mut self.overlay {
            overlay.replace_cells(self.transcript_cells.clone());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PrimaryContactPollConfig {
    mcp: String,
    tool: String,
    arguments: Option<serde_json::Value>,
    default_interval_seconds: u32,
    schedule: Vec<PrimaryContactPollSchedule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrimaryContactPollSchedule {
    days: Option<Vec<Weekday>>,
    start_minute: u16,
    end_minute: u16,
    interval_seconds: u32,
}

impl PrimaryContactPollConfig {
    fn from_config(config: &Config) -> Option<Self> {
        let primary_contact = &config.orchestrator.primary_contact;
        if !primary_contact.enabled || primary_contact.check_messages_every_seconds == 0 {
            return None;
        }
        let mcp = primary_contact.mcp.as_ref()?.trim();
        if mcp.is_empty() {
            return None;
        }
        let tool = primary_contact
            .check_tool
            .as_deref()
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_primary_contact_check_tool(mcp));
        Some(Self {
            mcp: mcp.to_string(),
            tool,
            arguments: default_primary_contact_check_arguments(mcp),
            default_interval_seconds: primary_contact.check_messages_every_seconds,
            schedule: primary_contact
                .schedule
                .iter()
                .filter_map(PrimaryContactPollSchedule::from_config)
                .collect(),
        })
    }

    fn interval_seconds_now(&self) -> u32 {
        let now = Local::now();
        self.interval_seconds_at(
            now.weekday(),
            (now.hour() * 60 + now.minute())
                .try_into()
                .unwrap_or(/*default*/ 0),
        )
    }

    fn interval_seconds_at(&self, weekday: Weekday, minute_of_day: u16) -> u32 {
        self.schedule
            .iter()
            .find_map(|schedule| schedule.interval_seconds_at(weekday, minute_of_day))
            .unwrap_or(self.default_interval_seconds)
    }
}

impl PrimaryContactPollSchedule {
    fn from_config(toml: &OrchestratorPrimaryContactScheduleToml) -> Option<Self> {
        let start_minute = parse_local_time_minutes(toml.start.as_deref()?)?;
        let end_minute = parse_local_time_minutes(toml.end.as_deref()?)?;
        let interval_seconds = toml.check_messages_every_seconds?;
        if interval_seconds == 0 || start_minute == end_minute {
            return None;
        }
        let days = parse_schedule_days(toml.days.as_deref())?;
        Some(Self {
            days,
            start_minute,
            end_minute,
            interval_seconds,
        })
    }

    fn interval_seconds_at(&self, weekday: Weekday, minute_of_day: u16) -> Option<u32> {
        let active_day = if self.start_minute < self.end_minute {
            (self.start_minute..self.end_minute)
                .contains(&minute_of_day)
                .then_some(weekday)?
        } else if minute_of_day >= self.start_minute {
            weekday
        } else if minute_of_day < self.end_minute {
            previous_weekday(weekday)
        } else {
            return None;
        };
        if self
            .days
            .as_ref()
            .is_none_or(|days| days.contains(&active_day))
        {
            Some(self.interval_seconds)
        } else {
            None
        }
    }
}

fn parse_local_time_minutes(value: &str) -> Option<u16> {
    let (hour, minute) = value.trim().split_once(':')?;
    let hour = hour.parse::<u16>().ok()?;
    let minute = minute.parse::<u16>().ok()?;
    if hour < 24 && minute < 60 {
        Some(hour * 60 + minute)
    } else {
        None
    }
}

fn parse_schedule_days(days: Option<&[String]>) -> Option<Option<Vec<Weekday>>> {
    let Some(days) = days else {
        return Some(None);
    };
    let mut parsed = Vec::new();
    for day in days {
        match day.trim().to_ascii_lowercase().as_str() {
            "" | "*" | "all" | "any" | "daily" | "everyday" => return Some(None),
            "weekday" | "weekdays" => {
                parsed.extend([
                    Weekday::Mon,
                    Weekday::Tue,
                    Weekday::Wed,
                    Weekday::Thu,
                    Weekday::Fri,
                ]);
            }
            "weekend" | "weekends" => {
                parsed.extend([Weekday::Sat, Weekday::Sun]);
            }
            "mon" | "monday" => parsed.push(Weekday::Mon),
            "tue" | "tues" | "tuesday" => parsed.push(Weekday::Tue),
            "wed" | "weds" | "wednesday" => parsed.push(Weekday::Wed),
            "thu" | "thur" | "thurs" | "thursday" => parsed.push(Weekday::Thu),
            "fri" | "friday" => parsed.push(Weekday::Fri),
            "sat" | "saturday" => parsed.push(Weekday::Sat),
            "sun" | "sunday" => parsed.push(Weekday::Sun),
            _ => return None,
        }
    }
    parsed.sort_by_key(Weekday::num_days_from_monday);
    parsed.dedup();
    if parsed.is_empty() {
        Some(None)
    } else {
        Some(Some(parsed))
    }
}

fn previous_weekday(weekday: Weekday) -> Weekday {
    match weekday {
        Weekday::Mon => Weekday::Sun,
        Weekday::Tue => Weekday::Mon,
        Weekday::Wed => Weekday::Tue,
        Weekday::Thu => Weekday::Wed,
        Weekday::Fri => Weekday::Thu,
        Weekday::Sat => Weekday::Fri,
        Weekday::Sun => Weekday::Sat,
    }
}

fn default_primary_contact_check_tool(mcp: &str) -> String {
    if mcp.eq_ignore_ascii_case("imessage") {
        "imessage_followup_wait_for_reply".to_string()
    } else {
        format!("{mcp}_followup_status")
    }
}

fn default_primary_contact_check_arguments(mcp: &str) -> Option<serde_json::Value> {
    if mcp.eq_ignore_ascii_case("imessage") {
        Some(serde_json::json!({
            "max_polls": 1,
            "block_until_reply": false
        }))
    } else {
        None
    }
}

async fn poll_primary_contact_once(
    request_handle: &AppServerRequestHandle,
    thread_id: ThreadId,
    poll_config: &PrimaryContactPollConfig,
) -> Result<Option<String>> {
    let request_id = RequestId::String(format!("primary-contact-poll-{}", Uuid::new_v4()));
    let response: McpServerToolCallResponse = request_handle
        .request_typed(ClientRequest::McpServerToolCall {
            request_id,
            params: McpServerToolCallParams {
                thread_id: thread_id.to_string(),
                server: poll_config.mcp.clone(),
                tool: poll_config.tool.clone(),
                arguments: poll_config.arguments.clone(),
                meta: None,
            },
        })
        .await
        .wrap_err("mcpServer/tool/call failed during primary contact poll")?;

    Ok(extract_primary_contact_message(&response))
}

fn extract_primary_contact_message(response: &McpServerToolCallResponse) -> Option<String> {
    if response.is_error == Some(true) {
        return None;
    }

    if let Some(value) = response.structured_content.as_ref()
        && let Some(message) = extract_message_from_value(value)
    {
        return Some(message);
    }

    response.content.iter().find_map(extract_message_from_value)
}

fn extract_message_from_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if explicit_no_new_message(map) {
                if let Some(message) = ambiguous_primary_contact_message(map) {
                    return Some(message);
                }
                return None;
            }
            let explicit_new_message = explicit_new_message(map);
            if let Some(messages) = map.get("messages").and_then(serde_json::Value::as_array)
                && let Some(message) = messages.iter().rev().find_map(extract_message_from_value)
            {
                return Some(message);
            }
            for key in ["user_message", "message", "reply_text", "raw_reply_text"] {
                if let Some(message) = map.get(key).and_then(extract_text_field)
                    && !message.is_empty()
                {
                    return Some(message);
                }
            }
            if explicit_new_message {
                for key in ["text", "body", "content"] {
                    if let Some(message) = map.get(key).and_then(extract_text_field)
                        && !message.is_empty()
                    {
                        return Some(message);
                    }
                }
            }
            None
        }
        serde_json::Value::Array(values) => {
            values.iter().rev().find_map(extract_message_from_value)
        }
        _ => None,
    }
}

fn ambiguous_primary_contact_message(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    if map.get("ambiguous").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    let match_reason = map
        .get("match_reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if !matches!(
        match_reason,
        "conflicting_reply_target" | "conflicting_session_ids" | "single_untagged_reply"
    ) {
        return None;
    }
    map.get("raw_reply_text")
        .and_then(extract_text_field)
        .filter(|message| !message.is_empty())
}

fn explicit_no_new_message(map: &serde_json::Map<String, serde_json::Value>) -> bool {
    [
        "has_new_message",
        "has_message",
        "new_message",
        "available",
        "reply_received",
    ]
    .iter()
    .any(|key| map.get(*key).and_then(serde_json::Value::as_bool) == Some(false))
}

fn explicit_new_message(map: &serde_json::Map<String, serde_json::Value>) -> bool {
    [
        "has_new_message",
        "has_message",
        "new_message",
        "available",
        "reply_received",
    ]
    .iter()
    .any(|key| map.get(*key).and_then(serde_json::Value::as_bool) == Some(true))
}

fn extract_text_field(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => {
            let text = text.trim().to_string();
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
                && let Some(message) = extract_message_from_value(&value)
            {
                return Some(message);
            }
            (!text.is_empty()).then_some(text)
        }
        serde_json::Value::Object(_) => extract_message_from_value(value),
        _ => None,
    }
}

pub(super) async fn fetch_all_mcp_server_statuses(
    request_handle: AppServerRequestHandle,
    detail: McpServerStatusDetail,
) -> Result<Vec<McpServerStatus>> {
    let mut cursor = None;
    let mut statuses = Vec::new();

    loop {
        let request_id = RequestId::String(format!("mcp-inventory-{}", Uuid::new_v4()));
        let response: ListMcpServerStatusResponse = request_handle
            .request_typed(ClientRequest::McpServerStatusList {
                request_id,
                params: ListMcpServerStatusParams {
                    cursor: cursor.clone(),
                    limit: Some(100),
                    detail: Some(detail),
                },
            })
            .await
            .wrap_err("mcpServerStatus/list failed in TUI")?;
        statuses.extend(response.data);
        if let Some(next_cursor) = response.next_cursor {
            cursor = Some(next_cursor);
        } else {
            break;
        }
    }

    Ok(statuses)
}

pub(super) async fn fetch_account_rate_limits(
    request_handle: AppServerRequestHandle,
) -> Result<Vec<RateLimitSnapshot>> {
    let request_id = RequestId::String(format!("account-rate-limits-{}", Uuid::new_v4()));
    let response: GetAccountRateLimitsResponse = request_handle
        .request_typed(ClientRequest::GetAccountRateLimits {
            request_id,
            params: None,
        })
        .await
        .wrap_err("account/rateLimits/read failed in TUI")?;

    Ok(app_server_rate_limit_snapshots_to_core(response))
}

pub(super) async fn send_add_credits_nudge_email(
    request_handle: AppServerRequestHandle,
    credit_type: AddCreditsNudgeCreditType,
) -> Result<codex_app_server_protocol::AddCreditsNudgeEmailStatus> {
    let request_id = RequestId::String(format!("add-credits-nudge-{}", Uuid::new_v4()));
    let response: codex_app_server_protocol::SendAddCreditsNudgeEmailResponse = request_handle
        .request_typed(ClientRequest::SendAddCreditsNudgeEmail {
            request_id,
            params: SendAddCreditsNudgeEmailParams { credit_type },
        })
        .await
        .wrap_err("account/sendAddCreditsNudgeEmail failed in TUI")?;

    Ok(response.status)
}

pub(super) async fn fetch_skills_list(
    request_handle: AppServerRequestHandle,
    cwd: PathBuf,
) -> Result<SkillsListResponse> {
    let request_id = RequestId::String(format!("startup-skills-list-{}", Uuid::new_v4()));
    // Use the cloneable request handle so startup can issue this RPC from a background task without
    // extending a borrow of `AppServerSession` across the first frame render.
    request_handle
        .request_typed(ClientRequest::SkillsList {
            request_id,
            params: SkillsListParams {
                cwds: vec![cwd],
                force_reload: true,
                per_cwd_extra_user_roots: None,
            },
        })
        .await
        .wrap_err("skills/list failed in TUI")
}

pub(super) async fn fetch_plugins_list(
    request_handle: AppServerRequestHandle,
    cwd: PathBuf,
) -> Result<PluginListResponse> {
    let cwd = AbsolutePathBuf::try_from(cwd).wrap_err("plugin list cwd must be absolute")?;
    let request_id = RequestId::String(format!("plugin-list-{}", Uuid::new_v4()));
    let mut response = request_handle
        .request_typed(ClientRequest::PluginList {
            request_id,
            params: PluginListParams {
                cwds: Some(vec![cwd]),
            },
        })
        .await
        .wrap_err("plugin/list failed in TUI")?;
    hide_cli_only_plugin_marketplaces(&mut response);
    Ok(response)
}

const CLI_HIDDEN_PLUGIN_MARKETPLACES: &[&str] = &["openai-bundled"];

pub(super) fn hide_cli_only_plugin_marketplaces(response: &mut PluginListResponse) {
    response
        .marketplaces
        .retain(|marketplace| !CLI_HIDDEN_PLUGIN_MARKETPLACES.contains(&marketplace.name.as_str()));
}

pub(super) async fn fetch_plugin_detail(
    request_handle: AppServerRequestHandle,
    params: PluginReadParams,
) -> Result<PluginReadResponse> {
    let request_id = RequestId::String(format!("plugin-read-{}", Uuid::new_v4()));
    request_handle
        .request_typed(ClientRequest::PluginRead { request_id, params })
        .await
        .wrap_err("plugin/read failed in TUI")
}

pub(super) async fn fetch_plugin_install(
    request_handle: AppServerRequestHandle,
    marketplace_path: AbsolutePathBuf,
    plugin_name: String,
) -> Result<PluginInstallResponse> {
    let request_id = RequestId::String(format!("plugin-install-{}", Uuid::new_v4()));
    request_handle
        .request_typed(ClientRequest::PluginInstall {
            request_id,
            params: PluginInstallParams {
                marketplace_path: Some(marketplace_path),
                remote_marketplace_name: None,
                plugin_name,
            },
        })
        .await
        .wrap_err("plugin/install failed in TUI")
}

pub(super) async fn fetch_plugin_uninstall(
    request_handle: AppServerRequestHandle,
    plugin_id: String,
) -> Result<PluginUninstallResponse> {
    let request_id = RequestId::String(format!("plugin-uninstall-{}", Uuid::new_v4()));
    request_handle
        .request_typed(ClientRequest::PluginUninstall {
            request_id,
            params: PluginUninstallParams { plugin_id },
        })
        .await
        .wrap_err("plugin/uninstall failed in TUI")
}

pub(super) async fn write_plugin_enabled(
    request_handle: AppServerRequestHandle,
    plugin_id: String,
    enabled: bool,
) -> Result<ConfigWriteResponse> {
    let request_id = RequestId::String(format!("plugin-enable-{}", Uuid::new_v4()));
    request_handle
        .request_typed(ClientRequest::ConfigValueWrite {
            request_id,
            params: ConfigValueWriteParams {
                key_path: format!("plugins.{plugin_id}"),
                value: serde_json::json!({ "enabled": enabled }),
                merge_strategy: MergeStrategy::Upsert,
                file_path: None,
                expected_version: None,
            },
        })
        .await
        .wrap_err("config/value/write failed while updating plugin enablement in TUI")
}

pub(super) fn build_feedback_upload_params(
    origin_thread_id: Option<ThreadId>,
    rollout_path: Option<PathBuf>,
    category: FeedbackCategory,
    reason: Option<String>,
    turn_id: Option<String>,
    include_logs: bool,
) -> FeedbackUploadParams {
    let extra_log_files = if include_logs {
        rollout_path.map(|rollout_path| vec![rollout_path])
    } else {
        None
    };
    let tags = turn_id.map(|turn_id| BTreeMap::from([(String::from("turn_id"), turn_id)]));
    FeedbackUploadParams {
        classification: crate::bottom_pane::feedback_classification(category).to_string(),
        reason,
        thread_id: origin_thread_id.map(|thread_id| thread_id.to_string()),
        include_logs,
        extra_log_files,
        tags,
    }
}

pub(super) async fn fetch_feedback_upload(
    request_handle: AppServerRequestHandle,
    params: FeedbackUploadParams,
) -> Result<FeedbackUploadResponse> {
    let request_id = RequestId::String(format!("feedback-upload-{}", Uuid::new_v4()));
    request_handle
        .request_typed(ClientRequest::FeedbackUpload { request_id, params })
        .await
        .wrap_err("feedback/upload failed in TUI")
}

/// Convert flat `McpServerStatus` responses into the per-server maps used by the
/// in-process MCP subsystem (tools keyed as `mcp__{server}__{tool}`, plus
/// per-server resource/template/auth maps). Test-only because the TUI
/// renders directly from `McpServerStatus` rather than these maps.
#[cfg(test)]
pub(super) type McpInventoryMaps = (
    HashMap<String, codex_protocol::mcp::Tool>,
    HashMap<String, Vec<codex_protocol::mcp::Resource>>,
    HashMap<String, Vec<codex_protocol::mcp::ResourceTemplate>>,
    HashMap<String, McpAuthStatus>,
);

#[cfg(test)]
pub(super) fn mcp_inventory_maps_from_statuses(statuses: Vec<McpServerStatus>) -> McpInventoryMaps {
    let mut tools = HashMap::new();
    let mut resources = HashMap::new();
    let mut resource_templates = HashMap::new();
    let mut auth_statuses = HashMap::new();

    for status in statuses {
        let server_name = status.name;
        auth_statuses.insert(
            server_name.clone(),
            match status.auth_status {
                codex_app_server_protocol::McpAuthStatus::Unsupported => McpAuthStatus::Unsupported,
                codex_app_server_protocol::McpAuthStatus::NotLoggedIn => McpAuthStatus::NotLoggedIn,
                codex_app_server_protocol::McpAuthStatus::BearerToken => McpAuthStatus::BearerToken,
                codex_app_server_protocol::McpAuthStatus::OAuth => McpAuthStatus::OAuth,
            },
        );
        resources.insert(server_name.clone(), status.resources);
        resource_templates.insert(server_name.clone(), status.resource_templates);
        for (tool_name, tool) in status.tools {
            tools.insert(format!("mcp__{server_name}__{tool_name}"), tool);
        }
    }

    (tools, resources, resource_templates, auth_statuses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::PluginMarketplaceEntry;
    use codex_protocol::mcp::Tool;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;

    fn test_absolute_path(path: &str) -> AbsolutePathBuf {
        AbsolutePathBuf::try_from(PathBuf::from(path)).expect("absolute test path")
    }

    #[test]
    fn hide_cli_only_plugin_marketplaces_removes_openai_bundled() {
        let mut response = PluginListResponse {
            marketplaces: vec![
                PluginMarketplaceEntry {
                    name: "openai-bundled".to_string(),
                    path: Some(test_absolute_path("/marketplaces/openai-bundled")),
                    interface: None,
                    plugins: Vec::new(),
                },
                PluginMarketplaceEntry {
                    name: "openai-curated".to_string(),
                    path: Some(test_absolute_path("/marketplaces/openai-curated")),
                    interface: None,
                    plugins: Vec::new(),
                },
            ],
            marketplace_load_errors: Vec::new(),
            featured_plugin_ids: Vec::new(),
        };

        hide_cli_only_plugin_marketplaces(&mut response);

        assert_eq!(
            response.marketplaces,
            vec![PluginMarketplaceEntry {
                name: "openai-curated".to_string(),
                path: Some(test_absolute_path("/marketplaces/openai-curated")),
                interface: None,
                plugins: Vec::new(),
            }]
        );
    }

    #[test]
    fn mcp_inventory_maps_prefix_tool_names_by_server() {
        let statuses = vec![
            McpServerStatus {
                name: "docs".to_string(),
                tools: HashMap::from([(
                    "list".to_string(),
                    Tool {
                        description: None,
                        name: "list".to_string(),
                        title: None,
                        input_schema: serde_json::json!({"type": "object"}),
                        output_schema: None,
                        annotations: None,
                        icons: None,
                        meta: None,
                    },
                )]),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                auth_status: codex_app_server_protocol::McpAuthStatus::Unsupported,
            },
            McpServerStatus {
                name: "disabled".to_string(),
                tools: HashMap::new(),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                auth_status: codex_app_server_protocol::McpAuthStatus::Unsupported,
            },
        ];

        let (tools, resources, resource_templates, auth_statuses) =
            mcp_inventory_maps_from_statuses(statuses);
        let mut resource_names = resources.keys().cloned().collect::<Vec<_>>();
        resource_names.sort();
        let mut template_names = resource_templates.keys().cloned().collect::<Vec<_>>();
        template_names.sort();

        assert_eq!(
            tools.keys().cloned().collect::<Vec<_>>(),
            vec!["mcp__docs__list".to_string()]
        );
        assert_eq!(resource_names, vec!["disabled", "docs"]);
        assert_eq!(template_names, vec!["disabled", "docs"]);
        assert_eq!(
            auth_statuses.get("disabled"),
            Some(&McpAuthStatus::Unsupported)
        );
    }

    #[test]
    fn extracts_primary_contact_message_from_structured_content() {
        let response = McpServerToolCallResponse {
            content: Vec::new(),
            structured_content: Some(serde_json::json!({
                "has_new_message": true,
                "message": " can you check this? "
            })),
            is_error: None,
            meta: None,
        };

        assert_eq!(
            extract_primary_contact_message(&response),
            Some("can you check this?".to_string())
        );
    }

    #[test]
    fn primary_contact_poll_ignores_explicit_empty_status() {
        let response = McpServerToolCallResponse {
            content: vec![serde_json::json!({
                "has_new_message": false,
                "message": "stale status"
            })],
            structured_content: None,
            is_error: None,
            meta: None,
        };

        assert_eq!(extract_primary_contact_message(&response), None);
    }

    #[test]
    fn primary_contact_poll_ignores_plain_status_text() {
        let response = McpServerToolCallResponse {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "No new messages"
            })],
            structured_content: None,
            is_error: None,
            meta: None,
        };

        assert_eq!(extract_primary_contact_message(&response), None);
    }

    #[test]
    fn primary_contact_poll_extracts_imessage_wait_reply_text() {
        let response = McpServerToolCallResponse {
            content: Vec::new(),
            structured_content: Some(serde_json::json!({
                "reply_received": true,
                "reply_text": " keep going "
            })),
            is_error: None,
            meta: None,
        };

        assert_eq!(
            extract_primary_contact_message(&response),
            Some("keep going".to_string())
        );
    }

    #[test]
    fn primary_contact_poll_ignores_imessage_wait_without_reply() {
        let response = McpServerToolCallResponse {
            content: Vec::new(),
            structured_content: Some(serde_json::json!({
                "reply_received": false,
                "awaiting_reply": true,
                "reply_text": "stale"
            })),
            is_error: None,
            meta: None,
        };

        assert_eq!(extract_primary_contact_message(&response), None);
    }

    #[test]
    fn primary_contact_poll_accepts_ambiguous_conflicting_reply_target() {
        let response = McpServerToolCallResponse {
            content: Vec::new(),
            structured_content: Some(serde_json::json!({
                "reply_received": false,
                "ambiguous": true,
                "match_reason": "conflicting_reply_target",
                "raw_reply_text": " just curious if you're awake "
            })),
            is_error: None,
            meta: None,
        };

        assert_eq!(
            extract_primary_contact_message(&response),
            Some("just curious if you're awake".to_string())
        );
    }

    #[test]
    fn primary_contact_imessage_check_is_one_shot() {
        assert_eq!(
            default_primary_contact_check_tool("imessage"),
            "imessage_followup_wait_for_reply"
        );
        assert_eq!(
            default_primary_contact_check_arguments("imessage"),
            Some(serde_json::json!({
                "max_polls": 1,
                "block_until_reply": false
            }))
        );
    }

    #[test]
    fn primary_contact_schedule_overrides_default_interval_when_active() {
        let poll_config = PrimaryContactPollConfig {
            mcp: "imessage".to_string(),
            tool: "imessage_followup_status".to_string(),
            arguments: None,
            default_interval_seconds: 900,
            schedule: vec![
                PrimaryContactPollSchedule::from_config(&OrchestratorPrimaryContactScheduleToml {
                    days: Some(vec!["weekdays".to_string()]),
                    start: Some("07:00".to_string()),
                    end: Some("22:00".to_string()),
                    check_messages_every_seconds: Some(300),
                })
                .expect("valid schedule"),
            ],
        };

        assert_eq!(poll_config.interval_seconds_at(Weekday::Mon, 8 * 60), 300);
        assert_eq!(poll_config.interval_seconds_at(Weekday::Sat, 8 * 60), 900);
        assert_eq!(poll_config.interval_seconds_at(Weekday::Mon, 23 * 60), 900);
    }

    #[test]
    fn primary_contact_schedule_supports_overnight_windows() {
        let schedule =
            PrimaryContactPollSchedule::from_config(&OrchestratorPrimaryContactScheduleToml {
                days: Some(vec!["mon".to_string()]),
                start: Some("22:00".to_string()),
                end: Some("07:00".to_string()),
                check_messages_every_seconds: Some(1800),
            })
            .expect("valid schedule");

        assert_eq!(
            schedule.interval_seconds_at(Weekday::Mon, 23 * 60),
            Some(1800)
        );
        assert_eq!(
            schedule.interval_seconds_at(Weekday::Tue, 6 * 60),
            Some(1800)
        );
        assert_eq!(schedule.interval_seconds_at(Weekday::Tue, 8 * 60), None);
    }

    #[test]
    fn build_feedback_upload_params_includes_thread_id_and_rollout_path() {
        let thread_id = ThreadId::new();
        let rollout_path = PathBuf::from("/tmp/rollout.jsonl");

        let params = build_feedback_upload_params(
            Some(thread_id),
            Some(rollout_path.clone()),
            FeedbackCategory::SafetyCheck,
            Some("needs follow-up".to_string()),
            Some("turn-123".to_string()),
            /*include_logs*/ true,
        );

        assert_eq!(params.classification, "safety_check");
        assert_eq!(params.reason, Some("needs follow-up".to_string()));
        assert_eq!(params.thread_id, Some(thread_id.to_string()));
        assert_eq!(
            params
                .tags
                .as_ref()
                .and_then(|tags| tags.get("turn_id"))
                .map(String::as_str),
            Some("turn-123")
        );
        assert_eq!(params.include_logs, true);
        assert_eq!(params.extra_log_files, Some(vec![rollout_path]));
    }

    #[test]
    fn build_feedback_upload_params_omits_rollout_path_without_logs() {
        let params = build_feedback_upload_params(
            /*origin_thread_id*/ None,
            Some(PathBuf::from("/tmp/rollout.jsonl")),
            FeedbackCategory::GoodResult,
            /*reason*/ None,
            /*turn_id*/ None,
            /*include_logs*/ false,
        );

        assert_eq!(params.classification, "good_result");
        assert_eq!(params.reason, None);
        assert_eq!(params.thread_id, None);
        assert_eq!(params.tags, None);
        assert_eq!(params.include_logs, false);
        assert_eq!(params.extra_log_files, None);
    }
}
