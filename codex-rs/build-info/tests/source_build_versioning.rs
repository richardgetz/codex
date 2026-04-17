#[path = "../src/versioning.rs"]
mod versioning;

use versioning::DEFAULT_SOURCE_VERSION_SUFFIX;
use versioning::DerivedVersion;
use versioning::LOCAL_DEV_BUILD_VERSION;
use versioning::derive_version;
use versioning::extract_semver_base;

#[test]
fn extracts_semver_from_rust_release_tags() {
    assert_eq!(
        extract_semver_base("rust-v0.120.0"),
        Some("0.120.0".to_string())
    );
    assert_eq!(
        extract_semver_base("rust-v0.120.0-alpha.1"),
        Some("0.120.0".to_string())
    );
}

#[test]
fn extracts_semver_from_codex_version_output() {
    assert_eq!(
        extract_semver_base("codex-cli 0.120.0-rick"),
        Some("0.120.0".to_string())
    );
    assert_eq!(
        extract_semver_base("OpenAI Codex (v0.120.0-rick)"),
        Some("0.120.0".to_string())
    );
}

#[test]
fn source_release_build_prefers_mainline_version_and_suffix() {
    let derived = derive_version(
        LOCAL_DEV_BUILD_VERSION,
        Some("release"),
        false,
        None,
        Some("rust-v0.120.0"),
        None,
        Some("codex-cli 0.0.0-rick"),
        Some(DEFAULT_SOURCE_VERSION_SUFFIX),
    );

    assert_eq!(
        derived,
        DerivedVersion {
            display_version: "0.120.0-rick".to_string(),
            release_line_version: "0.120.0".to_string(),
            is_source_build: true,
        }
    );
}

#[test]
fn source_release_build_uses_git_release_when_network_and_installed_fallbacks_fail() {
    let derived = derive_version(
        LOCAL_DEV_BUILD_VERSION,
        Some("release"),
        false,
        None,
        None,
        Some("rust-v0.119.0"),
        Some("codex-cli 0.0.0-rick"),
        Some(DEFAULT_SOURCE_VERSION_SUFFIX),
    );

    assert_eq!(
        derived,
        DerivedVersion {
            display_version: "0.119.0-rick".to_string(),
            release_line_version: "0.119.0".to_string(),
            is_source_build: true,
        }
    );
}

#[test]
fn source_release_build_uses_installed_mainline_version_for_wrapped_installs() {
    let derived = derive_version(
        LOCAL_DEV_BUILD_VERSION,
        Some("release"),
        false,
        None,
        None,
        None,
        Some("codex-cli 0.118.0"),
        Some(DEFAULT_SOURCE_VERSION_SUFFIX),
    );

    assert_eq!(
        derived,
        DerivedVersion {
            display_version: "0.118.0-rick".to_string(),
            release_line_version: "0.118.0".to_string(),
            is_source_build: true,
        }
    );
}

#[test]
fn release_line_source_branch_build_appends_suffix() {
    let derived = derive_version(
        "0.122.0",
        Some("release"),
        true,
        None,
        None,
        None,
        None,
        Some(DEFAULT_SOURCE_VERSION_SUFFIX),
    );

    assert_eq!(
        derived,
        DerivedVersion {
            display_version: "0.122.0-rick".to_string(),
            release_line_version: "0.122.0".to_string(),
            is_source_build: true,
        }
    );
}

#[test]
fn release_line_source_branch_build_supports_numeric_fork_suffix() {
    let derived = derive_version(
        "0.122.0",
        Some("release"),
        true,
        None,
        None,
        None,
        None,
        Some("rick.2"),
    );

    assert_eq!(
        derived,
        DerivedVersion {
            display_version: "0.122.0-rick.2".to_string(),
            release_line_version: "0.122.0".to_string(),
            is_source_build: true,
        }
    );
}

#[test]
fn exact_release_tag_build_keeps_plain_version() {
    let derived = derive_version(
        "0.122.0",
        Some("release"),
        false,
        None,
        None,
        None,
        None,
        Some(DEFAULT_SOURCE_VERSION_SUFFIX),
    );

    assert_eq!(
        derived,
        DerivedVersion {
            display_version: "0.122.0".to_string(),
            release_line_version: "0.122.0".to_string(),
            is_source_build: false,
        }
    );
}
