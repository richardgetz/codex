use codex_protocol::error::CodexErr;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use tokio_util::sync::CancellationToken;

use super::active_thread_scratchpad;
use super::continuous_run_policy_enabled;
use super::session::Session;
use super::turn_context::TurnContext;

pub(crate) async fn wait_for_model_capacity_retry(
    sess: &Session,
    turn_context: &TurnContext,
    cancellation_token: &CancellationToken,
) -> Result<bool, CodexErr> {
    let retry = turn_context.config.scratchpad.capacity_retry;
    if !retry.enabled || !continuous_policy_enabled(sess, turn_context) {
        return Ok(false);
    }

    sess.send_event(
        turn_context,
        EventMsg::Warning(WarningEvent {
            message: format!(
                "Selected model is at capacity. Continuous mode will retry in {} minute(s). Use `/continuous off` to stop automatic retries.",
                retry.delay.as_secs() / 60
            ),
        }),
    )
    .await;

    let deadline = tokio::time::Instant::now() + retry.delay;
    loop {
        tokio::select! {
            _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            _ = tokio::time::sleep_until(deadline) => {
                return Ok(continuous_policy_enabled(sess, turn_context));
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                if !continuous_policy_enabled(sess, turn_context) {
                    return Ok(false);
                }
            }
        }
    }
}

pub(crate) async fn wait_for_active_turn_model_capacity_retry(
    sess: &Session,
    turn_context: &TurnContext,
) -> Result<bool, CodexErr> {
    let Some((_, cancellation_token)) = sess.active_turn_context_and_cancellation_token().await
    else {
        return Ok(false);
    };
    wait_for_model_capacity_retry(sess, turn_context, &cancellation_token).await
}

fn continuous_policy_enabled(sess: &Session, turn_context: &TurnContext) -> bool {
    active_thread_scratchpad(&turn_context.config.codex_home, sess.thread_id)
        .as_ref()
        .is_some_and(continuous_run_policy_enabled)
}
