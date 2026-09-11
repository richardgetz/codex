use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ThreadActivityContinueResponse;
use codex_app_server_protocol::ThreadActivityPauseResponse;
use codex_app_server_protocol::ThreadActivityReadResponse;
use codex_app_server_protocol::ThreadPauseState;
use codex_app_server_protocol::ThreadStartParams;
use tempfile::TempDir;
use tokio::time::timeout;

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn thread_activity_pause_continue_is_root_scoped_and_wake_only() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(REQUEST_TIMEOUT)
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;

    let pause_request = app
        .send_raw_request(
            "thread/activity/pause",
            Some(serde_json::json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let pause_response: ThreadActivityPauseResponse =
        timeout(REQUEST_TIMEOUT, app.read_response(pause_request)).await??;
    assert_eq!(pause_response, ThreadActivityPauseResponse {});

    let read_request = app
        .send_raw_request(
            "thread/activity/read",
            Some(serde_json::json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let paused: ThreadActivityReadResponse =
        timeout(REQUEST_TIMEOUT, app.read_response(read_request)).await??;
    assert!(!paused.activities.is_empty());
    assert!(
        paused
            .activities
            .iter()
            .all(|activity| activity.pause_state == ThreadPauseState::Paused)
    );

    let continue_request = app
        .send_raw_request(
            "thread/activity/continue",
            Some(serde_json::json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let continue_response: ThreadActivityContinueResponse =
        timeout(REQUEST_TIMEOUT, app.read_response(continue_request)).await??;
    assert_eq!(continue_response, ThreadActivityContinueResponse {});

    let read_request = app
        .send_raw_request(
            "thread/activity/read",
            Some(serde_json::json!({"threadId": thread.id.clone()})),
        )
        .await?;
    let resumed: ThreadActivityReadResponse =
        timeout(REQUEST_TIMEOUT, app.read_response(read_request)).await??;
    assert!(
        resumed
            .activities
            .iter()
            .all(|activity| activity.pause_state == ThreadPauseState::Running)
    );
    Ok(())
}
