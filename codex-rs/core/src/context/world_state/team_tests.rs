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
