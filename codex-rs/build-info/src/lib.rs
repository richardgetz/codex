/// User-facing Codex version string embedded at compile time.
///
/// Release builds use the published package version. Source builds may inherit
/// a release line from `CODEX_SOURCE_BASE_VERSION` or the currently installed
/// `codex` / `codex-agent` binary, and may append an optional suffix from
/// `CODEX_SOURCE_VERSION_SUFFIX`.
pub const CODEX_DISPLAY_VERSION: &str = env!("CODEX_DISPLAY_VERSION");

/// Stable release-line version used as the base for source builds.
pub const CODEX_RELEASE_LINE_VERSION: &str = env!("CODEX_RELEASE_LINE_VERSION");

/// Whether this binary was compiled from a source build instead of a published release.
pub fn is_source_build() -> bool {
    matches!(env!("CODEX_IS_SOURCE_BUILD"), "true")
}
