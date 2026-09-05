use super::PreviousSectionState;
use super::UsageLimitsState;
use super::WorldStateSection;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use codex_protocol::protocol::ThreadUsagePolicy;
use codex_utils_output_truncation::approx_token_count;
use pretty_assertions::assert_eq;

#[test]
fn usage_limits_fragment_describes_provider_windows_and_policy() {
    let rate_limits = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 70.0,
            window_minutes: Some(300),
            resets_at: Some(1_700_000_000),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 80.0,
            window_minutes: Some(10_080),
            resets_at: Some(1_700_100_000),
        }),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let state = UsageLimitsState::new(
        ThreadUsagePolicy {
            auto_resume: true,
            minimum_remaining_percent: Some(20),
        },
        std::slice::from_ref(&rate_limits),
    );

    let fragment = state
        .render_diff(PreviousSectionState::Absent)
        .expect("usage status should be rendered");

    assert_eq!(fragment.content_kind().0, "usage_limits.status");
    assert_eq!(
        fragment.markers(),
        ("<thread_usage_limits>\n", "\n</thread_usage_limits>")
    );
    assert!(fragment.body().contains("30% remaining"));
    assert!(fragment.body().contains("weekly window: 20% remaining"));
    assert!(
        fragment
            .body()
            .contains("Automatic resume after a reset is enabled")
    );
}

#[test]
fn usage_limits_fragment_is_not_repeated_when_snapshot_is_unchanged() {
    let state = UsageLimitsState::new(
        ThreadUsagePolicy::default(),
        std::slice::from_ref(&RateLimitSnapshot {
            limit_id: None,
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 10.0,
                window_minutes: Some(300),
                resets_at: Some(1_700_000_000),
            }),
            secondary: None,
            credits: None,
            individual_limit: None,
            spend_control_reached: None,
            plan_type: None,
            rate_limit_reached_type: None,
        }),
    );
    let snapshot = state.snapshot();

    assert!(
        state
            .render_diff(PreviousSectionState::Known(&snapshot))
            .is_none()
    );
}

#[test]
fn usage_limits_fragment_reports_when_previous_status_is_retired() {
    let previous = UsageLimitsState::new(
        ThreadUsagePolicy {
            auto_resume: true,
            minimum_remaining_percent: Some(20),
        },
        &[],
    )
    .snapshot();
    let current = UsageLimitsState::new(ThreadUsagePolicy::default(), &[]);

    let fragment = current
        .render_diff(PreviousSectionState::Known(&previous))
        .expect("retiring usage status should be rendered");

    assert!(
        fragment
            .body()
            .contains("previously provided thread usage status no longer applies")
    );
    assert!(!UsageLimitsState::matches_retained_fragment(
        "developer",
        &fragment.render()
    ));
    assert!(!UsageLimitsState::matches_legacy_fragment(
        "developer",
        &fragment.render()
    ));
}

#[test]
fn usage_limits_removal_is_not_rediscovered_from_legacy_history() {
    let previous_state = UsageLimitsState::new(
        ThreadUsagePolicy {
            auto_resume: true,
            minimum_remaining_percent: Some(20),
        },
        &[],
    );
    let mut previous_world_state = crate::context::world_state::WorldState::default();
    previous_world_state.add_section(previous_state);
    let previous = previous_world_state.snapshot();

    let current = UsageLimitsState::new(ThreadUsagePolicy::default(), &[]);
    let mut world_state = crate::context::world_state::WorldState::default();
    world_state.add_section(current);
    let removal = world_state
        .render_diff(&previous)
        .pop()
        .expect("first removal should be rendered");
    let removal = crate::context::ContextualUserFragment::into_boxed_response_item(removal);
    assert!(
        world_state
            .render_history_diff(Some(&previous), std::slice::from_ref(&removal))
            .is_empty(),
        "a removal tombstone must not count as retained usage status"
    );
}

#[test]
fn usage_limits_snapshot_accepts_removed_optional_fields() {
    let snapshot: super::UsageLimitsSnapshot = serde_json::from_value(serde_json::json!({
        "policy": {},
        "limits": [{
            "limit_id": "codex",
            "primary": {"remaining_percent": 20.5}
        }]
    }))
    .expect("missing optional usage fields should deserialize");

    assert_eq!(
        snapshot,
        super::UsageLimitsSnapshot {
            policy: ThreadUsagePolicy::default(),
            limits: vec![super::UsageLimitSnapshot {
                limit_id: "codex".to_string(),
                primary: Some(super::UsageWindowSnapshot {
                    remaining_percent: 20.5,
                    window_minutes: None,
                    resets_at: None,
                }),
                secondary: None,
            }],
        }
    );
}

#[test]
fn usage_limits_context_has_a_token_bound_for_multibyte_limit_ids() {
    let rate_limits = (0..super::MAX_RENDERED_LIMITS)
        .map(|index| RateLimitSnapshot {
            limit_id: Some(format!(
                "限界-{index}-{}",
                "界".repeat(super::MAX_LIMIT_ID_CHARS)
            )),
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 50.0,
                window_minutes: Some(300),
                resets_at: None,
            }),
            secondary: None,
            credits: None,
            individual_limit: None,
            spend_control_reached: None,
            plan_type: None,
            rate_limit_reached_type: None,
        })
        .collect::<Vec<_>>();
    let fragment = UsageLimitsState::new(ThreadUsagePolicy::default(), &rate_limits)
        .render_diff(PreviousSectionState::Absent)
        .expect("usage status should be rendered");

    assert!(approx_token_count(&fragment.body()) <= 1_000);
}
