use super::ScratchpadLoopbackLimiter;
use codex_config::types::ScratchpadLoopbackConfig;
use pretty_assertions::assert_eq;
use std::time::Duration;
use std::time::Instant;

fn config(max_loopbacks: usize, window: Duration) -> ScratchpadLoopbackConfig {
    ScratchpadLoopbackConfig {
        max_loopbacks,
        window,
    }
}

#[test]
fn allows_the_configured_count_then_blocks_until_the_window_expires() {
    let start = Instant::now();
    let mut limiter = ScratchpadLoopbackLimiter::default();
    let config = config(2, Duration::from_secs(5 * 60));

    assert!(limiter.try_record_at(start, config));
    assert!(limiter.try_record_at(start + Duration::from_secs(1), config));
    assert!(!limiter.try_record_at(start + Duration::from_secs(2), config));
    assert!(limiter.try_record_at(start + Duration::from_secs(5 * 60), config));
}

#[test]
fn expires_each_timestamp_independently_in_the_rolling_window() {
    let start = Instant::now();
    let mut limiter = ScratchpadLoopbackLimiter::default();
    let config = config(2, Duration::from_secs(5));

    assert!(limiter.try_record_at(start, config));
    assert!(limiter.try_record_at(start + Duration::from_secs(4), config));
    assert!(limiter.try_record_at(start + Duration::from_secs(5), config));
    assert!(!limiter.try_record_at(start + Duration::from_secs(8), config));
    assert!(limiter.try_record_at(start + Duration::from_secs(9), config));
    assert_eq!(limiter.timestamps.len(), 2);
}

#[test]
fn changing_the_config_starts_a_fresh_window() {
    let start = Instant::now();
    let mut limiter = ScratchpadLoopbackLimiter::default();
    let initial = config(2, Duration::from_secs(5 * 60));
    let changed = config(2, Duration::from_secs(10 * 60));

    assert!(limiter.try_record_at(start, initial));
    assert!(limiter.try_record_at(start + Duration::from_secs(1), initial));
    assert!(!limiter.try_record_at(start + Duration::from_secs(2), initial));
    assert!(limiter.try_record_at(start + Duration::from_secs(2), changed));
    assert!(limiter.try_record_at(start + Duration::from_secs(3), changed));
    assert!(!limiter.try_record_at(start + Duration::from_secs(4), changed));
}
