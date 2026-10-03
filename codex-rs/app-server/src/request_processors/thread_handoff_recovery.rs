use super::ThreadRequestProcessor;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_core::NewThread;
use codex_core::config::ConfigOverrides;
use codex_protocol::ThreadId;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_rollout::InitialHistory;
use codex_rollout::ReverseJsonlScanner;
use codex_rollout::RolloutItem;
use codex_rollout::ScanOutcome;
use codex_rollout::open_rollout_seekable_reader;
use std::io;
use std::path::Path;
use std::path::PathBuf;

pub(super) async fn latest_owned_settings_from_rollout(
    path: PathBuf,
    thread_id: ThreadId,
) -> io::Result<Option<ThreadSettingsAppliedEvent>> {
    tokio::task::spawn_blocking(move || {
        latest_owned_settings_from_rollout_blocking(&path, thread_id)
    })
    .await
    .map_err(io::Error::other)?
}

fn latest_owned_settings_from_rollout_blocking(
    path: &Path,
    thread_id: ThreadId,
) -> io::Result<Option<ThreadSettingsAppliedEvent>> {
    let mut scanner = ReverseJsonlScanner::new(open_rollout_seekable_reader(path)?)?;
    while let Some(outcome) = scanner.scan_next_rollout_line()? {
        match outcome {
            ScanOutcome::Parsed(line) => {
                if let RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) = line.item
                    && event.thread_id == Some(thread_id)
                {
                    return Ok(Some(event));
                }
            }
            ScanOutcome::Rejected(error) => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, error));
            }
        }
    }
    Ok(None)
}

pub(super) async fn resume_thread(
    processor: &ThreadRequestProcessor,
    initial_history: InitialHistory,
    thread_settings_event: Option<ThreadSettingsAppliedEvent>,
) -> Result<NewThread, JSONRPCErrorError> {
    let thread_id = match &initial_history {
        InitialHistory::Resumed(resumed) => resumed.conversation_id,
        InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => {
            return Err(crate::error_code::internal_error(
                "handoff recovery history did not identify a resumed thread".to_string(),
            ));
        }
    };
    let thread_settings_event =
        thread_settings_event.filter(|event| event.thread_id == Some(thread_id));
    let thread_settings_snapshot = thread_settings_event
        .as_ref()
        .map(|event| event.thread_settings.clone())
        .or_else(|| latest_owned_settings_in_history(&initial_history));
    let history_cwd = thread_settings_snapshot
        .as_ref()
        .map(|settings| settings.cwd.to_path_buf())
        .or_else(|| initial_history.session_cwd());
    let mut request_overrides = None;
    let mut typesafe_overrides = ConfigOverrides {
        workspace_roots: thread_settings_snapshot
            .as_ref()
            .and_then(|settings| settings.runtime_workspace_roots.clone()),
        ..ConfigOverrides::default()
    };
    let reasoning_effort_was_cleared = thread_settings_snapshot
        .as_ref()
        .map(|settings| settings.reasoning_effort.is_none())
        .unwrap_or_else(|| latest_reasoning_effort_was_cleared(&initial_history, thread_id));
    let persisted_metadata = processor
        .load_and_apply_persisted_resume_metadata(
            &initial_history,
            &mut request_overrides,
            &mut typesafe_overrides,
            thread_settings_snapshot.as_ref(),
        )
        .await;
    let mut config = processor
        .config_manager
        .load_for_cwd(request_overrides, typesafe_overrides, history_cwd)
        .await
        .map_err(|error| super::config_load_error(&error))?;
    if reasoning_effort_was_cleared
        && persisted_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.reasoning_effort.is_none())
    {
        config.model_reasoning_effort = None;
    }
    processor
        .thread_manager
        .resume_thread_with_history_and_settings(
            config,
            initial_history,
            processor.auth_manager.clone(),
            None,
            ClientMcpExtensions::default(),
            thread_settings_event,
        )
        .await
        .map_err(|error| super::thread_resume_error(error, &thread_id.to_string()))
}

fn latest_owned_settings_in_history(history: &InitialHistory) -> Option<ThreadSettingsSnapshot> {
    let InitialHistory::Resumed(resumed) = history else {
        return None;
    };
    resumed.history.iter().rev().find_map(|item| match item {
        RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event))
            if event.thread_id == Some(resumed.conversation_id) =>
        {
            Some(event.thread_settings.clone())
        }
        _ => None,
    })
}

fn latest_reasoning_effort_was_cleared(history: &InitialHistory, thread_id: ThreadId) -> bool {
    match history {
        InitialHistory::Resumed(resumed) => resumed
            .history
            .iter()
            .rev()
            .find_map(|item| match item {
                RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event))
                    if event.thread_id == Some(thread_id) =>
                {
                    Some(event.thread_settings.reasoning_effort.is_none())
                }
                _ => None,
            })
            .unwrap_or(false),
        InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => false,
    }
}

#[cfg(test)]
#[path = "thread_handoff_recovery_tests.rs"]
mod tests;
