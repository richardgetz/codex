use chrono::DateTime;
use chrono::TimeZone;
use chrono::Utc;
use codex_protocol::error::CodexErr;
use codex_protocol::error::UsageLimitReachedError;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RateLimitReachedType;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use codex_protocol::protocol::ThreadUsagePolicy;
use codex_protocol::protocol::WarningEvent;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::session::Session;
use super::turn_context::TurnContext;

const USAGE_LIMIT_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const MAX_USAGE_LIMIT_RETRIES: usize = 3;

impl Session {
    pub(crate) async fn usage_policy_and_rate_limits(
        &self,
    ) -> (ThreadUsagePolicy, Vec<RateLimitSnapshot>) {
        let (policy, rate_limits) = {
            let state = self.state.lock().await;
            (
                state.session_configuration.usage_policy,
                state.rate_limit_snapshots(),
            )
        };
        let rate_limits = match self
            .services
            .time_provider
            .current_time(self.thread_id)
            .await
        {
            Ok(now) => active_rate_limits(rate_limits, now.timestamp()),
            Err(err) => {
                tracing::warn!(%err, "unable to read current time for usage-limit status");
                rate_limits
            }
        };
        (policy, rate_limits)
    }
}

fn active_rate_limits(rate_limits: Vec<RateLimitSnapshot>, now: i64) -> Vec<RateLimitSnapshot> {
    rate_limits
        .into_iter()
        .map(|mut snapshot| {
            snapshot.primary = active_rate_limit_window(snapshot.primary, now);
            snapshot.secondary = active_rate_limit_window(snapshot.secondary, now);
            snapshot
        })
        .collect()
}

fn active_rate_limit_window(window: Option<RateLimitWindow>, now: i64) -> Option<RateLimitWindow> {
    window.filter(|window| window.resets_at.is_none_or(|resets_at| resets_at > now))
}

/// Returns whether a harness-driven continuation may start under the current policy.
pub(crate) fn automatic_continuation_allowed(
    policy: ThreadUsagePolicy,
    rate_limits: &[RateLimitSnapshot],
) -> bool {
    let Some(minimum_remaining_percent) = policy.minimum_remaining_percent else {
        return true;
    };
    if rate_limits.is_empty() {
        // Unknown usage must not disable work. The policy is enforced as soon as
        // the provider supplies a comparable snapshot.
        return true;
    }

    let minimum_remaining_percent = f64::from(minimum_remaining_percent);
    rate_limits.iter().all(|rate_limits| {
        let windows = [rate_limits.primary.as_ref(), rate_limits.secondary.as_ref()];
        windows
            .into_iter()
            .flatten()
            .all(|window| remaining_percent(window) >= minimum_remaining_percent)
    })
}

/// Finds the provider reset time that should be used for a usage-limit retry.
///
/// The error's timestamp is authoritative. Older or partial responses may only
/// provide window timestamps, in which case an exhausted window uses the latest
/// reset among exhausted windows to avoid a tight retry loop. The retained
/// session snapshots are a final fallback for errors that omit rate-limit data.
pub(crate) fn usage_limit_reset_at(
    error: &UsageLimitReachedError,
    fallback_rate_limits: &[RateLimitSnapshot],
) -> Option<DateTime<Utc>> {
    error
        .resets_at
        .or_else(|| error.rate_limits.as_deref().and_then(snapshot_reset_at))
        .or_else(|| {
            fallback_rate_limits
                .iter()
                .filter_map(snapshot_reset_at)
                .max()
        })
}

fn snapshot_reset_at(snapshot: &RateLimitSnapshot) -> Option<DateTime<Utc>> {
    let exhausted_resets = [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
        .into_iter()
        .flatten()
        .filter(|window| remaining_percent(window) <= 0.0)
        .filter_map(|window| window.resets_at);
    let reset_timestamp = exhausted_resets.max().or_else(|| {
        [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(|window| window.resets_at)
            .min()
    })?;
    Utc.timestamp_opt(reset_timestamp, 0).single()
}

fn remaining_percent(window: &RateLimitWindow) -> f64 {
    if window.used_percent.is_finite() {
        (100.0 - window.used_percent).clamp(0.0, 100.0)
    } else {
        100.0
    }
}

fn reset_is_not_automatic(
    error: &UsageLimitReachedError,
    fallback_rate_limits: &[RateLimitSnapshot],
) -> bool {
    matches!(
        error.rate_limit_reached_type.or_else(|| error
            .rate_limits
            .as_deref()
            .and_then(|snapshot| snapshot.rate_limit_reached_type)
            .or_else(|| {
                fallback_rate_limits
                    .iter()
                    .find_map(|snapshot| snapshot.rate_limit_reached_type)
            })),
        Some(
            RateLimitReachedType::WorkspaceOwnerCreditsDepleted
                | RateLimitReachedType::WorkspaceMemberCreditsDepleted
                | RateLimitReachedType::WorkspaceOwnerUsageLimitReached
                | RateLimitReachedType::WorkspaceMemberUsageLimitReached
        )
    )
}

fn snapshot_is_non_resettable(snapshot: &RateLimitSnapshot) -> bool {
    matches!(
        snapshot.rate_limit_reached_type,
        Some(
            RateLimitReachedType::WorkspaceOwnerCreditsDepleted
                | RateLimitReachedType::WorkspaceMemberCreditsDepleted
                | RateLimitReachedType::WorkspaceOwnerUsageLimitReached
                | RateLimitReachedType::WorkspaceMemberUsageLimitReached
        )
    ) || snapshot.spend_control_reached == Some(true)
}

fn usage_snapshot_recovered(policy: ThreadUsagePolicy, rate_limits: &[RateLimitSnapshot]) -> bool {
    if rate_limits.is_empty() || rate_limits.iter().any(snapshot_is_non_resettable) {
        return false;
    }
    let has_window = rate_limits
        .iter()
        .any(|snapshot| snapshot.primary.is_some() || snapshot.secondary.is_some());
    has_window
        && rate_limits.iter().all(|snapshot| {
            [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
                .into_iter()
                .flatten()
                .all(|window| remaining_percent(window) > 0.0)
        })
        && automatic_continuation_allowed(policy, rate_limits)
}

fn usage_resume_sleep_duration(
    reset_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    check_interval: Duration,
) -> Duration {
    reset_at
        .map(|reset_at| {
            (reset_at - now)
                .to_std()
                .unwrap_or_default()
                .min(check_interval)
        })
        .filter(|remaining| !remaining.is_zero())
        .unwrap_or_else(|| {
            if reset_at.is_some_and(|reset_at| reset_at <= now) {
                Duration::ZERO
            } else {
                check_interval
            }
        })
}

/// Waits for a resettable provider usage window while allowing live policy changes.
pub(crate) async fn wait_for_usage_limit_reset(
    sess: &Session,
    turn_context: &TurnContext,
    error: &UsageLimitReachedError,
    cancellation_token: &CancellationToken,
) -> Result<bool, CodexErr> {
    wait_for_usage_recovery(sess, turn_context, Some(error), cancellation_token).await
}

/// Waits for a provider window to rise above the configured continuation floor.
///
/// Floor pauses are kept inside the active turn so an opted-in worker can
/// resume after a five-hour or weekly reset instead of silently terminating its
/// automatic work. Explicit user turns never call this function.
pub(crate) async fn wait_for_usage_limit_floor(
    sess: &Session,
    turn_context: &TurnContext,
    cancellation_token: &CancellationToken,
) -> Result<bool, CodexErr> {
    let (policy, rate_limits) = sess.usage_policy_and_rate_limits().await;
    if !policy.auto_resume
        || policy.minimum_remaining_percent.is_none()
        || automatic_continuation_allowed(policy, &rate_limits)
    {
        return Ok(false);
    }
    wait_for_usage_recovery(sess, turn_context, /*error*/ None, cancellation_token).await
}

struct UsageResumeWaitGuard<'a>(&'a Session);

impl Drop for UsageResumeWaitGuard<'_> {
    fn drop(&mut self) {
        self.0.set_usage_resume_waiting(false);
    }
}

async fn wait_for_usage_recovery(
    sess: &Session,
    turn_context: &TurnContext,
    error: Option<&UsageLimitReachedError>,
    cancellation_token: &CancellationToken,
) -> Result<bool, CodexErr> {
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    let (policy, rate_limits) = sess.usage_policy_and_rate_limits().await;
    let retained_rate_limits = sess.state.lock().await.rate_limit_snapshots();
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if !policy.auto_resume
        || retained_rate_limits.iter().any(snapshot_is_non_resettable)
        || error.is_some_and(|error| reset_is_not_automatic(error, &retained_rate_limits))
    {
        return Ok(false);
    }
    let mut reset_at = error
        .and_then(|error| usage_limit_reset_at(error, &retained_rate_limits))
        .or_else(|| rate_limits.iter().filter_map(snapshot_reset_at).max());
    let check_interval = Duration::from_secs(
        turn_context
            .config
            .tui_usage_auto_resume
            .check_interval_minutes
            .saturating_mul(60),
    );
    if check_interval.is_zero() {
        return Ok(false);
    }

    let mut warning_sent = false;
    sess.set_usage_resume_waiting(true);
    let _wait_guard = UsageResumeWaitGuard(sess);
    loop {
        let (policy, _) = sess.usage_policy_and_rate_limits().await;
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        if !policy.auto_resume {
            return Ok(false);
        }
        let now = match sess
            .services
            .time_provider
            .current_time(sess.thread_id)
            .await
        {
            Ok(now) => now,
            Err(err) => {
                tracing::warn!(%err, "unable to read current time for usage-limit auto-resume");
                if cancellation_token.is_cancelled() {
                    return Err(CodexErr::TurnAborted);
                }
                return Ok(false);
            }
        };
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }

        if !warning_sent {
            let retry_after = match reset_at {
                Some(reset_at) if reset_at <= now => "now".to_string(),
                Some(reset_at) => format!(
                    "in {} minutes (provider reset at {})",
                    usage_resume_sleep_duration(Some(reset_at), now, check_interval).as_secs() / 60,
                    reset_at.to_rfc3339()
                ),
                None => format!("in {} minutes", check_interval.as_secs() / 60),
            };
            sess.send_event(
                turn_context,
                EventMsg::Warning(WarningEvent {
                    message: format!(
                        "Usage limit reached. Auto-resume is enabled; the next account check is {retry_after}. Use `/continue` to check now or `/usage auto-resume off` to stop."
                    ),
                }),
            )
            .await;
            warning_sent = true;
        }

        let sleep_duration = usage_resume_sleep_duration(reset_at, now, check_interval);
        let sleep = sess
            .services
            .time_provider
            .sleep(sess.thread_id, sleep_duration);
        let mut manually_requested = false;
        tokio::select! {
            _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            _ = sess.wait_for_usage_resume_check() => {
                manually_requested = true;
            }
            result = sleep => {
                if let Err(err) = result {
                    tracing::warn!(%err, "usage-limit auto-resume wait failed");
                    if cancellation_token.is_cancelled() {
                        return Err(CodexErr::TurnAborted);
                    }
                    return Ok(false);
                }
            }
        }

        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }

        let refreshed = tokio::select! {
            _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            refreshed = tokio::time::timeout(
                USAGE_LIMIT_REFRESH_TIMEOUT,
                refresh_account_rate_limits(sess, turn_context),
            ) => {
                match refreshed {
                    Ok(refreshed) => refreshed,
                    Err(_) => {
                        tracing::warn!(
                            timeout_seconds = USAGE_LIMIT_REFRESH_TIMEOUT.as_secs(),
                            "account usage refresh timed out during auto-resume"
                        );
                        None
                    }
                }
            }
        };
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        let (policy, current_rate_limits) = sess.usage_policy_and_rate_limits().await;
        if !policy.auto_resume {
            return Ok(false);
        }
        if refreshed.is_some() && usage_snapshot_recovered(policy, &current_rate_limits) {
            sess.send_event(
                turn_context,
                EventMsg::Warning(WarningEvent {
                    message: if manually_requested {
                        "Usage is available again. Resuming the paused work now.".to_string()
                    } else {
                        "Usage reset detected. Resuming the paused work now.".to_string()
                    },
                }),
            )
            .await;
            return Ok(true);
        }

        if manually_requested {
            let message = if refreshed.is_some() {
                "Usage is still exhausted or unavailable. The paused work will keep waiting for the next scheduled check."
            } else {
                "Account usage could not be refreshed. The paused work will keep waiting for the next scheduled check."
            };
            sess.send_event(
                turn_context,
                EventMsg::Warning(WarningEvent {
                    message: message.to_string(),
                }),
            )
            .await;
        }

        let now_after_refresh = match sess
            .services
            .time_provider
            .current_time(sess.thread_id)
            .await
        {
            Ok(now) => now,
            Err(err) => {
                tracing::warn!(%err, "unable to read current time after usage-limit refresh");
                return Ok(false);
            }
        };
        if refreshed.is_some() {
            if current_rate_limits.iter().any(snapshot_is_non_resettable) {
                return Ok(false);
            }
            reset_at = current_rate_limits
                .iter()
                .filter_map(snapshot_reset_at)
                .max();
            if reset_at.is_some_and(|reset_at| reset_at > now_after_refresh)
                && current_rate_limits.iter().all(|snapshot| {
                    [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
                        .into_iter()
                        .flatten()
                        .all(|window| remaining_percent(window) <= 0.0)
                })
            {
                // The provider returned another exhausted window. Keep this
                // same parked turn alive; the model retry budget is consumed
                // only after an availability check succeeds.
                continue;
            }
        } else if error.is_some() && reset_at.is_some_and(|reset_at| reset_at <= now_after_refresh)
        {
            // A known reset time is enough to permit the normal model retry when
            // the authenticated account endpoint is unavailable. If the model
            // still rejects the request, its next error re-enters this wait.
            // Floor pauses have no model error to corroborate the reset, so they
            // stay parked and use the next mechanical check instead.
            return Ok(true);
        }

        // A reset timestamp that did not restore capacity is stale. Fall back
        // to the configured mechanical check interval and avoid a busy loop.
        if reset_at.is_some_and(|reset_at| reset_at <= now_after_refresh) {
            reset_at = None;
        }
    }
}

async fn refresh_account_rate_limits(
    sess: &Session,
    turn_context: &TurnContext,
) -> Option<Vec<RateLimitSnapshot>> {
    let auth = sess.services.auth_manager.auth().await?;
    if !auth.uses_codex_backend() {
        return None;
    }
    let client = codex_backend_client::Client::from_auth(
        turn_context.config.chatgpt_base_url.clone(),
        &auth,
        turn_context.config.http_client_factory(),
    );
    match client.get_rate_limits_many().await {
        Ok(rate_limits) => {
            for rate_limit in &rate_limits {
                sess.record_rate_limits_info(rate_limit.clone()).await;
            }
            Some(rate_limits)
        }
        Err(err) => {
            tracing::warn!(%err, "unable to refresh account usage for auto-resume");
            None
        }
    }
}

#[cfg(test)]
#[path = "usage_policy_tests.rs"]
mod tests;
