use super::persist_preflight_mailbox_diagnostics;
use codex_core::HandoffBlocker;
use codex_core::HandoffJournal;
use codex_core::HandoffJournalState;
use codex_core::HandoffNode;
use codex_core::HandoffNodeState;
use codex_core::PendingMailboxBlockerDetail;
use codex_core::PendingMailboxBlockerSource;

#[tokio::test]
async fn quarantine_preflight_resolution_is_durable_without_clearing_blockers() {
    let home = tempfile::tempdir().expect("temporary home");
    let mut journal = HandoffJournal::begin_with_pending_mailbox_diagnostics(
        home.path(),
        "test",
        vec![HandoffNode {
            thread_id: "thread".to_string(),
            root_thread_id: "thread".to_string(),
            parent_thread_id: None,
            agent_path: None,
            turn_id: None,
            rollout_path: None,
            was_running: false,
            was_paused: false,
            state: HandoffNodeState::NeedsAttention,
            blockers: vec![
                HandoffBlocker::PendingMailbox,
                HandoffBlocker::ActiveOperation,
            ],
        }],
        vec![PendingMailboxBlockerDetail {
            thread_id: "thread".to_string(),
            source: PendingMailboxBlockerSource::InterAgentMailbox,
            count: 1,
            oldest_age_ms: Some(10),
        }],
    )
    .await
    .expect("begin handoff");
    journal.set_state(HandoffJournalState::NeedsAttention);
    journal
        .persist(home.path())
        .await
        .expect("persist blocked receipt");

    // The queue has drained while another preflight blocker still makes quarantine return.
    persist_preflight_mailbox_diagnostics(home.path(), &mut journal, "thread", &[])
        .await
        .expect("persist queue resolution before returning the other blocker");

    let restored = HandoffJournal::load_all(home.path())
        .await
        .expect("reload handoff receipt");
    assert_eq!(restored.len(), 1);
    let restored = &restored[0];
    assert_eq!(restored.state, HandoffJournalState::NeedsAttention);
    assert_eq!(
        restored.nodes[0].blockers,
        vec![
            HandoffBlocker::PendingMailbox,
            HandoffBlocker::ActiveOperation,
        ]
    );
    assert!(restored.blocker_diagnostics[0].resolved_at_ms.is_some());
    assert_eq!(
        restored.blocker_diagnostics[0]
            .resolved_by_handoff_id
            .as_deref(),
        Some(restored.handoff_id.as_str())
    );
}
