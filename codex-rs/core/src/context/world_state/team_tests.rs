use super::*;

#[test]
fn worker_ceiling_is_part_of_team_snapshot_and_updates_guidance() {
    let current = TeamPolicyState::new(TeamRole::Lead, Some(3));
    let fragment = current
        .render_diff(PreviousSectionState::Absent)
        .expect("Lead instructions should be rendered");
    assert!(fragment.body().contains("concurrency ceiling: 3"));

    let unchanged = current.snapshot();
    assert!(
        current
            .render_diff(PreviousSectionState::Known(&unchanged))
            .is_none()
    );

    let changed = TeamPolicyState::new(TeamRole::Lead, Some(4));
    let fragment = changed
        .render_diff(PreviousSectionState::Known(&unchanged))
        .expect("changed ceiling should update Lead instructions");
    assert!(fragment.body().contains("concurrency ceiling: 4"));
}

#[test]
fn worker_role_does_not_include_lead_ceiling_guidance() {
    let worker = TeamPolicyState::new(TeamRole::Worker, Some(3));
    let fragment = worker
        .render_diff(PreviousSectionState::Absent)
        .expect("Worker instructions should be rendered");

    assert!(
        !fragment
            .body()
            .contains("Direct Worker concurrency ceiling")
    );
}

#[test]
fn dynamic_handoff_updates_lead_and_worker_guidance() {
    let default_lead = TeamPolicyState::new(TeamRole::Lead, None);
    let default_snapshot = default_lead.snapshot();
    let enabled_lead = TeamPolicyState::new(TeamRole::Lead, None).with_dynamic_handoff(true);
    let fragment = enabled_lead
        .render_diff(PreviousSectionState::Known(&default_snapshot))
        .expect("dynamic handoff should update Lead instructions");
    assert!(fragment.body().contains("quick preflight judgment"));

    let enabled_worker = TeamPolicyState::new(TeamRole::Worker, None).with_dynamic_handoff(true);
    let fragment = enabled_worker
        .render_diff(PreviousSectionState::Absent)
        .expect("dynamic handoff should render Worker instructions");
    assert!(fragment.body().contains("filter irrelevant material"));

    let disabled = TeamPolicyState::disabled();
    let fragment = disabled
        .render_diff(PreviousSectionState::Known(&enabled_lead.snapshot()))
        .expect("disabling team policy should replace retained instructions");
    assert!(!fragment.body().contains("Dynamic lookup handoff"));
}

#[test]
fn lead_balance_updates_lead_guidance_but_not_worker_guidance() {
    let default_lead = TeamPolicyState::new(TeamRole::Lead, None);
    let default_snapshot = default_lead.snapshot();
    let focused_lead = TeamPolicyState::new(TeamRole::Lead, None).with_lead_balance(4);
    let fragment = focused_lead
        .render_diff(PreviousSectionState::Known(&default_snapshot))
        .expect("Lead balance should update Lead instructions");
    assert!(fragment.body().contains("Confidence focused"));

    let worker = TeamPolicyState::new(TeamRole::Worker, None).with_lead_balance(5);
    let fragment = worker
        .render_diff(PreviousSectionState::Absent)
        .expect("Worker instructions should be rendered");
    assert!(!fragment.body().contains("Lead usage/confidence balance"));

    let worker_snapshot = worker.snapshot();
    let default_worker = TeamPolicyState::new(TeamRole::Worker, None);
    assert!(
        default_worker
            .render_diff(PreviousSectionState::Known(&worker_snapshot))
            .is_none(),
        "Lead-only balance changes must not refresh Worker context"
    );
}
