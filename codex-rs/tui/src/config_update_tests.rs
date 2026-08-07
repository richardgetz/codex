use super::*;
use color_eyre::eyre::WrapErr;
use pretty_assertions::assert_eq;
use std::path::Path;

#[test]
fn app_scoped_key_path_quotes_dotted_app_ids() {
    assert_eq!(
        app_scoped_key_path("plugin.linear", "enabled"),
        "apps.\"plugin.linear\".enabled"
    );
}

#[test]
fn trusted_project_edit_targets_project_trust_level() {
    assert_eq!(
        trusted_project_edit(Path::new("/workspace/team.project")),
        ConfigEdit {
            key_path: "projects.\"/workspace/team.project\".trust_level".to_string(),
            value: serde_json::json!("trusted"),
            merge_strategy: MergeStrategy::Replace,
        }
    );
}

#[test]
fn realtime_controls_write_to_their_config_paths() {
    assert_eq!(
        build_realtime_microphone_edit("Clip-On Mic"),
        ConfigEdit {
            key_path: "audio.microphone".to_string(),
            value: serde_json::json!("Clip-On Mic"),
            merge_strategy: MergeStrategy::Replace,
        }
    );
    assert_eq!(
        build_realtime_voice_edit("arbor"),
        ConfigEdit {
            key_path: "realtime.voice".to_string(),
            value: serde_json::json!("arbor"),
            merge_strategy: MergeStrategy::Replace,
        }
    );
    assert_eq!(
        build_realtime_hotkey_edit("f13"),
        ConfigEdit {
            key_path: "realtime.hotkey".to_string(),
            value: serde_json::json!("f13"),
            merge_strategy: MergeStrategy::Replace,
        }
    );
}

#[test]
fn format_config_error_preserves_server_validation_message() {
    let err = Err::<(), _>(color_eyre::eyre::eyre!(
        "config/batchWrite failed: Invalid configuration: features.fast_mode=true violates \
         managed requirements; allowed set [fast_mode=false]"
    ))
    .wrap_err("config/batchWrite failed in TUI")
    .unwrap_err();

    assert_eq!(
        format_config_error(&err),
        "config/batchWrite failed in TUI: config/batchWrite failed: Invalid configuration: \
         features.fast_mode=true violates managed requirements; allowed set [fast_mode=false]"
    );
}
