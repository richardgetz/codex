use crate::AppServerTarget;
use crate::RemoteAppServerEndpoint;
use crate::app::AppRunControl;
use crate::app::ExitReason;
use crate::app::tests::make_test_app_with_channels;
use crate::app_event::AppEvent;
use codex_protocol::ThreadId;
use color_eyre::Result;

#[tokio::test]
async fn local_daemon_frontend_refresh_carries_chat_widget_provider() -> Result<()> {
    let (mut app, _events, _operations) = make_test_app_with_channels().await;
    let effective_provider = app.chat_widget.config_ref().model_provider_id.clone();
    let mut app_server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    app.config.model_provider_id = "startup-default-provider".to_string();
    assert_ne!(
        app.config.model_provider_id, effective_provider,
        "the test must distinguish startup and active-thread providers"
    );
    app.app_server_target = AppServerTarget::LocalDaemon {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: "wss://example.com/".to_string(),
            auth_token: None,
        },
    };
    app.frontend_launcher = Some(std::env::current_exe()?);
    let thread_id = ThreadId::new();
    let mut tui = crate::tui::test_support::make_test_tui()?;

    let control = app
        .handle_event(
            &mut tui,
            &mut app_server,
            AppEvent::FrontendRefreshRequested { thread_id },
        )
        .await?;

    let AppRunControl::Exit(ExitReason::FrontendRefresh { model_provider, .. }) = control else {
        panic!("local daemon refresh should request a frontend restart");
    };
    assert_eq!(model_provider.as_deref(), Some(effective_provider.as_str()));
    app_server.shutdown().await?;
    Ok(())
}
