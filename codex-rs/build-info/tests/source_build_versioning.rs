#[path = "../src/versioning.rs"]
mod versioning;

use versioning::DEFAULT_SOURCE_VERSION_SUFFIX;
use versioning::DerivedVersion;
use versioning::LOCAL_DEV_BUILD_VERSION;
use versioning::VersionDerivationInputs;
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
    let derived = derive_version(VersionDerivationInputs {
        cargo_version: LOCAL_DEV_BUILD_VERSION,
        profile: Some("release"),
        official_release_version: Some("rust-v0.120.0"),
        installed_release_version: Some("codex-cli 0.0.0-rick"),
        source_version_suffix: Some(DEFAULT_SOURCE_VERSION_SUFFIX),
        ..Default::default()
    });

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
    let derived = derive_version(VersionDerivationInputs {
        cargo_version: LOCAL_DEV_BUILD_VERSION,
        profile: Some("release"),
        git_release_version: Some("rust-v0.119.0"),
        installed_release_version: Some("codex-cli 0.0.0-rick"),
        source_version_suffix: Some(DEFAULT_SOURCE_VERSION_SUFFIX),
        ..Default::default()
    });

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
    let derived = derive_version(VersionDerivationInputs {
        cargo_version: LOCAL_DEV_BUILD_VERSION,
        profile: Some("release"),
        installed_release_version: Some("codex-cli 0.118.0"),
        source_version_suffix: Some(DEFAULT_SOURCE_VERSION_SUFFIX),
        ..Default::default()
    });

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
    let derived = derive_version(VersionDerivationInputs {
        cargo_version: "0.122.0",
        profile: Some("release"),
        source_build_from_release_branch: true,
        source_version_suffix: Some(DEFAULT_SOURCE_VERSION_SUFFIX),
        ..Default::default()
    });

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
    let derived = derive_version(VersionDerivationInputs {
        cargo_version: "0.122.0",
        profile: Some("release"),
        source_build_from_release_branch: true,
        source_version_suffix: Some("rick.2"),
        ..Default::default()
    });

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
    let derived = derive_version(VersionDerivationInputs {
        cargo_version: "0.122.0",
        profile: Some("release"),
        source_version_suffix: Some(DEFAULT_SOURCE_VERSION_SUFFIX),
        ..Default::default()
    });

    assert_eq!(
        derived,
        DerivedVersion {
            display_version: "0.122.0".to_string(),
            release_line_version: "0.122.0".to_string(),
            is_source_build: false,
        }
    );
}
