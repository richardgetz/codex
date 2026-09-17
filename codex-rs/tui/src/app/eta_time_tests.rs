use super::EtaTimestampFormatter;
use crate::legacy_core::config::EtaConfig;
use jiff::Timestamp;
use jiff::tz::TimeZone;

fn config(use_local_timezone: bool, timezone: Option<&str>) -> EtaConfig {
    EtaConfig {
        freshness_minimum_minutes: 15,
        use_local_timezone,
        timezone: timezone.map(str::to_string),
    }
}

fn timestamp(value: &str) -> i64 {
    value
        .parse::<Timestamp>()
        .expect("valid timestamp")
        .as_second()
}

#[test]
fn default_formatter_renders_utc() {
    let formatter = EtaTimestampFormatter::from_config_with_system_timezone(
        &config(/*use_local_timezone*/ false, /*timezone*/ None),
        TimeZone::get("America/New_York").expect("known time zone"),
    );
    assert_eq!(
        formatter.format(timestamp("2023-11-14T22:13:20Z")),
        "2023-11-14 22:13:20 UTC"
    );
}

#[test]
fn local_mode_uses_injected_system_timezone_without_environment_changes() {
    let formatter = EtaTimestampFormatter::from_config_with_system_timezone(
        &config(/*use_local_timezone*/ true, /*timezone*/ None),
        TimeZone::get("America/Los_Angeles").expect("known time zone"),
    );
    assert_eq!(
        formatter.format(timestamp("2023-11-14T22:13:20Z")),
        "2023-11-14 14:13:20 PST"
    );
}

#[test]
fn named_timezone_takes_precedence_and_applies_dst() {
    let formatter = EtaTimestampFormatter::from_config_with_system_timezone(
        &config(
            /*use_local_timezone*/ true,
            /*timezone*/ Some("America/New_York"),
        ),
        TimeZone::get("Asia/Tokyo").expect("known time zone"),
    );
    assert_eq!(
        formatter.format(timestamp("2024-03-10T06:30:00Z")),
        "2024-03-10 01:30:00 EST"
    );
    assert_eq!(
        formatter.format(timestamp("2024-03-10T07:30:00Z")),
        "2024-03-10 03:30:00 EDT"
    );
}

#[test]
fn invalid_named_timezone_falls_back_to_utc_for_unvalidated_runtime_config() {
    let formatter = EtaTimestampFormatter::from_config_with_system_timezone(
        &config(
            /*use_local_timezone*/ false,
            /*timezone*/ Some("Mars/Olympus"),
        ),
        TimeZone::get("America/New_York").expect("known time zone"),
    );
    assert_eq!(
        formatter.format(timestamp("2023-11-14T22:13:20Z")),
        "2023-11-14 22:13:20 UTC"
    );
}
