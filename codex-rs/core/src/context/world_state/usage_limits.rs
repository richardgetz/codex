use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use crate::context::UsageLimitsContext;
use chrono::TimeZone;
use chrono::Utc;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use codex_protocol::protocol::ThreadUsagePolicy;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use serde::Deserialize;
use serde::Serialize;

const FIVE_HOUR_WINDOW_MINUTES: i64 = 5 * 60;
const WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;
const MAX_RENDERED_LIMITS: usize = 8;
const MAX_LIMIT_ID_CHARS: usize = 64;
// Keep this comfortably below the 1K-token review threshold even after the
// truncation marker is included in the rendered text.
const MAX_USAGE_CONTEXT_TOKENS: usize = 512;
const REPLACEMENT_NOTICE: &str =
    "These thread usage instructions replace all previously provided thread usage instructions.";
const REMOVAL_NOTICE: &str = "The previously provided thread usage status no longer applies.";

/// The bounded provider usage state shown to the model.
#[derive(Clone, Debug)]
pub(crate) struct UsageLimitsState {
    snapshot: UsageLimitsSnapshot,
    body: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct UsageLimitsSnapshot {
    #[serde(default)]
    policy: ThreadUsagePolicy,
    #[serde(default)]
    limits: Vec<UsageLimitSnapshot>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct UsageLimitSnapshot {
    limit_id: String,
    #[serde(default)]
    primary: Option<UsageWindowSnapshot>,
    #[serde(default)]
    secondary: Option<UsageWindowSnapshot>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct UsageWindowSnapshot {
    remaining_percent: f64,
    #[serde(default)]
    window_minutes: Option<i64>,
    #[serde(default)]
    resets_at: Option<i64>,
}

impl UsageLimitsState {
    pub(crate) fn new(policy: ThreadUsagePolicy, rate_limits: &[RateLimitSnapshot]) -> Self {
        let snapshot = UsageLimitsSnapshot {
            policy,
            limits: rate_limits
                .iter()
                .take(MAX_RENDERED_LIMITS)
                .map(Self::limit)
                .collect(),
        };
        let body = render_body(&snapshot);
        Self { snapshot, body }
    }

    fn limit(rate_limits: &RateLimitSnapshot) -> UsageLimitSnapshot {
        UsageLimitSnapshot {
            limit_id: rate_limits
                .limit_id
                .as_deref()
                .unwrap_or("codex")
                .chars()
                .take(MAX_LIMIT_ID_CHARS)
                .collect(),
            primary: rate_limits.primary.as_ref().map(Self::window),
            secondary: rate_limits.secondary.as_ref().map(Self::window),
        }
    }

    fn window(window: &RateLimitWindow) -> UsageWindowSnapshot {
        UsageWindowSnapshot {
            remaining_percent: if window.used_percent.is_finite() {
                (100.0 - window.used_percent).clamp(0.0, 100.0)
            } else {
                100.0
            },
            window_minutes: window.window_minutes,
            resets_at: window.resets_at,
        }
    }
}

impl WorldStateSection for UsageLimitsState {
    const ID: &'static str = "usage_limits";
    type Snapshot = UsageLimitsSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        self.snapshot.clone()
    }

    fn should_persist(&self) -> bool {
        self.snapshot.policy != ThreadUsagePolicy::default()
            || self
                .snapshot
                .limits
                .iter()
                .any(|limit| limit.primary.is_some() || limit.secondary.is_some())
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "developer"
            && UsageLimitsContext::matches_text(text)
            && !text.contains(REMOVAL_NOTICE)
    }

    fn has_retained_fragment_matcher() -> bool {
        true
    }

    fn matches_retained_fragment(role: &str, text: &str) -> bool {
        Self::matches_legacy_fragment(role, text)
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        let current_should_persist = self.should_persist();
        if current_should_persist
            && matches!(&previous, PreviousSectionState::Known(previous) if *previous == &self.snapshot)
        {
            return None;
        }
        let previous_had_status = !matches!(&previous, PreviousSectionState::Absent);
        let body = if !current_should_persist {
            if !previous_had_status {
                return None;
            }
            REMOVAL_NOTICE.to_string()
        } else if previous_had_status {
            format!("{REPLACEMENT_NOTICE}\n\n{}", self.body)
        } else {
            self.body.clone()
        };
        Some(Box::new(UsageLimitsContext::new(body)))
    }
}

fn render_body(snapshot: &UsageLimitsSnapshot) -> String {
    let mut lines = vec![
        "Provider usage status is advisory and may be stale; do not treat it as a guarantee of available capacity.".to_string(),
        format!(
            "Automatic resume after a reset is {}.",
            if snapshot.policy.auto_resume {
                "enabled for this thread"
            } else {
                "disabled for this thread"
            }
        ),
    ];
    if let Some(minimum_remaining_percent) = snapshot.policy.minimum_remaining_percent {
        lines.push(format!(
            "Automatic continuation stops when a known provider window has less than {minimum_remaining_percent}% remaining."
        ));
    }

    let show_limit_id = snapshot.limits.len() > 1;
    for limit in &snapshot.limits {
        for (default_label, window) in [
            ("primary", limit.primary.as_ref()),
            ("secondary", limit.secondary.as_ref()),
        ] {
            let Some(window) = window else {
                continue;
            };
            let label = window_label(default_label, window.window_minutes);
            let label = if show_limit_id {
                format!("{} {label}", limit.limit_id)
            } else {
                label
            };
            lines.push(format!(
                "{label} window: {}% remaining; resets at {}.",
                format_remaining_percent(window.remaining_percent),
                format_reset(window.resets_at)
            ));
        }
    }
    if lines.len() == 2 {
        lines.push("No provider usage window is currently available.".to_string());
    }
    truncate_text(
        &lines.join("\n"),
        TruncationPolicy::Tokens(MAX_USAGE_CONTEXT_TOKENS),
    )
}

fn format_remaining_percent(remaining_percent: f64) -> String {
    if remaining_percent.fract() == 0.0 {
        format!("{remaining_percent:.0}")
    } else {
        format!("{remaining_percent:.1}")
    }
}

fn window_label(default_label: &str, window_minutes: Option<i64>) -> String {
    match window_minutes {
        Some(FIVE_HOUR_WINDOW_MINUTES) => "5-hour".to_string(),
        Some(WEEKLY_WINDOW_MINUTES) => "weekly".to_string(),
        Some(minutes) => format!("{minutes}-minute"),
        None => default_label.to_string(),
    }
}

fn format_reset(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|timestamp| Utc.timestamp_opt(timestamp, 0).single())
        .map(|timestamp| timestamp.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
#[path = "usage_limits_tests.rs"]
mod tests;
