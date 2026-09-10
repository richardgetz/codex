use super::lines_to_single_string;
use crate::app_event::AppEvent;
use crate::chatwidget::ChatWidget;
use crate::chatwidget::ChatWidgetInit;
use crate::chatwidget::ReplayKind;
use crate::tui::FrameRequester;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::RawResponseCompletedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadTokenUsage;
use codex_app_server_protocol::ThreadTokenUsageAttribution;
use codex_app_server_protocol::ThreadTokenUsageProjection;
use codex_app_server_protocol::ThreadTokenUsageProjectionThread;
use codex_app_server_protocol::ThreadTokenUsageProjectionUpdatedNotification;
use codex_app_server_protocol::ThreadTokenUsageResponseIdentity;
use codex_app_server_protocol::ThreadTokenUsageSource;
use codex_app_server_protocol::TokenUsageBreakdown;
use codex_protocol::ThreadId;
use color_eyre::eyre::Result;

fn usage(tokens: i64) -> TokenUsageBreakdown {
    TokenUsageBreakdown {
        total_tokens: tokens,
        input_tokens: tokens,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 0,
        reasoning_output_tokens: 0,
    }
}

fn raw_response(
    thread_id: ThreadId,
    parent_thread_id: ThreadId,
    response_id: &str,
    tokens: i64,
) -> ServerNotification {
    raw_response_with_model(
        thread_id,
        Some(parent_thread_id),
        response_id,
        tokens,
        "gpt-5.4",
    )
}

fn raw_response_with_model(
    thread_id: ThreadId,
    parent_thread_id: Option<ThreadId>,
    response_id: &str,
    tokens: i64,
    model: &str,
) -> ServerNotification {
    ServerNotification::RawResponseCompleted(RawResponseCompletedNotification {
        thread_id: thread_id.to_string(),
        turn_id: format!("turn-{response_id}"),
        response_id: response_id.to_string(),
        usage: Some(usage(tokens)),
        usage_metadata: None,
        source_thread_id: Some(thread_id.to_string()),
        parent_thread_id: parent_thread_id.map(|thread_id| thread_id.to_string()),
        completed_at: None,
        attribution: Some(ThreadTokenUsageAttribution {
            model: Some(model.to_string()),
            model_provider: Some("openai".to_string()),
            service_tier: None,
            context_length: Some("short".to_string()),
        }),
    })
}

fn projection(
    root_thread_id: ThreadId,
    worker_thread_id: ThreadId,
    grandchild_thread_id: ThreadId,
) -> ThreadTokenUsageProjection {
    let worker_response_id = "worker-response".to_string();
    let grandchild_response_id = "grandchild-response".to_string();
    ThreadTokenUsageProjection {
        total: usage(30),
        threads: vec![
            ThreadTokenUsageProjectionThread {
                thread_id: root_thread_id.to_string(),
                parent_thread_id: None,
                forked_from_id: None,
                sources: Vec::new(),
                response_ids: Vec::new(),
            },
            ThreadTokenUsageProjectionThread {
                thread_id: worker_thread_id.to_string(),
                parent_thread_id: Some(root_thread_id.to_string()),
                forked_from_id: None,
                sources: vec![ThreadTokenUsageSource {
                    thread_id: worker_thread_id.to_string(),
                    attribution: ThreadTokenUsageAttribution {
                        model: Some("gpt-5.4".to_string()),
                        model_provider: Some("openai".to_string()),
                        service_tier: None,
                        context_length: Some("short".to_string()),
                    },
                    response_ids: vec![worker_response_id.clone()],
                    usage: usage(10),
                }],
                response_ids: vec![ThreadTokenUsageResponseIdentity {
                    thread_id: worker_thread_id.to_string(),
                    response_id: worker_response_id,
                }],
            },
            ThreadTokenUsageProjectionThread {
                thread_id: grandchild_thread_id.to_string(),
                parent_thread_id: Some(worker_thread_id.to_string()),
                forked_from_id: None,
                sources: vec![ThreadTokenUsageSource {
                    thread_id: grandchild_thread_id.to_string(),
                    attribution: ThreadTokenUsageAttribution {
                        model: Some("gpt-5.4".to_string()),
                        model_provider: Some("openai".to_string()),
                        service_tier: None,
                        context_length: Some("short".to_string()),
                    },
                    response_ids: vec![grandchild_response_id.clone()],
                    usage: usage(20),
                }],
                response_ids: vec![ThreadTokenUsageResponseIdentity {
                    thread_id: grandchild_thread_id.to_string(),
                    response_id: grandchild_response_id,
                }],
            },
        ],
    }
}

fn direct_token_usage_notification(thread_id: ThreadId, total_tokens: i64) -> ServerNotification {
    let token_usage = usage(total_tokens);
    ServerNotification::ThreadTokenUsageUpdated(
        codex_app_server_protocol::ThreadTokenUsageUpdatedNotification {
            thread_id: thread_id.to_string(),
            turn_id: "direct".to_string(),
            token_usage: ThreadTokenUsage {
                total: token_usage.clone(),
                last: token_usage,
                ..ThreadTokenUsage::default()
            },
        },
    )
}

fn projection_notification(
    thread_id: ThreadId,
    projection: ThreadTokenUsageProjection,
) -> ServerNotification {
    ServerNotification::ThreadTokenUsageProjectionUpdated(
        ThreadTokenUsageProjectionUpdatedNotification {
            thread_id: thread_id.to_string(),
            usage_projection: Some(projection),
        },
    )
}

fn unavailable_projection_notification(thread_id: ThreadId) -> ServerNotification {
    ServerNotification::ThreadTokenUsageProjectionUpdated(
        ThreadTokenUsageProjectionUpdatedNotification {
            thread_id: thread_id.to_string(),
            usage_projection: None,
        },
    )
}

#[tokio::test]
async fn background_descendant_usage_survives_routing_and_widget_switch() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = super::make_test_app_with_channels().await;
    app.config.tui_status_token_usage.enabled = true;
    let root_thread_id = ThreadId::new();
    let worker_thread_id = ThreadId::new();
    let grandchild_thread_id = ThreadId::new();
    app.primary_thread_id = Some(root_thread_id);
    app.ensure_thread_channel(root_thread_id);
    app.ensure_thread_channel(worker_thread_id);
    app.ensure_thread_channel(grandchild_thread_id);

    let app_server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    for (index, notification) in [
        raw_response(worker_thread_id, root_thread_id, "worker-response", 10),
        raw_response(
            grandchild_thread_id,
            worker_thread_id,
            "grandchild-response",
            20,
        ),
        projection_notification(
            root_thread_id,
            projection(root_thread_id, worker_thread_id, grandchild_thread_id),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        app.handle_app_server_event(
            &app_server,
            AppServerEvent::ServerNotification(Box::new(notification)),
        )
        .await;

        if index == 1 {
            let live_snapshot = app.usage_rollup.lock().snapshot_for(root_thread_id);
            assert!(!live_snapshot.complete);
            assert_eq!(live_snapshot.total_usage.total_tokens, 30);
            assert_eq!(live_snapshot.sources.len(), 2);
        }
    }

    let before_switch = app.usage_rollup.lock().snapshot_for(root_thread_id);
    assert!(before_switch.complete);
    assert_eq!(before_switch.total_usage.total_tokens, 30);
    assert_eq!(before_switch.sources.len(), 2);

    let replacement = ChatWidget::new_with_app_event(ChatWidgetInit {
        config: app.config.clone(),
        environment_manager: app.environment_manager.clone(),
        frame_requester: FrameRequester::test_dummy(),
        app_event_tx: app.app_event_tx.clone(),
        state_db: None,
        provenance_commands_enabled: true,
        workspace_command_runner: None,
        initial_user_message: None,
        enhanced_keys_supported: app.enhanced_keys_supported,
        has_chatgpt_account: app.chat_widget.has_chatgpt_account(),
        has_codex_backend_auth: app.chat_widget.has_codex_backend_auth(),
        model_catalog: app.model_catalog.clone(),
        feedback: app.feedback.clone(),
        is_first_run: false,
        status_account_display: app.chat_widget.status_account_display().cloned(),
        runtime_model_provider_base_url: app
            .chat_widget
            .runtime_model_provider_base_url()
            .map(str::to_string),
        initial_plan_type: app.chat_widget.current_plan_type(),
        initial_collaboration_mode: None,
        model: Some(app.chat_widget.current_model().to_string()),
        startup_tooltip_override: None,
        status_line_invalid_items_warned: app.status_line_invalid_items_warned.clone(),
        terminal_title_invalid_items_warned: app.terminal_title_invalid_items_warned.clone(),
        session_telemetry: app.session_telemetry.clone(),
    });
    let mut replacement = replacement;
    replacement.set_thread_id_for_test(root_thread_id);
    app.replace_chat_widget(replacement);
    let after_switch = app.usage_rollup.lock().snapshot_for(root_thread_id);
    assert_eq!(after_switch, before_switch);

    while app_event_rx.try_recv().is_ok() {}
    app.chat_widget.add_status_output(
        /*refreshing_rate_limits*/ false, /*request_id*/ None,
    );
    let status_cell = loop {
        match app_event_rx.try_recv() {
            Ok(AppEvent::InsertHistoryCell(cell))
                if lines_to_single_string(&cell.display_lines(/*width*/ 120))
                    .contains("/status") =>
            {
                break cell;
            }
            Ok(_) => continue,
            Err(error) => panic!("expected replacement widget /status output: {error}"),
        }
    };
    let rendered_status = lines_to_single_string(&status_cell.display_lines(/*width*/ 120));
    assert!(
        rendered_status.contains("30 total"),
        "replacement widget must render recursive usage: {rendered_status}"
    );

    for notification in [
        raw_response(worker_thread_id, root_thread_id, "worker-response", 10),
        raw_response(
            grandchild_thread_id,
            worker_thread_id,
            "grandchild-response",
            20,
        ),
    ] {
        app.handle_app_server_event(
            &app_server,
            AppServerEvent::ServerNotification(Box::new(notification)),
        )
        .await;
    }
    let after_replay = app.usage_rollup.lock().snapshot_for(root_thread_id);
    assert_eq!(after_replay, before_switch);

    let history_path = app.config.codex_home.join("usage").join("daily_spend.json");
    let history: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(history_path)?)?;
    let response_dates = history["response_dates"]
        .as_object()
        .expect("daily spend response identity map");
    assert_eq!(response_dates.len(), 2);
    assert!(response_dates.contains_key(&format!("{worker_thread_id}:worker-response")));
    assert!(response_dates.contains_key(&format!("{grandchild_thread_id}:grandchild-response")));
    assert_eq!(
        history["days"].as_object().map(|days| {
            days.values()
                .filter_map(|day| day["tokens"].as_i64())
                .sum::<i64>()
        }),
        Some(30)
    );
    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn live_usage_is_visible_before_projection_and_deduplicates_replays() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = super::make_test_app_with_channels().await;
    app.config.tui_status_token_usage.enabled = true;
    let root_thread_id = ThreadId::new();
    let worker_thread_id = ThreadId::new();
    app.primary_thread_id = Some(root_thread_id);
    app.chat_widget.set_thread_id_for_test(root_thread_id);
    app.ensure_thread_channel(root_thread_id);
    app.ensure_thread_channel(worker_thread_id);

    let app_server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let root_response =
        raw_response_with_model(root_thread_id, None, "root-live-response", 11, "gpt-5.4");
    let worker_response = raw_response_with_model(
        worker_thread_id,
        Some(root_thread_id),
        "worker-live-response",
        13,
        "gpt-5.3",
    );
    for notification in [root_response.clone(), worker_response.clone()] {
        app.handle_app_server_event(
            &app_server,
            AppServerEvent::ServerNotification(Box::new(notification)),
        )
        .await;
    }

    app.handle_app_server_event(
        &app_server,
        AppServerEvent::ServerNotification(Box::new(unavailable_projection_notification(
            root_thread_id,
        ))),
    )
    .await;

    let live_snapshot = app.usage_rollup.lock().snapshot_for(root_thread_id);
    assert!(!live_snapshot.complete);
    assert_eq!(live_snapshot.total_usage.total_tokens, 24);
    assert_eq!(live_snapshot.sources.len(), 2);
    let live_models = live_snapshot
        .sources
        .iter()
        .filter_map(|source| source.model.as_deref())
        .collect::<Vec<_>>();
    assert!(live_models.contains(&"gpt-5.3"));
    assert!(live_models.contains(&"gpt-5.4"));

    for notification in [root_response, worker_response] {
        app.handle_app_server_event(
            &app_server,
            AppServerEvent::ServerNotification(Box::new(notification)),
        )
        .await;
    }
    let replayed_snapshot = app.usage_rollup.lock().snapshot_for(root_thread_id);
    assert_eq!(replayed_snapshot.total_usage.total_tokens, 24);
    assert_eq!(replayed_snapshot.sources.len(), 2);

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn late_projection_baseline_does_not_regress_direct_context_usage() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = super::make_test_app_with_channels().await;
    let root_thread_id = ThreadId::new();
    let worker_thread_id = ThreadId::new();
    let grandchild_thread_id = ThreadId::new();
    app.chat_widget.set_thread_id_for_test(root_thread_id);

    app.chat_widget.handle_server_notification(
        direct_token_usage_notification(root_thread_id, 100),
        /*replay_kind*/ None,
    );
    assert_eq!(app.chat_widget.token_usage().total_tokens, 100);

    app.chat_widget.handle_server_notification(
        projection_notification(
            root_thread_id,
            projection(root_thread_id, worker_thread_id, grandchild_thread_id),
        ),
        Some(ReplayKind::ThreadSnapshot),
    );

    assert_eq!(app.chat_widget.token_usage().total_tokens, 100);
    let recursive_snapshot = app.usage_rollup.lock().snapshot_for(root_thread_id);
    assert_eq!(recursive_snapshot.total_usage.total_tokens, 30);
    assert!(recursive_snapshot.complete);

    // A later direct update remains authoritative even when compaction lowers the context
    // counters; the guard is provenance-based rather than a cumulative max.
    app.chat_widget.handle_server_notification(
        direct_token_usage_notification(root_thread_id, 40),
        /*replay_kind*/ None,
    );
    assert_eq!(app.chat_widget.token_usage().total_tokens, 40);
    Ok(())
}
