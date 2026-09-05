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

const USAGE_RESET_POLICY_POLL_INTERVAL: Duration = Duration::from_secs(1);
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

/// Waits for a resettable provider usage window while allowing live policy changes.
pub(crate) async fn wait_for_usage_limit_reset(
    sess: &Session,
    turn_context: &TurnContext,
    error: &UsageLimitReachedError,
    cancellation_token: &CancellationToken,
) -> Result<bool, CodexErr> {
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    let (policy, _) = sess.usage_policy_and_rate_limits().await;
    let retained_rate_limits = sess.state.lock().await.rate_limit_snapshots();
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if !policy.auto_resume || reset_is_not_automatic(error, &retained_rate_limits) {
        return Ok(false);
    }
    let Some(reset_at) = usage_limit_reset_at(error, &retained_rate_limits) else {
        return Ok(false);
    };

    let mut warning_sent = false;
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
        let remaining = reset_at.signed_duration_since(now);
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        if remaining <= chrono::Duration::zero() {
            return Ok(true);
        }

        if !warning_sent {
            sess.send_event(
                turn_context,
                EventMsg::Warning(WarningEvent {
                    message: format!(
                        "Usage limit reached. Auto-resume is enabled and will retry after {}. Use the thread settings update to disable it.",
                        reset_at.to_rfc3339()
                    ),
                }),
            )
            .await;
            warning_sent = true;
        }

        let remaining = remaining
            .to_std()
            .unwrap_or(USAGE_RESET_POLICY_POLL_INTERVAL);
        let sleep_duration = remaining.min(USAGE_RESET_POLICY_POLL_INTERVAL);
        let sleep = sess
            .services
            .time_provider
            .sleep(sess.thread_id, sleep_duration);
        tokio::select! {
            _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            result = sleep => {
                if let Err(err) = result {
                    tracing::warn!(%err, "usage-limit auto-resume wait failed");
                    if cancellation_token.is_cancelled() {
                        return Err(CodexErr::TurnAborted);
                    }
                    return Ok(false);
                }
                if cancellation_token.is_cancelled() {
                    return Err(CodexErr::TurnAborted);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "usage_policy_tests.rs"]
mod tests;
