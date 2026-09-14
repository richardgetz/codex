use std::path::PathBuf;

use pretty_assertions::assert_eq;

use super::retain_launch_identity;
use super::super::LaunchIdentity;
use crate::managed_install::executable_identity_from_bytes;

#[test]
fn launch_identity_is_retained_when_launcher_generation_is_stable() {
    assert_eq!(
        retain_launch_identity(
            PathBuf::from("/opt/homebrew/bin/codex-rick"),
            Some("0.154.0-rick.6".to_string()),
            Some(executable_identity_from_bytes(b"rick.6")),
            Some("0.154.0-rick.6".to_string()),
            Some(executable_identity_from_bytes(b"rick.6")),
        ),
        LaunchIdentity {
            path: PathBuf::from("/opt/homebrew/bin/codex-rick"),
            version: Some("0.154.0-rick.6".to_string()),
        }
    );
}

#[test]
fn launch_identity_is_unknown_when_shim_changes_between_capture_and_spawn() {
    assert_eq!(
        retain_launch_identity(
            PathBuf::from("/opt/homebrew/bin/codex-rick"),
            Some("0.154.0-rick.6".to_string()),
            Some(executable_identity_from_bytes(b"rick.6")),
            Some("0.154.0-rick.7".to_string()),
            Some(executable_identity_from_bytes(b"rick.7")),
        ),
        LaunchIdentity {
            path: PathBuf::from("/opt/homebrew/bin/codex-rick"),
            version: None,
        }
    );
}

#[test]
fn launch_identity_is_unknown_when_version_or_file_identity_cannot_be_verified() {
    assert_eq!(
        retain_launch_identity(
            PathBuf::from("/opt/homebrew/bin/codex-rick"),
            Some("0.154.0-rick.6".to_string()),
            None,
            Some("0.154.0-rick.6".to_string()),
            None,
        )
        .version,
        None
    );
}
