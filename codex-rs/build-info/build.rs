use std::process::Command;

#[path = "src/versioning.rs"]
mod versioning;

const OPENAI_CODEX_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/openai/codex/releases/latest";

fn main() {
    println!("cargo:rerun-if-env-changed=CODEX_SOURCE_BASE_VERSION");
    println!("cargo:rerun-if-env-changed=CODEX_SOURCE_VERSION_SUFFIX");

    let cargo_version = std::env::var("CARGO_PKG_VERSION")
        .ok()
        .filter(|version| !version.trim().is_empty())
        .unwrap_or_else(|| versioning::LOCAL_DEV_BUILD_VERSION.to_string());

    let source_base_override = std::env::var("CODEX_SOURCE_BASE_VERSION").ok();
    let profile = std::env::var("PROFILE").ok();
    let is_release_profile = profile.as_deref() == Some("release");
    let is_local_dev_version = cargo_version == versioning::LOCAL_DEV_BUILD_VERSION;
    let is_git_checkout = in_git_worktree();
    let head_release_version = is_git_checkout.then(head_release_semver_base).flatten();
    let source_build_from_release_branch = is_release_profile
        && !is_local_dev_version
        && is_git_checkout
        && head_release_version.as_deref() != Some(cargo_version.as_str());
    let official_release_version = (is_release_profile && is_local_dev_version)
        .then(official_release_semver_base)
        .flatten();
    let git_release_version = (is_release_profile && is_local_dev_version)
        .then(local_repo_release_semver_base)
        .flatten();
    let installed_release_version = (is_release_profile && is_local_dev_version)
        .then(installed_codex_semver_base)
        .flatten();
    let source_version_suffix = source_version_suffix();
    let derived = versioning::derive_version(
        &cargo_version,
        profile.as_deref(),
        source_build_from_release_branch,
        source_base_override.as_deref(),
        official_release_version.as_deref(),
        git_release_version.as_deref(),
        installed_release_version.as_deref(),
        source_version_suffix.as_deref(),
    );

    println!(
        "cargo:rustc-env=CODEX_DISPLAY_VERSION={}",
        derived.display_version
    );
    println!(
        "cargo:rustc-env=CODEX_RELEASE_LINE_VERSION={}",
        derived.release_line_version
    );
    println!(
        "cargo:rustc-env=CODEX_IS_SOURCE_BUILD={}",
        derived.is_source_build
    );
}

fn in_git_worktree() -> bool {
    let output = match Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }

    String::from_utf8(output.stdout)
        .ok()
        .is_some_and(|stdout| stdout.trim() == "true")
}

fn official_release_semver_base() -> Option<String> {
    if let Some(version) = gh_latest_release_semver_base() {
        return Some(version);
    }

    curl_latest_release_semver_base()
}

fn head_release_semver_base() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--exact-match", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    versioning::extract_semver_base(&stdout)
}

fn gh_latest_release_semver_base() -> Option<String> {
    let output = Command::new("gh")
        .args([
            "release",
            "view",
            "--repo",
            "openai/codex",
            "--json",
            "tagName",
            "--jq",
            ".tagName",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    versioning::extract_semver_base(&stdout)
}

fn curl_latest_release_semver_base() -> Option<String> {
    let output = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            OPENAI_CODEX_LATEST_RELEASE_URL,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    versioning::extract_semver_base(&stdout)
}

fn local_repo_release_semver_base() -> Option<String> {
    let output = Command::new("git")
        .args(["tag", "--list", "--sort=-v:refname"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout.lines().find_map(versioning::extract_semver_base)
}

fn installed_codex_semver_base() -> Option<String> {
    for executable in ["codex-real", "codex", "codex-agent"] {
        let output = match Command::new(executable).arg("--version").output() {
            Ok(output) if output.status.success() => output,
            _ => continue,
        };

        if let Ok(stdout) = String::from_utf8(output.stdout)
            && let Some(version) = versioning::extract_semver_base(&stdout)
        {
            return Some(version);
        }
    }

    None
}

fn source_version_suffix() -> Option<String> {
    match std::env::var("CODEX_SOURCE_VERSION_SUFFIX") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => {
            Some(versioning::DEFAULT_SOURCE_VERSION_SUFFIX.to_string())
        }
        Err(std::env::VarError::NotUnicode(_)) => None,
    }
}
