use std::path::Path;
use std::path::PathBuf;

use pretty_assertions::assert_eq;

use super::super::LaunchIdentity;
use super::managed_app_server_codex_home;
use super::retain_launch_identity;
use crate::managed_install::executable_identity_from_reader;

#[test]
fn managed_app_server_home_comes_from_the_daemon_state_directory() {
    assert_eq!(
        managed_app_server_codex_home(Path::new("/home/user/.codex/app-server-daemon/daemon.pid"))
            .expect("daemon Codex home"),
        Path::new("/home/user/.codex")
    );
    assert_eq!(
        managed_app_server_codex_home(Path::new("/tmp/codex-home/daemon.pid"))
            .expect("test daemon Codex home"),
        Path::new("/tmp/codex-home")
    );
}

#[test]
fn launch_identity_is_retained_when_launcher_generation_is_stable() {
    assert_eq!(
        retain_launch_identity(
            PathBuf::from("/opt/homebrew/bin/codex-rick"),
            Some("0.154.0-rick.6".to_string()),
            Some(
                executable_identity_from_reader(&b"rick.6"[..]).expect("test executable identity"),
            ),
            Some("0.154.0-rick.6".to_string()),
            Some(
                executable_identity_from_reader(&b"rick.6"[..]).expect("test executable identity"),
            ),
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
            Some(
                executable_identity_from_reader(&b"rick.6"[..]).expect("test executable identity"),
            ),
            Some("0.154.0-rick.7".to_string()),
            Some(
                executable_identity_from_reader(&b"rick.7"[..]).expect("test executable identity"),
            ),
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
