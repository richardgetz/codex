use super::Session;
use super::SessionSettingsUpdate;
use super::tests::make_session_and_context;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_state::ThreadControlMode;
use codex_state::ThreadControlRecord;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn continuous_mode_update_creates_and_releases_thread_control() {
    let (session, _turn_context) = make_session_and_context().await;
    let mut continuous_mode = session.collaboration_mode().await;
    continuous_mode.mode = ModeKind::Continuous;

    session
        .update_settings(SessionSettingsUpdate {
            collaboration_mode: Some(continuous_mode),
            ..Default::default()
        })
        .await
        .expect("continuous mode update should succeed");

    let active_control = session
        .active_thread_control()
        .await
        .expect("continuous control should be active");
    assert_eq!(
        active_control,
        ThreadControlRecord {
            thread_id: session.conversation_id,
            mode: ThreadControlMode::Continuous,
            reason: Session::CONTINUOUS_MODE_CONTROL_REASON.to_string(),
            release_channel: None,
            watch_interval_seconds: None,
            released_at: None,
            updated_at: active_control.updated_at,
            target_thread_ids: Vec::new(),
        }
    );

    let mut default_mode = session.collaboration_mode().await;
    default_mode.mode = ModeKind::Default;
    session
        .update_settings(SessionSettingsUpdate {
            collaboration_mode: Some(default_mode),
            ..Default::default()
        })
        .await
        .expect("default mode update should succeed");

    assert_eq!(session.active_thread_control().await, None);
}

#[tokio::test]
async fn continuous_mode_does_not_replace_active_router_control() {
    let (session, _turn_context) = make_session_and_context().await;
    let router_control = ThreadControlRecord {
        thread_id: session.conversation_id,
        mode: ThreadControlMode::Router,
        reason: "Router mode is supervising this thread.".to_string(),
        release_channel: Some("imessage".to_string()),
        watch_interval_seconds: Some(30),
        released_at: None,
        updated_at: chrono::Utc::now(),
        target_thread_ids: vec![
            ThreadId::from_string("00000000-0000-0000-0000-000000000022")
                .expect("target thread id"),
        ],
    };
    session
        .set_active_thread_control(Some(router_control.clone()))
        .await;

    let mut continuous_mode = session.collaboration_mode().await;
    continuous_mode.mode = ModeKind::Continuous;
    session
        .update_settings(SessionSettingsUpdate {
            collaboration_mode: Some(continuous_mode),
            ..Default::default()
        })
        .await
        .expect("continuous mode update should succeed");

    assert_eq!(session.active_thread_control().await, Some(router_control));
}
