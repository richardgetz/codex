use super::OrchestratorSupervisionStore;
use crate::agent::AgentStatus;
use codex_protocol::ThreadId;
use tempfile::TempDir;

fn thread_id(value: &str) -> ThreadId {
    ThreadId::from_string(value).expect("valid thread id")
}

#[tokio::test]
async fn watched_session_events_update_poll_state() {
    let tmp = TempDir::new().expect("tempdir");
    let codex_home = codex_utils_absolute_path::AbsolutePathBuf::try_from(tmp.path().to_path_buf())
        .expect("absolute path");
    let store = OrchestratorSupervisionStore::new(codex_home);
    let orchestrator = thread_id("019dbc89-81eb-7300-a9a7-8db90bfa4f1f");
    let target = thread_id("019dbfd0-6c49-7623-bcd3-6d43a46d5916");

    store
        .note_watched_session_event(
            orchestrator,
            target,
            &AgentStatus::Completed(Some("PR is ready".to_string())),
        )
        .await
        .expect("record watched session completion");

    let poll_state = store
        .poll_state(orchestrator)
        .await
        .expect("poll state after completion");
    assert!(poll_state.has_supervised_workers);
    assert!(!poll_state.has_nonterminal_workers);
}
