//! Formatting and configuration helpers for the `/tmp` command.

use crate::legacy_core::config::Config;
use codex_session_tmp::CleanupReport;
use codex_session_tmp::SessionTmpConfig;
use codex_session_tmp::SessionTmpListing;
use codex_utils_absolute_path::AbsolutePathBuf;

pub(crate) const USAGE: &str = "Usage: /tmp [status|list|clean|clear|reap [days]]";

pub(crate) fn config(config: &Config) -> SessionTmpConfig {
    SessionTmpConfig {
        enabled: config.session_tmp.enabled,
        root: config
            .session_tmp
            .root
            .as_ref()
            .map(AbsolutePathBuf::to_path_buf),
        stale_after: config.session_tmp.stale_after,
    }
}

pub(crate) fn status_message(listing: &SessionTmpListing) -> String {
    format!(
        "Session temporary storage is enabled.\nSession: {}\nAgent root: {}\nTracked entries: {}\nUntracked paths: {}",
        listing.session_id,
        listing.agent_root.display(),
        listing.entries.len(),
        listing.untracked_paths.len(),
    )
}

pub(crate) fn cleanup_message(action: &str, report: &CleanupReport) -> String {
    format!(
        "Session temporary {action} complete: removed {} path(s), preserved {} path(s), removed {} session(s).",
        report.removed_paths, report.preserved_paths, report.removed_sessions,
    )
}
