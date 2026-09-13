use crate::settings::DaemonSettings;

use super::ensure_apply_launcher;

#[test]
fn apply_requires_an_explicit_launcher() {
    let settings = DaemonSettings {
        remote_control_enabled: false,
        managed_codex_path: None,
    };

    let error = ensure_apply_launcher(&settings).expect_err("standalone apply must be rejected");
    assert!(error.to_string().contains("bootstrap --codex-bin PATH"));
}
