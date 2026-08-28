use super::await_existing_call_step;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn existing_call_handshake_honors_cancellation() {
    let stop_token = CancellationToken::new();
    let task_stop_token = stop_token.clone();
    let task = tokio::spawn(async move {
        await_existing_call_step(std::future::pending::<CodexResult<()>>(), &task_stop_token).await
    });

    stop_token.cancel();

    assert!(matches!(
        task.await.expect("handshake task should finish"),
        Err(error) if matches!(error.details(), CodexErrorDetails::TurnAborted)
    ));
}
