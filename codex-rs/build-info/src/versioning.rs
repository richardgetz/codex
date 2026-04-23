pub(crate) const LOCAL_DEV_BUILD_VERSION: &str = "0.0.0";
pub(crate) const DEFAULT_SOURCE_VERSION_SUFFIX: &str = "rick";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DerivedVersion {
    pub(crate) display_version: String,
    pub(crate) release_line_version: String,
    pub(crate) is_source_build: bool,
}

pub(crate) fn derive_version(
    cargo_version: &str,
    profile: Option<&str>,
    source_build_from_release_branch: bool,
    source_base_override: Option<&str>,
    official_release_version: Option<&str>,
    git_release_version: Option<&str>,
    installed_release_version: Option<&str>,
    source_version_suffix: Option<&str>,
) -> DerivedVersion {
    let is_local_dev_version = cargo_version == LOCAL_DEV_BUILD_VERSION;
    let is_source_build = is_local_dev_version || source_build_from_release_branch;
    let release_line_version = if is_local_dev_version {
        resolve_source_build_base_version(
            cargo_version,
            profile,
            source_base_override,
            official_release_version,
            git_release_version,
            installed_release_version,
        )
    } else {
        cargo_version.to_string()
    };
    let display_version = if is_source_build {
        format_source_display_version(&release_line_version, source_version_suffix)
    } else {
        cargo_version.to_string()
    };

    DerivedVersion {
        display_version,
        release_line_version,
        is_source_build,
    }
}

fn resolve_source_build_base_version(
    cargo_version: &str,
    profile: Option<&str>,
    source_base_override: Option<&str>,
    official_release_version: Option<&str>,
    git_release_version: Option<&str>,
    installed_release_version: Option<&str>,
) -> String {
    extract_semver_base(source_base_override.unwrap_or_default())
        .or_else(|| {
            (profile == Some("release")).then_some(()).and_then(|_| {
                official_release_version
                    .and_then(extract_semver_base)
                    .or_else(|| git_release_version.and_then(extract_semver_base))
                    .or_else(|| installed_release_version.and_then(extract_semver_base))
            })
        })
        .unwrap_or_else(|| cargo_version.to_string())
}

pub(crate) fn extract_semver_base(text: &str) -> Option<String> {
    text.char_indices().find_map(|(index, _)| {
        let prefix = &text[..index];
        let candidate = &text[index..];
        if prefix
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_ascii_alphanumeric())
        {
            return None;
        }

        extract_leading_simple_semver(candidate).or_else(|| {
            candidate
                .strip_prefix('v')
                .and_then(extract_leading_simple_semver)
        })
    })
}

fn extract_leading_simple_semver(text: &str) -> Option<String> {
    let mut end = 0usize;
    let mut dot_count = 0usize;

    for (index, ch) in text.char_indices() {
        if ch.is_ascii_digit() {
            end = index + ch.len_utf8();
            continue;
        }
        if ch == '.' {
            dot_count += 1;
            if dot_count > 2 {
                break;
            }
            end = index + ch.len_utf8();
            continue;
        }
        break;
    }

    let candidate = text.get(..end)?;
    is_simple_semver(candidate).then(|| candidate.to_string())
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

fn format_source_display_version(base_version: &str, suffix: Option<&str>) -> String {
    let Some(suffix) = suffix else {
        return base_version.to_string();
    };
    let trimmed = suffix.trim().trim_start_matches('-');
    if trimmed.is_empty() {
        return base_version.to_string();
    }

    format!("{base_version}-{trimmed}")
}
