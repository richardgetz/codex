//! Time zone aware formatting for ETA timestamps.
//!
//! ETA timestamps remain Unix seconds in the app-server protocol and persisted state. This
//! formatter only chooses the zone used while rendering those values in a client view, so it can
//! also be shared by future timestamp based views such as All Sessions.

use crate::legacy_core::config::EtaConfig;
use jiff::Timestamp;
use jiff::tz::TimeZone;

const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S %Z";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EtaTimestampFormatter {
    timezone: TimeZone,
}

impl EtaTimestampFormatter {
    pub(super) fn utc() -> Self {
        Self::with_timezone(TimeZone::UTC)
    }

    pub(super) fn with_timezone(timezone: TimeZone) -> Self {
        Self { timezone }
    }

    pub(super) fn from_config(config: &EtaConfig) -> Self {
        if let Some(timezone) = config
            .timezone
            .as_deref()
            .and_then(|name| TimeZone::get(name).ok())
        {
            return Self::with_timezone(timezone);
        }
        if config.use_local_timezone {
            return Self::with_timezone(TimeZone::try_system().unwrap_or(TimeZone::UTC));
        }
        Self::utc()
    }

    pub(super) fn from_config_with_system_timezone(
        config: &EtaConfig,
        system_timezone: TimeZone,
    ) -> Self {
        if let Some(timezone) = config
            .timezone
            .as_deref()
            .and_then(|name| TimeZone::get(name).ok())
        {
            return Self::with_timezone(timezone);
        }
        if config.use_local_timezone {
            return Self::with_timezone(system_timezone);
        }
        Self::utc()
    }

    pub(super) fn format(&self, seconds: i64) -> String {
        let Ok(timestamp) = Timestamp::from_second(seconds) else {
            return format!("unix {seconds}");
        };
        timestamp
            .to_zoned(self.timezone.clone())
            .strftime(TIMESTAMP_FORMAT)
            .to_string()
    }
}

#[cfg(test)]
#[path = "eta_time_tests.rs"]
mod tests;
