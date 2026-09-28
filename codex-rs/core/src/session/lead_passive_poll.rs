use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::watch;

#[derive(Debug)]
pub(crate) struct LeadPassivePollState {
    progress: Mutex<LeadPassivePollProgress>,
    substantive_work_tx: watch::Sender<u64>,
}

#[cfg(test)]
#[path = "lead_passive_poll_tests.rs"]
mod tests;

#[derive(Debug, Default)]
struct LeadPassivePollProgress {
    last_long_sleep_sample_id: Option<u64>,
    last_status_probe_sample_id: Option<u64>,
    pending_sleep_calls: HashMap<String, (u64, bool)>,
    substantive_work_generation: u64,
}

impl Default for LeadPassivePollState {
    fn default() -> Self {
        let (substantive_work_tx, _) = watch::channel(0);
        Self {
            progress: Mutex::new(LeadPassivePollProgress::default()),
            substantive_work_tx,
        }
    }
}

impl LeadPassivePollState {
    pub(crate) fn schedule_sleep(&self, call_id: &str, sample_id: u64) {
        let mut progress = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let should_park = matches!(
            (
                progress.last_long_sleep_sample_id,
                progress.last_status_probe_sample_id,
            ),
            (Some(sleep_id), Some(status_id))
                if status_id > sleep_id && status_id < sample_id
        );
        progress.last_long_sleep_sample_id = Some(sample_id);
        progress
            .pending_sleep_calls
            .insert(call_id.to_string(), (sample_id, should_park));
    }

    pub(crate) fn take_sleep_park_decision(
        &self,
        call_id: &str,
    ) -> Option<(watch::Receiver<u64>, u64)> {
        let mut progress = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let should_park = progress
            .pending_sleep_calls
            .remove(call_id)
            .is_some_and(|(_, should_park)| should_park);
        should_park.then(|| {
            (
                self.substantive_work_tx.subscribe(),
                progress.substantive_work_generation,
            )
        })
    }

    pub(crate) fn observed_status_probe(&self, sample_id: u64) {
        let mut progress = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        progress.last_status_probe_sample_id = Some(sample_id);
    }

    pub(crate) fn observed_substantive_work(&self, sample_id: u64) {
        let mut progress = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        progress.substantive_work_generation = progress.substantive_work_generation.wrapping_add(1);
        self.substantive_work_tx
            .send_replace(progress.substantive_work_generation);
        progress.last_long_sleep_sample_id = None;
        progress.last_status_probe_sample_id = None;
        for (pending_sample_id, should_park) in progress.pending_sleep_calls.values_mut() {
            if *pending_sample_id == sample_id {
                *should_park = false;
            }
        }
    }

    pub(crate) fn reset(&self) {
        let mut progress = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        progress.last_long_sleep_sample_id = None;
        progress.last_status_probe_sample_id = None;
        progress.pending_sleep_calls.clear();
    }
}
