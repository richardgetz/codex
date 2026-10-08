use super::latest_owned_settings_from_rollout;
use anyhow::Result;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_rollout::InitialHistory;
use codex_rollout::ResumedHistory;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::io::Write;
use std::sync::Arc;

fn thread_id(value: &str) -> ThreadId {
    ThreadId::from_string(value).expect("valid thread id")
}

fn settings_event(
    thread_id: Option<ThreadId>,
    disabled_plugin_id: &str,
) -> ThreadSettingsAppliedEvent {
    ThreadSettingsAppliedEvent {
        thread_id,
        thread_settings: ThreadSettingsSnapshot {
            model: "gpt-5".to_string(),
            model_provider_id: "openai".to_string(),
            service_tier: None,
            approval_policy: AskForApproval::Never,
            approvals_reviewer: ApprovalsReviewer::User,
            permission_profile: PermissionProfile::read_only(),
            active_permission_profile: None,
            cwd: AbsolutePathBuf::try_from(std::env::current_dir().expect("current directory"))
                .expect("absolute current directory"),
            runtime_workspace_roots: None,
            reasoning_effort: None,
            reasoning_summary: None,
            personality: None,
            collaboration_mode: CollaborationMode {
                mode: ModeKind::Default,
                settings: Settings {
                    model: "gpt-5".to_string(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            },
            memory_policy: Default::default(),
            user_preferences_memory_policy: Default::default(),
            usage_policy: Default::default(),
            team: None,
            disabled_plugin_ids: vec![disabled_plugin_id.to_string()],
        },
    }
}

#[tokio::test]
async fn paginated_recovery_finds_latest_owned_settings_outside_model_context() -> Result<()> {
    let owned_thread_id = thread_id("00000000-0000-7000-8000-000000000001");
    let foreign_thread_id = thread_id("00000000-0000-7000-8000-000000000002");
    let old_owned_event = settings_event(Some(owned_thread_id), "older-owned");
    let latest_owned_event = settings_event(Some(owned_thread_id), "latest-owned");
    let newer_foreign_event = settings_event(Some(foreign_thread_id), "newer-foreign");
    let newer_unowned_legacy_event = settings_event(None, "newer-unowned-legacy");
    let temp_dir = tempfile::tempdir()?;
    let rollout_path = temp_dir.path().join("rollout.jsonl");
    let mut rollout = std::fs::File::create(&rollout_path)?;
    for event in [
        old_owned_event,
        latest_owned_event.clone(),
        newer_foreign_event,
        newer_unowned_legacy_event,
    ] {
        let line = RolloutLine {
            timestamp: "2026-10-02T12:00:00Z".to_string(),
            ordinal: None,
            item: RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)),
        };
        writeln!(rollout, "{}", serde_json::to_string(&line)?)?;
    }

    // This is the selected paginated model context; the old settings snapshots are outside it.
    let initial_history = InitialHistory::Resumed(ResumedHistory {
        conversation_id: owned_thread_id,
        history: Arc::new(Vec::new()),
        history_revision: None,
        rollout_path: Some(rollout_path.clone()),
    });
    let InitialHistory::Resumed(resumed) = &initial_history else {
        unreachable!("constructed resumed history")
    };
    assert!(resumed.history.is_empty());

    assert_eq!(
        latest_owned_settings_from_rollout(rollout_path, owned_thread_id).await?,
        Some(latest_owned_event)
    );
    assert!(resumed.history.is_empty());
    Ok(())
}
