use super::super::test_support::render_section_cases;
use super::*;

fn state(active: bool, start_instructions: Option<&str>) -> RealtimeState {
    RealtimeState::new(active, start_instructions)
}

#[test]
fn snapshots() {
    use PreviousSectionState::Absent;
    use PreviousSectionState::Known;
    use PreviousSectionState::Unknown;

    let inactive = state(/*active*/ false, /*start_instructions*/ None);
    let active = state(/*active*/ true, /*start_instructions*/ None);
    let custom_active = state(/*active*/ true, Some("custom realtime instructions"));
    let changed_custom_active = state(
        /*active*/ true,
        Some("changed custom realtime instructions"),
    );

    insta::assert_snapshot!(render_section_cases(&[
        (Absent, Absent),
        (Absent, Known(&inactive)),
        (Absent, Known(&active)),
        (Known(&inactive), Known(&active)),
        (Known(&inactive), Known(&custom_active)),
        (Known(&active), Known(&active)),
        (Known(&custom_active), Known(&changed_custom_active)),
        (Known(&active), Known(&inactive)),
        (Unknown, Known(&active)),
        (Unknown, Known(&inactive)),
    ]));
}

#[test]
fn retained_fragment_matcher_only_matches_starts() {
    let start = RealtimeStartWithInstructions::new("custom instructions").render();
    let end = RealtimeEndInstructions::new("inactive").render();

    assert!(RealtimeState::matches_legacy_fragment("developer", &start));
    assert!(!RealtimeState::matches_legacy_fragment("developer", &end));
}

#[test]
fn disabled_preambles_add_a_directive_to_main_agent_context() {
    let state = RealtimeState::new(true, None).suppress_preambles();
    let rendered = state.render_start().render();

    assert!(rendered.contains("Respond normally to the user's direct conversational turns"));
    assert!(rendered.contains("Begin the substantive answer or action directly"));
}

#[test]
fn realtime_start_instructions_are_bounded_before_policy_is_appended() {
    let custom_instructions = "x".repeat(50_000);
    let state = RealtimeState::new(true, Some(&custom_instructions)).suppress_preambles();
    let rendered = state.render_start().render();

    assert!(rendered.len() < custom_instructions.len());
    assert!(rendered.contains("A normal direct answer is not a preamble and must still be spoken"));
}

#[test]
fn preamble_transition_emits_the_current_policy() {
    let enabled = RealtimeState::new(true, None);
    let suppressed = enabled.clone().suppress_preambles();

    let suppressed_fragment = suppressed
        .render_diff(PreviousSectionState::Known(&enabled.snapshot()))
        .expect("suppression transition should update realtime instructions");
    assert!(
        suppressed_fragment
            .render()
            .contains("Respond normally to the user's direct conversational turns")
    );

    let reenabled_fragment = enabled
        .render_diff(PreviousSectionState::Known(&suppressed.snapshot()))
        .expect("reenable transition should update realtime instructions");
    assert!(
        reenabled_fragment
            .render()
            .contains("Conversational backchannels and progress preambles are enabled")
    );
}

#[test]
fn legacy_snapshots_default_to_preambles_enabled() {
    let snapshot: RealtimeSnapshot = serde_json::from_value(serde_json::json!({"active": true}))
        .expect("legacy realtime snapshot should deserialize");

    assert_eq!(
        snapshot,
        RealtimeSnapshot {
            active: true,
            suppress_preambles: false,
        }
    );
}
