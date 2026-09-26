//! Catalog display names are presentation only; model selection retains wire slugs.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn custom_model_display_name_in_pickers_preserves_selection_slug() {
    let slug = "us.openai.gpt-5.6-luna";
    let (mut chat, mut events, _ops) = make_chatwidget_manual(Some(slug)).await;
    let mut preset = get_available_model(&chat, "gpt-5.5");
    preset.id = slug.to_string();
    preset.model = slug.to_string();
    preset.display_name = "GPT-5.6 Luna".to_string();
    preset.description = "Custom provider model".to_string();
    preset.default_reasoning_effort = ReasoningEffortConfig::High;
    preset.supported_reasoning_efforts = vec![
        ReasoningEffortPreset {
            effort: ReasoningEffortConfig::Low,
            description: "Quick answers".to_string(),
        },
        ReasoningEffortPreset {
            effort: ReasoningEffortConfig::High,
            description: "Deeper reasoning".to_string(),
        },
    ];
    let mut auto = preset.clone();
    auto.id = "codex-auto-fast".to_string();
    auto.model = auto.id.clone();
    auto.display_name = "Auto Fast".to_string();
    chat.model_catalog = Arc::new(ModelCatalog::new(vec![auto, preset.clone()]));
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::High));
    chat.open_model_popup_with_presets(chat.model_catalog.models.clone());
    assert_chatwidget_snapshot!(
        "custom_model_display_name_quick_picker",
        render_bottom_popup(&chat, /*width*/ 80)
    );
    chat.handle_key_event(KeyCode::Enter.into());
    assert_matches!(events.try_recv(), Ok(AppEvent::OpenAllModelsPopup));
    chat.open_all_models_popup();
    assert_chatwidget_snapshot!(
        "custom_model_display_name_all_models",
        render_bottom_popup(&chat, /*width*/ 80)
    );
    chat.handle_key_event(KeyCode::Enter.into());
    let selected =
        assert_matches!(events.try_recv(), Ok(AppEvent::OpenReasoningPopup { model }) => model);
    assert_eq!(selected, preset);
    chat.open_reasoning_popup(selected);
    assert_chatwidget_snapshot!(
        "custom_model_display_name_reasoning",
        render_bottom_popup(&chat, /*width*/ 80)
    );
    chat.handle_key_event(KeyCode::Enter.into());
    assert_matches!(events.try_recv(), Ok(AppEvent::UpdateModel(model)) if model == slug);
    assert_matches!(
        events.try_recv(),
        Ok(AppEvent::UpdateReasoningEffort(Some(
            ReasoningEffortConfig::High
        )))
    );
    let persisted = assert_matches!(events.try_recv(), Ok(AppEvent::PersistModelSelection { model, effort }) => (model, effort));
    assert_eq!(
        persisted,
        (slug.to_string(), Some(ReasoningEffortConfig::High))
    );
}

#[tokio::test]
async fn custom_model_display_name_in_status_line_and_fallback() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let slug = "us.openai.gpt-5.6-luna";
    let (mut chat, _events, _ops) = make_chatwidget_manual(Some(slug)).await;
    let mut preset = get_available_model(&chat, "gpt-5.5");
    preset.model = slug.to_string();
    preset.display_name = "GPT-5.6 Luna".to_string();
    preset.show_in_picker = false;
    chat.model_catalog = Arc::new(ModelCatalog::new(vec![preset]));
    chat.show_welcome_banner = false;
    chat.local_settings.tui.status_line = Some(vec![
        "model-name".to_string(),
        "model-with-reasoning".to_string(),
    ]);
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::High));
    chat.refresh_status_line();
    let width = 80;
    let mut terminal = Terminal::new(TestBackend::new(width, chat.desired_height(width)))
        .expect("create terminal");
    terminal
        .draw(|frame| chat.render(frame.area(), frame.buffer_mut()))
        .expect("draw model status line");
    assert_chatwidget_snapshot!(
        "custom_model_display_name_status_line",
        normalized_backend_snapshot(terminal.backend())
    );

    Arc::make_mut(&mut chat.model_catalog).models.clear();
    assert_eq!(chat.model_display_name(), slug);
    chat.set_model(crate::model_catalog::LUNA_RESERVE_MODEL);
    assert_eq!(chat.model_display_name(), "Luna Reserve");
    chat.set_model("");
    assert_eq!(chat.model_display_name(), DEFAULT_MODEL_DISPLAY_NAME);
}

fn install_team_model_catalog(chat: &mut ChatWidget) {
    let base = get_available_model(chat, "gpt-5.5");
    let models = [
        ("gpt-6-astra", "GPT-6 Astra"),
        ("gpt-6-sol", "GPT-6 Sol"),
        ("gpt-6-luna", "GPT-6 Luna"),
    ]
    .map(|(model, display_name)| {
        let mut preset = base.clone();
        preset.id = model.to_string();
        preset.model = model.to_string();
        preset.display_name = display_name.to_string();
        preset
    });
    chat.model_catalog = Arc::new(ModelCatalog::new(models.into()));
}

fn team_status_settings(
    mode: codex_app_server_protocol::TeamMode,
    role: Option<codex_app_server_protocol::TeamRole>,
    lead_model: &str,
    lead_effort: ReasoningEffortConfig,
    worker_model: &str,
    worker_effort: ReasoningEffortConfig,
) -> codex_app_server_protocol::ThreadTeamSettings {
    codex_app_server_protocol::ThreadTeamSettings {
        mode,
        role,
        lead_model: Some(lead_model.to_string()),
        lead_reasoning_effort: Some(lead_effort),
        lead_balance: Some(3),
        lead_work_policy: Some(codex_app_server_protocol::TeamLeadWorkPolicy::PromptGuided),
        worker_model: Some(worker_model.to_string()),
        worker_reasoning_effort: Some(worker_effort),
        previous_model: None,
        previous_reasoning_effort: None,
    }
}

fn render_status_snapshot(chat: &mut ChatWidget, width: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut terminal = Terminal::new(TestBackend::new(width, chat.desired_height(width)))
        .expect("create terminal");
    terminal
        .draw(|frame| chat.render(frame.area(), frame.buffer_mut()))
        .expect("draw Team model status line");
    normalized_backend_snapshot(terminal.backend())
}

#[tokio::test]
async fn status_line_setup_team_models_and_refreshes_after_settings_changes() {
    let (mut chat, _events, _ops) = make_chatwidget_manual(Some("gpt-6-sol")).await;
    install_team_model_catalog(&mut chat);
    chat.set_model("gpt-6-sol");
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::XHigh));
    chat.local_settings.tui.status_line = Some(vec!["model-with-reasoning".to_string()]);
    chat.refresh_status_line();
    assert_eq!(chat.status_line_text(), Some("GPT-6 Sol xhigh".to_string()));

    chat.local_settings.tui.status_line = Some(vec!["current-dir".to_string()]);
    chat.refresh_status_line();
    let current_dir = chat.status_line_text().expect("show current directory");
    chat.local_settings.tui.status_line = Some(vec![
        "model-with-reasoning".to_string(),
        "current-dir".to_string(),
    ]);
    chat.set_team_settings(Some(team_status_settings(
        codex_app_server_protocol::TeamMode::LeadWorker,
        Some(codex_app_server_protocol::TeamRole::Lead),
        "gpt-6-astra",
        ReasoningEffortConfig::XHigh,
        "gpt-6-sol",
        ReasoningEffortConfig::XHigh,
    )));
    let status_line = chat.status_line_text().expect("show Team status line");
    assert!(status_line.ends_with(&format!(" · {current_dir}")));
    chat.local_settings.tui.status_line = Some(vec!["model-with-reasoning".to_string()]);
    chat.refresh_status_line();
    assert_eq!(
        chat.status_line_text(),
        Some("Lead: GPT-6 Astra xhigh · Worker default: GPT-6 Sol xhigh".to_string())
    );

    chat.local_settings.tui.status_line = Some(vec!["model".to_string()]);
    chat.refresh_status_line();
    assert_eq!(
        chat.status_line_text(),
        Some("Lead: GPT-6 Astra · Worker default: GPT-6 Sol".to_string())
    );
    chat.local_settings.tui.status_line = Some(vec!["reasoning".to_string()]);
    chat.refresh_status_line();
    assert_eq!(
        chat.status_line_text(),
        Some("Lead: xhigh · Worker default: xhigh".to_string())
    );

    chat.local_settings.tui.status_line = Some(vec!["model-with-reasoning".to_string()]);
    chat.refresh_status_line();
    assert_chatwidget_snapshot!(
        "team_model_status_line_wide",
        render_status_snapshot(&mut chat, 80)
    );
    assert_chatwidget_snapshot!(
        "team_model_status_line_narrow",
        render_status_snapshot(&mut chat, 40)
    );

    chat.set_model("gpt-6-luna");
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::High));
    assert_eq!(
        chat.status_line_text(),
        Some("Lead: GPT-6 Luna high · Worker default: GPT-6 Sol xhigh".to_string())
    );
    chat.set_team_settings(Some(team_status_settings(
        codex_app_server_protocol::TeamMode::LeadWorker,
        Some(codex_app_server_protocol::TeamRole::Lead),
        "gpt-6-astra",
        ReasoningEffortConfig::XHigh,
        "gpt-6-sol",
        ReasoningEffortConfig::XHigh,
    )));
    assert_eq!(
        chat.status_line_text(),
        Some("Lead: GPT-6 Luna high · Worker default: GPT-6 Sol xhigh".to_string())
    );

    chat.set_team_settings(Some(team_status_settings(
        codex_app_server_protocol::TeamMode::LeadWorker,
        Some(codex_app_server_protocol::TeamRole::Lead),
        "gpt-6-astra",
        ReasoningEffortConfig::XHigh,
        "gpt-6-luna",
        ReasoningEffortConfig::High,
    )));
    assert_eq!(
        chat.status_line_text(),
        Some("Lead: GPT-6 Astra xhigh · Worker default: GPT-6 Luna high".to_string())
    );

    chat.set_team_settings(Some(team_status_settings(
        codex_app_server_protocol::TeamMode::LeadWorker,
        Some(codex_app_server_protocol::TeamRole::Lead),
        "gpt-6-luna",
        ReasoningEffortConfig::High,
        "gpt-6-sol",
        ReasoningEffortConfig::XHigh,
    )));
    assert_eq!(
        chat.status_line_text(),
        Some("Lead: GPT-6 Luna high · Worker default: GPT-6 Sol xhigh".to_string())
    );

    chat.set_team_settings(Some(team_status_settings(
        codex_app_server_protocol::TeamMode::LeadWorker,
        Some(codex_app_server_protocol::TeamRole::Worker),
        "gpt-6-luna",
        ReasoningEffortConfig::High,
        "gpt-6-sol",
        ReasoningEffortConfig::XHigh,
    )));
    assert_eq!(
        chat.status_line_text(),
        Some("Lead default: GPT-6 Luna high · Worker: GPT-6 Sol xhigh".to_string())
    );
    chat.set_model("gpt-6-astra");
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::High));
    assert_eq!(
        chat.status_line_text(),
        Some("Lead default: GPT-6 Luna high · Worker: GPT-6 Astra high".to_string())
    );

    chat.handle_thread_session(crate::session_state::ThreadSessionState {
        windows_sandbox_host: crate::app::WindowsSandboxHost::Local,
        thread_id: ThreadId::new(),
        forked_from_id: None,
        fork_parent_title: None,
        thread_name: None,
        model: "gpt-6-astra".to_string(),
        model_provider_id: "openai".to_string(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        approvals_reviewer: ApprovalsReviewer::User,
        permission_profile: PermissionProfile::read_only(),
        active_permission_profile: None,
        cwd: chat.config.cwd.clone(),
        runtime_workspace_roots: chat.config.workspace_roots.clone(),
        instruction_source_paths: Vec::new(),
        reasoning_effort: Some(ReasoningEffortConfig::High),
        collaboration_mode: None,
        team: Some(team_status_settings(
            codex_app_server_protocol::TeamMode::LeadWorker,
            Some(codex_app_server_protocol::TeamRole::Worker),
            "gpt-6-luna",
            ReasoningEffortConfig::High,
            "gpt-6-sol",
            ReasoningEffortConfig::XHigh,
        )),
        personality: None,
        message_history: None,
        network_proxy: None,
        rollout_path: None,
    });
    assert_eq!(
        chat.status_line_text(),
        Some("Lead default: GPT-6 Luna high · Worker: GPT-6 Sol xhigh".to_string())
    );

    chat.set_team_settings(Some(team_status_settings(
        codex_app_server_protocol::TeamMode::Off,
        None,
        "gpt-6-luna",
        ReasoningEffortConfig::High,
        "gpt-6-sol",
        ReasoningEffortConfig::XHigh,
    )));
    assert_eq!(
        chat.status_line_text(),
        Some("GPT-6 Astra high".to_string())
    );
    chat.set_model("gpt-6-luna");
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::High));
    assert_eq!(chat.status_line_text(), Some("GPT-6 Luna high".to_string()));
}
