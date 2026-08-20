use std::fs;

use super::CargoProfile;
use super::build_spec;
use super::can_build;

#[test]
fn recognizes_the_checkout_debug_target() {
    let crate_dir = fs::canonicalize(env!("CARGO_MANIFEST_DIR"))
        .expect("code-mode crate directory should exist");
    let manifest_dir = crate_dir.parent().expect("workspace directory");
    let host_program = manifest_dir.join("target/debug/codex-code-mode-host");
    let build = build_spec(&host_program).expect("checkout host should be auto-buildable");

    assert_eq!(build.manifest_dir, manifest_dir);
    assert_eq!(build.profile, CargoProfile::Debug);
    assert!(can_build(&host_program));
}

#[test]
fn recognizes_a_cross_target_checkout_target() {
    let crate_dir = fs::canonicalize(env!("CARGO_MANIFEST_DIR"))
        .expect("code-mode crate directory should exist");
    let manifest_dir = crate_dir.parent().expect("workspace directory");
    let host_program = manifest_dir.join("target/aarch64-apple-darwin/debug/codex-code-mode-host");
    let build = build_spec(&host_program).expect("cross-target host should be auto-buildable");

    assert_eq!(build.target_dir, manifest_dir.join("target"));
    assert_eq!(build.target.as_deref(), Some("aarch64-apple-darwin"));
}

#[test]
fn recognizes_a_custom_target_directory() {
    let crate_dir = fs::canonicalize(env!("CARGO_MANIFEST_DIR"))
        .expect("code-mode crate directory should exist");
    let manifest_dir = crate_dir.parent().expect("workspace directory");
    let host_program = manifest_dir.join("target/custom-target/debug/codex-code-mode-host");
    let build = build_spec(&host_program).expect("custom-target host should be auto-buildable");

    assert_eq!(build.target.as_deref(), Some("custom-target"));
}

#[test]
fn does_not_auto_build_an_unrelated_missing_host() {
    assert!(!can_build("codex-code-mode-host-does-not-exist".as_ref()));
}
