use std::collections::VecDeque;
use std::time::Instant;

use codex_config::types::ScratchpadLoopbackConfig;

#[derive(Debug, Default)]
pub(crate) struct ScratchpadLoopbackLimiter {
    timestamps: VecDeque<Instant>,
    config: Option<ScratchpadLoopbackConfig>,
}

impl ScratchpadLoopbackLimiter {
    pub(crate) fn try_record_at(&mut self, now: Instant, config: ScratchpadLoopbackConfig) -> bool {
        if self.config != Some(config) {
            self.timestamps.clear();
            self.config = Some(config);
        }
        self.expire_before(now, config.window);
        if self.timestamps.len() >= config.max_loopbacks {
            return false;
        }
        self.timestamps.push_back(now);
        true
    }

    fn expire_before(&mut self, now: Instant, window: std::time::Duration) {
        while self
            .timestamps
            .front()
            .is_some_and(|timestamp| now.duration_since(*timestamp) >= window)
        {
            self.timestamps.pop_front();
        }
    }
}

#[cfg(test)]
#[path = "continuous_loopback_tests.rs"]
mod tests;
