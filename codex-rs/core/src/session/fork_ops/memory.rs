use super::super::session::Session;
use crate::config::Config;
use codex_protocol::config_types::MemoryAccessPolicy;
use codex_protocol::config_types::UserPreferencesMemoryBucket;
use codex_protocol::config_types::UserPreferencesMemoryBucketPolicy;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use std::sync::Arc;
use tracing::warn;

pub(in crate::session) fn consolidate_orchestrator_memory(
    session: &Arc<Session>,
    config: &Arc<Config>,
    submission_id: String,
) {
    let Ok(handoff_admission) = session
        .services
        .local_agent_control()
        .begin_handoff_admission()
    else {
        return;
    };
    let session = Arc::clone(session);
    let config = Arc::clone(config);
    tokio::spawn(async move {
        let _handoff_admission = handoff_admission;
        match crate::orchestrator_memory::run_cleanup_now_for_session(&session, &config).await {
            Ok(result) => {
                session
                    .send_event_raw(Event {
                        id: submission_id,
                        msg: EventMsg::Warning(WarningEvent {
                            message: format!(
                                "Orchestrator memory consolidation completed. Raw events: {} -> {} (removed {}).",
                                result.raw_events_before,
                                result.raw_events_after,
                                result.removed_raw_events
                            ),
                        }),
                    })
                    .await;
            }
            Err(error) => {
                session
                    .send_event_raw(Event {
                        id: submission_id,
                        msg: EventMsg::Error(ErrorEvent {
                            misalignment: None,
                            message: format!("Failed to consolidate orchestrator memory: {error}"),
                            codex_error_info: Some(CodexErrorInfo::Other),
                        }),
                    })
                    .await;
            }
        }
    });
}

pub(in crate::session) fn forget_orchestrator_memory(
    session: &Arc<Session>,
    config: &Arc<Config>,
    submission_id: String,
    needle: String,
) {
    let Ok(handoff_admission) = session
        .services
        .local_agent_control()
        .begin_handoff_admission()
    else {
        return;
    };
    let session = Arc::clone(session);
    let config = Arc::clone(config);
    tokio::spawn(async move {
        let _handoff_admission = handoff_admission;
        let Some(_permit) = session.memory_write_permit().await else {
            session
                .send_event_raw(Event {
                    id: submission_id,
                    msg: EventMsg::Error(ErrorEvent {
                        misalignment: None,
                        message: "Memory writes are disabled for this session; enable memory generation before editing user preferences memory.".to_string(),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                })
                .await;
            return;
        };

        let bucket_policy = session.user_preferences_memory_policy().await;
        if !UserPreferencesMemoryBucket::all()
            .iter()
            .copied()
            .all(|bucket| bucket_policy.can_write(bucket))
        {
            session
                .send_event_raw(Event {
                    id: submission_id,
                    msg: EventMsg::Error(ErrorEvent {
                        misalignment: None,
                        message: "Orchestrator memory forget requires write access to all user-preferences memory buckets; widen this session's userPreferencesMemoryPolicy.writeBuckets before running this global maintenance command.".to_string(),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                })
                .await;
            return;
        }

        match crate::orchestrator_memory::prune_entries_matching_needle(
            &config.codex_home,
            &config.orchestrator_memory,
            &needle,
        )
        .await
        {
            Ok(result) => {
                if let Err(error) =
                    crate::orchestrator_memory::sync_memory_forget(&session, &needle).await
                {
                    warn!("failed synchronizing forgotten preference boundaries: {error:#}");
                }
                session
                    .send_event_raw(Event {
                        id: submission_id,
                        msg: EventMsg::Warning(WarningEvent {
                            message: format!(
                                "Orchestrator memory forget completed for `{needle}`. Removed preference events: {}; summary lines: {}; profile lines: {}.",
                                result.removed_preference_events,
                                result.removed_summary_lines,
                                result.removed_profile_lines
                            ),
                        }),
                    })
                    .await;
            }
            Err(error) => {
                session
                    .send_event_raw(Event {
                        id: submission_id,
                        msg: EventMsg::Error(ErrorEvent {
                            misalignment: None,
                            message: format!("Orchestrator memory forget failed: {error}"),
                            codex_error_info: Some(CodexErrorInfo::Other),
                        }),
                    })
                    .await;
            }
        }
    });
}

pub(in crate::session) fn migrate_user_preferences_memory(
    session: &Arc<Session>,
    config: &Arc<Config>,
    submission_id: String,
) {
    let Ok(handoff_admission) = session
        .services
        .local_agent_control()
        .begin_handoff_admission()
    else {
        return;
    };
    let session = Arc::clone(session);
    let config = Arc::clone(config);
    tokio::spawn(async move {
        let _handoff_admission = handoff_admission;
        let Some(_permit) = session.memory_write_permit().await else {
            session
                .send_event_raw(Event {
                    id: submission_id,
                    msg: EventMsg::Error(ErrorEvent {
                        misalignment: None,
                        message: "Memory writes are disabled for this session; enable memory generation before migrating user preferences memory.".to_string(),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                })
                .await;
            return;
        };

        match crate::orchestrator_memory::migrate_orchestrator_memory_to_user_preferences(
            &config.codex_home,
        ) {
            Ok(true) => {
                session
                    .send_event_raw(Event {
                        id: submission_id,
                        msg: EventMsg::Warning(WarningEvent {
                            message: "User preferences memory migration completed.".to_string(),
                        }),
                    })
                    .await;
            }
            Ok(false) => {
                session
                    .send_event_raw(Event {
                        id: submission_id,
                        msg: EventMsg::Warning(WarningEvent {
                            message: "No legacy orchestrator memory files were found to migrate."
                                .to_string(),
                        }),
                    })
                    .await;
            }
            Err(error) => {
                session
                    .send_event_raw(Event {
                        id: submission_id,
                        msg: EventMsg::Error(ErrorEvent {
                            misalignment: None,
                            message: format!("User preferences memory migration failed: {error}"),
                            codex_error_info: Some(CodexErrorInfo::Other),
                        }),
                    })
                    .await;
            }
        }
    });
}

pub(in crate::session) async fn set_memory_access_policy(
    session: &Arc<Session>,
    submission_id: String,
    policy: MemoryAccessPolicy,
) {
    let msg = match session
        .update_settings(super::super::session::SessionSettingsUpdate {
            memory_policy: Some(policy),
            ..Default::default()
        })
        .await
    {
        Ok(_) => super::super::handlers::thread_settings_applied_event(session).await,
        Err(error) => {
            warn!("Failed to update memory access policy: {error}");
            EventMsg::Error(ErrorEvent {
                misalignment: None,
                message: error.to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            })
        }
    };
    session.send_event_raw(Event { id: submission_id, msg }).await;
}

pub(in crate::session) async fn set_user_preferences_memory_policy(
    session: &Arc<Session>,
    submission_id: String,
    policy: UserPreferencesMemoryBucketPolicy,
) {
    let msg = match session
        .update_settings(super::super::session::SessionSettingsUpdate {
            user_preferences_memory_policy: Some(policy),
            ..Default::default()
        })
        .await
    {
        Ok(_) => super::super::handlers::thread_settings_applied_event(session).await,
        Err(error) => {
            warn!("Failed to update user preferences memory policy: {error}");
            EventMsg::Error(ErrorEvent {
                misalignment: None,
                message: error.to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            })
        }
    };
    session.send_event_raw(Event { id: submission_id, msg }).await;
}
