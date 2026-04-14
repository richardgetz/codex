use std::process::Command;

const LOCAL_DEV_BUILD_VERSION: &str = "0.0.0";

fn main() {
    println!("cargo:rerun-if-env-changed=CODEX_SOURCE_BASE_VERSION");
    println!("cargo:rerun-if-env-changed=CODEX_SOURCE_VERSION_SUFFIX");

    let cargo_version = std::env::var("CARGO_PKG_VERSION")
        .ok()
        .filter(|version| !version.trim().is_empty())
        .unwrap_or_else(|| LOCAL_DEV_BUILD_VERSION.to_string());

    let is_source_build = cargo_version == LOCAL_DEV_BUILD_VERSION;
    let base_version = if is_source_build {
        source_build_base_version().unwrap_or_else(|| cargo_version.clone())
    } else {
        cargo_version.clone()
    };
    let display_version = if is_source_build {
        format_source_display_version(
            &base_version,
            std::env::var("CODEX_SOURCE_VERSION_SUFFIX").ok(),
        )
    } else {
        cargo_version
    };

    println!("cargo:rustc-env=CODEX_DISPLAY_VERSION={display_version}");
    println!("cargo:rustc-env=CODEX_RELEASE_LINE_VERSION={base_version}");
    println!("cargo:rustc-env=CODEX_IS_SOURCE_BUILD={is_source_build}");
}

fn source_build_base_version() -> Option<String> {
    if let Ok(override_version) = std::env::var("CODEX_SOURCE_BASE_VERSION")
        && let Some(version) = extract_semver_base(&override_version)
    {
        return Some(version);
    }

    if std::env::var("PROFILE").ok().as_deref() != Some("release") {
        return None;
    }

    installed_codex_semver_base()
}

fn installed_codex_semver_base() -> Option<String> {
    for executable in ["codex", "codex-agent"] {
        let output = match Command::new(executable).arg("--version").output() {
            Ok(output) if output.status.success() => output,
            _ => continue,
        };

        if let Ok(stdout) = String::from_utf8(output.stdout)
            && let Some(version) = extract_semver_base(&stdout)
        {
            return Some(version);
        }
    }

    None
}

fn extract_semver_base(text: &str) -> Option<String> {
    text.split_whitespace().find_map(|word| {
        let candidate = word.strip_prefix('v').unwrap_or(word);
        let base = candidate
            .split_once(['-', '+'])
            .map_or(candidate, |(prefix, _)| prefix);
        is_simple_semver(base).then(|| base.to_owned())
    })
}

fn is_simple_semver(candidate: &str) -> bool {
    let mut parts = candidate.split('.');
    let major = parts.next();
    let minor = parts.next();
    let patch = parts.next();

    major.is_some_and(all_ascii_digits)
        && minor.is_some_and(all_ascii_digits)
        && patch.is_some_and(all_ascii_digits)
        && parts.next().is_none()
}

fn all_ascii_digits(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
}

fn format_source_display_version(base_version: &str, suffix: Option<String>) -> String {
    let Some(suffix) = suffix else {
        return base_version.to_string();
    };
    let trimmed = suffix.trim().trim_start_matches('-');
    if trimmed.is_empty() {
        return base_version.to_string();
    }

    format!("{base_version}-{trimmed}")
}
