use super::LeadPassivePollState;

fn assert_sleep_does_not_park(state: &LeadPassivePollState, call_id: &str, sample_id: u64) {
    state.schedule_sleep(call_id, sample_id);
    assert!(state.take_sleep_park_decision(call_id).is_none());
}

#[test]
fn same_sample_status_and_sleep_do_not_trigger_a_passive_park() {
    let status_before_sleep = LeadPassivePollState::default();
    assert_sleep_does_not_park(&status_before_sleep, "first", 1);
    status_before_sleep.observed_status_probe(2);
    assert_sleep_does_not_park(&status_before_sleep, "same-sample", 2);
    assert_sleep_does_not_park(&status_before_sleep, "next-sample", 3);

    let sleep_before_status = LeadPassivePollState::default();
    assert_sleep_does_not_park(&sleep_before_status, "first", 1);
    assert_sleep_does_not_park(&sleep_before_status, "same-sample", 2);
    sleep_before_status.observed_status_probe(2);
    assert_sleep_does_not_park(&sleep_before_status, "next-sample", 3);

    sleep_before_status.observed_status_probe(4);
    sleep_before_status.schedule_sleep("after-later-probe", 5);
    assert!(sleep_before_status
        .take_sleep_park_decision("after-later-probe")
        .is_some());
}

#[test]
fn same_sample_status_and_substantive_work_do_not_form_a_passive_poll() {
    let status_then_work = LeadPassivePollState::default();
    assert_sleep_does_not_park(&status_then_work, "first", 1);
    status_then_work.observed_status_probe(2);
    status_then_work.observed_substantive_work(2);
    assert_sleep_does_not_park(&status_then_work, "after-status-and-work", 3);

    let work_then_status = LeadPassivePollState::default();
    assert_sleep_does_not_park(&work_then_status, "first", 1);
    work_then_status.observed_substantive_work(2);
    work_then_status.observed_status_probe(2);
    assert_sleep_does_not_park(&work_then_status, "after-work-and-status", 3);
}

#[tokio::test]
async fn substantive_sibling_wakes_an_already_consumed_passive_park() {
    let state = LeadPassivePollState::default();
    assert_sleep_does_not_park(&state, "first", 1);
    state.observed_status_probe(2);
    state.schedule_sleep("passive", 3);
    let (mut substantive_work_rx, generation) = state
        .take_sleep_park_decision("passive")
        .expect("later-sample status should make this a passive park");

    state.observed_substantive_work(3);
    substantive_work_rx
        .changed()
        .await
        .expect("substantive work should wake the passive park");
    assert_ne!(*substantive_work_rx.borrow_and_update(), generation);
}
