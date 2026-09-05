use super::*;
use chrono::TimeZone;
use chrono::Utc;
use pretty_assertions::assert_eq;

fn rate_limits(
    primary_used_percent: f64,
    secondary_used_percent: Option<f64>,
) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: primary_used_percent,
            window_minutes: Some(300),
            resets_at: Some(1_700_000_000),
        }),
        secondary: secondary_used_percent.map(|used_percent| RateLimitWindow {
            used_percent,
            window_minutes: Some(10_080),
            resets_at: Some(1_700_100_000),
        }),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }
}

#[test]
fn automatic_continuation_uses_the_lowest_known_remaining_budget() {
    let policy = ThreadUsagePolicy {
        auto_resume: false,
        minimum_remaining_percent: Some(20),
    };
    let primary_only = rate_limits(70.0, Some(75.0));
    let allowed = [
        automatic_continuation_allowed(policy, &[]),
        automatic_continuation_allowed(policy, std::slice::from_ref(&primary_only)),
        automatic_continuation_allowed(
            policy,
            std::slice::from_ref(&rate_limits(81.0, Some(75.0))),
        ),
        automatic_continuation_allowed(
            policy,
            std::slice::from_ref(&rate_limits(70.0, Some(75.0))),
        ),
    ];

    assert_eq!(allowed, [true, true, false, true]);
}

#[test]
fn expired_rate_limit_windows_do_not_block_continuation() {
    let policy = ThreadUsagePolicy {
        auto_resume: false,
        minimum_remaining_percent: Some(20),
    };
    let active = active_rate_limits(vec![rate_limits(100.0, Some(100.0))], 1_700_100_000);

    assert_eq!(
        active,
        vec![RateLimitSnapshot {
            primary: None,
            secondary: None,
            ..rate_limits(100.0, Some(100.0))
        }]
    );
    assert!(automatic_continuation_allowed(policy, &active));
}

#[test]
fn usage_limit_reset_prefers_error_timestamp_and_uses_latest_exhausted_window() {
    let snapshot = RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent: 100.0,
            window_minutes: Some(300),
            resets_at: Some(1_700_000_010),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 100.0,
            window_minutes: Some(10_080),
            resets_at: Some(1_700_000_020),
        }),
        ..rate_limits(0.0, None)
    };
    let from_snapshot = UsageLimitReachedError {
        plan_type: None,
        resets_at: None,
        rate_limits: Some(Box::new(snapshot)),
        promo_message: None,
        rate_limit_reached_type: None,
    };
    let from_error = UsageLimitReachedError {
        plan_type: None,
        resets_at: Some(Utc.timestamp_opt(1_700_000_030, 0).single().unwrap()),
        rate_limits: Some(Box::new(RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 100.0,
                window_minutes: Some(300),
                resets_at: Some(1_700_000_010),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 100.0,
                window_minutes: Some(10_080),
                resets_at: Some(1_700_000_020),
            }),
            ..rate_limits(0.0, None)
        })),
        promo_message: None,
        rate_limit_reached_type: None,
    };

    assert_eq!(
        usage_limit_reset_at(&from_snapshot, &[]),
        Utc.timestamp_opt(1_700_000_020, 0).single()
    );
    assert_eq!(
        usage_limit_reset_at(&from_error, &[]),
        Utc.timestamp_opt(1_700_000_030, 0).single()
    );
}

#[test]
fn usage_limit_reset_uses_retained_snapshot_when_error_omits_it() {
    let error = UsageLimitReachedError {
        plan_type: None,
        resets_at: None,
        rate_limits: None,
        promo_message: None,
        rate_limit_reached_type: None,
    };

    assert_eq!(
        usage_limit_reset_at(
            &error,
            std::slice::from_ref(&RateLimitSnapshot {
                primary: Some(RateLimitWindow {
                    used_percent: 100.0,
                    window_minutes: Some(300),
                    resets_at: Some(1_700_000_040),
                }),
                ..rate_limits(0.0, None)
            }),
        ),
        Utc.timestamp_opt(1_700_000_040, 0).single()
    );
}

#[test]
fn workspace_limits_are_not_auto_resumed() {
    let error = UsageLimitReachedError {
        plan_type: None,
        resets_at: Some(Utc.timestamp_opt(1_700_000_030, 0).single().unwrap()),
        rate_limits: None,
        promo_message: None,
        rate_limit_reached_type: Some(RateLimitReachedType::WorkspaceOwnerUsageLimitReached),
    };

    assert!(reset_is_not_automatic(&error, &[]));
}
