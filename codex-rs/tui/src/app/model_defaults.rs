//! Apply session model choices separately from saved model defaults.
//!
//! A successful config write can still be overridden. Report that distinction without
//! replacing the active task's explicit selection with launch-time configuration.

use super::App;
use super::AppRunControl;
use crate::app_event::AppEvent;
use crate::app_server_session::AppServerSession;
use crate::chatwidget::AstraModelPickerAction;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ConfigEdit;
use codex_app_server_protocol::WriteStatus;
use codex_protocol::config_types::ModeKind;
use codex_protocol::openai_models::ReasoningEffort;
use color_eyre::eyre::Result;

impl App {
    /// Apply the model change selected from a picker without recursively dispatching an
    /// `AppEvent`. The event dispatcher future is intentionally large, so nesting another
    /// `handle_event` call here can exhaust the terminal thread's stack.
    pub(super) async fn handle_model_picker_action(
        &mut self,
        app_server: &mut AppServerSession,
        model: String,
        action: AstraModelPickerAction,
    ) -> AppRunControl {
        match action {
            AstraModelPickerAction::UpdateModel => {
                self.handle_update_model(app_server, model).await
            }
            AstraModelPickerAction::ApplyAdvancedReasoning { effort } => {
                self.handle_apply_advanced_reasoning(app_server, model, effort)
                    .await
            }
            AstraModelPickerAction::SelectSessionModel { effort } => {
                self.app_event_tx.send(AppEvent::FollowTranscript);
                self.select_session_model(app_server, model, effort).await;
                AppRunControl::Continue
            }
        }
    }

    pub(super) async fn handle_update_model(
        &mut self,
        app_server: &mut AppServerSession,
        model: String,
    ) -> AppRunControl {
        if self
            .active_thread_model_setting_update_params(model.clone())
            .is_some_and(|params| params.permissions.is_some())
            && self.reject_pending_permission_change()
        {
            return AppRunControl::Continue;
        }
        let model_changed = self.chat_widget.current_model() != model
            || self.chat_widget.current_collaboration_mode().model() != model;
        if model_changed {
            self.chat_widget.set_model(&model);
            self.sync_active_thread_model_setting(app_server, model, /*effort*/ None)
                .await;
            self.sync_active_thread_service_tier_to_cached_session()
                .await;
        }
        AppRunControl::Continue
    }

    pub(super) async fn handle_apply_advanced_reasoning(
        &mut self,
        app_server: &mut AppServerSession,
        model: String,
        effort: ReasoningEffort,
    ) -> AppRunControl {
        self.app_event_tx.send(AppEvent::FollowTranscript);
        if self
            .active_thread_model_setting_update_params(model.clone())
            .is_some_and(|params| params.permissions.is_some())
            && self.reject_pending_permission_change()
        {
            return AppRunControl::Continue;
        }
        let model_changed = self.chat_widget.current_model() != model
            || self.chat_widget.current_collaboration_mode().model() != model;
        let default_effort = self.on_apply_advanced_reasoning(model.as_str(), effort.clone());
        if model_changed {
            self.sync_active_thread_model_setting(app_server, model.clone(), Some(effort.clone()))
                .await;
        } else if let Some(mut params) =
            self.active_thread_reasoning_setting_update_params(Some(effort.clone()))
        {
            params.collaboration_mode = Some(self.chat_widget.effective_collaboration_mode());
            self.send_thread_settings_update(app_server, params).await;
        }
        self.sync_active_thread_service_tier_to_cached_session()
            .await;

        if let Some(default_effort) = default_effort.as_ref()
            && let Err(err) = self
                .persist_model_defaults(
                    app_server.request_handle(),
                    crate::config_update::build_model_selection_edits(
                        model.as_str(),
                        Some(default_effort),
                    ),
                    "default model and reasoning effort",
                )
                .await
        {
            let error = crate::config_update::format_config_error(&err);
            tracing::error!(error = %error, "failed to persist conversation model");
            self.chat_widget
                .add_error_message(format!("Failed to save default model: {error}"));
        } else {
            self.chat_widget.add_info_message(
                format!("Model changed to {model} {effort} for this conversation"),
                /*hint*/ None,
            );
        }
        AppRunControl::Continue
    }

    pub(super) async fn select_session_model(
        &mut self,
        app_server: &mut AppServerSession,
        model: String,
        effort: Option<ReasoningEffort>,
    ) {
        let model_changed = self.chat_widget.current_model() != model
            || self.chat_widget.current_collaboration_mode().model() != model;
        if model_changed
            && self
                .active_thread_model_setting_update_params(model.clone())
                .is_some_and(|params| params.permissions.is_some())
            && self.reject_pending_permission_change()
        {
            return;
        }
        let in_plan_mode = self.chat_widget.effective_collaboration_mode().mode == ModeKind::Plan;
        let ultra = effort == Some(ReasoningEffort::Ultra);
        let clear_default_ultra = self
            .chat_widget
            .current_collaboration_mode()
            .reasoning_effort()
            == Some(ReasoningEffort::Ultra)
            && self.config.model_reasoning_effort != Some(ReasoningEffort::Ultra);
        let clear_plan_ultra = self.chat_widget.config_ref().plan_mode_reasoning_effort
            == Some(ReasoningEffort::Ultra)
            && self.config.plan_mode_reasoning_effort != Some(ReasoningEffort::Ultra);
        self.chat_widget.set_model(&model);
        if !in_plan_mode || ultra || clear_default_ultra {
            self.chat_widget.set_reasoning_effort(effort.clone());
        }
        if in_plan_mode || ultra || clear_plan_ultra {
            self.chat_widget
                .set_plan_mode_reasoning_effort(effort.clone());
        }
        if model_changed {
            self.sync_active_thread_model_setting(app_server, model.clone(), effort.clone())
                .await;
        } else if let Some(mut params) =
            self.active_thread_reasoning_setting_update_params(effort.clone())
        {
            params.collaboration_mode = Some(self.chat_widget.effective_collaboration_mode());
            self.send_thread_settings_update(app_server, params).await;
        }
        self.sync_active_thread_service_tier_to_cached_session()
            .await;
        let mut message = format!("Model changed to {model}");
        if let Some(label) = Self::reasoning_label_for(&model, effort.as_ref()) {
            message.push(' ');
            message.push_str(&label);
        }
        message.push_str(" for this session only");
        self.chat_widget.add_info_message(message, /*hint*/ None);
    }

    pub(super) async fn persist_model_defaults(
        &mut self,
        request_handle: AppServerRequestHandle,
        edits: Vec<ConfigEdit>,
        setting: &str,
    ) -> Result<()> {
        let response = crate::config_update::write_config_batch(request_handle, edits).await?;
        if response.status == WriteStatus::OkOverridden {
            self.chat_widget.add_warning_message(format!(
                "Saved {setting}, but a higher-priority configuration layer overrides the saved value."
            ));
        }
        Ok(())
    }
}
