use super::spawn_handoff_diagnostic_retention_task;
use codex_core::HandoffBlocker;
use codex_core::HandoffJournal;
use codex_core::HandoffJournalState;
use codex_core::HandoffNode;
use codex_core::HandoffNodeState;
use codex_core::PendingMailboxBlockerDetail;
use codex_core::PendingMailboxBlockerSource;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tempfile::tempdir;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio::time::timeout;

fn node(thread_id: &str) -> HandoffNode {
    HandoffNode {
        thread_id: thread_id.to_string(),
        root_thread_id: "root".to_string(),
        parent_thread_id: None,
        agent_path: None,
        turn_id: Some("turn".to_string()),
        rollout_path: Some("/tmp/thread.jsonl".to_string()),
        was_running: true,
        was_paused: false,
        state: HandoffNodeState::Planned,
        blockers: Vec::new(),
    }
}

async fn expired_journal(home: &Path) -> HandoffJournal {
    let mut journal = HandoffJournal::begin_with_pending_mailbox_diagnostics(
        home,
        "test",
        vec![node("thread")],
        vec![PendingMailboxBlockerDetail {
            thread_id: "thread".to_string(),
            source: PendingMailboxBlockerSource::InterAgentMailbox,
            count: 1,
            oldest_age_ms: Some(900),
        }],
    )
    .await
    .expect("create handoff journal");
    journal.blocker_diagnostics[0].last_observed_at_ms = 0;
    journal.persist(home).await.expect("persist expired row");
    journal
}

#[tokio::test]
async fn expired_rows_are_filtered_and_pruned_without_removing_recovery_state() {
    const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;

    let home = tempdir().expect("temporary CODEX_HOME");
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_millis();
    let now_ms = i64::try_from(now_ms).expect("Unix timestamp fits i64 milliseconds");
    let mut journal = HandoffJournal::begin_with_pending_mailbox_diagnostics(
        home.path(),
        "test",
        vec![node("thread"), node("resolved-thread")],
        vec![
            PendingMailboxBlockerDetail {
                thread_id: "thread".to_string(),
                source: PendingMailboxBlockerSource::InterAgentMailbox,
                count: 2,
                oldest_age_ms: Some(700),
            },
            PendingMailboxBlockerDetail {
                thread_id: "resolved-thread".to_string(),
                source: PendingMailboxBlockerSource::LeadOversightWake,
                count: 1,
                oldest_age_ms: Some(100),
            },
        ],
    )
    .await
    .expect("begin handoff with diagnostics");
    journal.blocker_diagnostics[0].last_observed_at_ms = 0;
    journal.blocker_diagnostics[1].last_observed_at_ms = now_ms - (2 * RETENTION_MS);
    journal.blocker_diagnostics[1].resolved_at_ms = Some(now_ms - RETENTION_MS + 60_000);
    journal.blocker_diagnostics[1].resolved_by_handoff_id = Some("later-handoff".to_string());
    journal
        .persist(home.path())
        .await
        .expect("persist old diagnostics");
    let journal_path = journal.path(home.path());

    let loaded = HandoffJournal::load_all(home.path())
        .await
        .expect("load filtered journal");
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].requires_recovery());
    assert_eq!(loaded[0].blocker_diagnostics.len(), 1);
    assert_eq!(
        loaded[0].blocker_diagnostics[0].thread_id,
        "resolved-thread"
    );

    let on_disk_before_cleanup: serde_json::Value = serde_json::from_slice(
        &tokio::fs::read(&journal_path)
            .await
            .expect("read original journal"),
    )
    .expect("parse original journal");
    assert_eq!(
        on_disk_before_cleanup["blockerDiagnostics"]
            .as_array()
            .expect("diagnostic rows on disk")
            .len(),
        2
    );

    assert_eq!(
        HandoffJournal::prune_expired_blocker_diagnostics(home.path())
            .await
            .expect("prune expired diagnostics"),
        1
    );
    assert!(
        journal_path.is_file(),
        "the recovery journal remains on disk"
    );
    let persisted: HandoffJournal = serde_json::from_slice(
        &tokio::fs::read(&journal_path)
            .await
            .expect("read pruned journal"),
    )
    .expect("parse pruned journal");
    assert!(persisted.requires_recovery());
    assert_eq!(persisted.handoff_id, journal.handoff_id);
    assert_eq!(persisted.state, HandoffJournalState::Prepared);
    assert_eq!(persisted.transfer_started, Some(false));
    assert_eq!(persisted.nodes, journal.nodes);
    assert_eq!(
        persisted.blocker_diagnostics,
        vec![journal.blocker_diagnostics[1].clone()]
    );
    assert_eq!(
        persisted.blocker_diagnostics[0].blocker,
        HandoffBlocker::PendingMailbox
    );
}

async fn wait_for_diagnostic_cleanup(path: &Path) {
    timeout(Duration::from_secs(3), async {
        loop {
            let bytes = tokio::fs::read(path).await.expect("read handoff journal");
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).expect("parse handoff journal");
            if value.get("blockerDiagnostics").is_none() {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("retention task should physically prune expired rows");
}

#[tokio::test]
async fn retention_task_prunes_on_startup_and_during_idle_operation() {
    let home = tempdir().expect("temporary CODEX_HOME");
    let startup_journal = expired_journal(home.path()).await;
    let operation = Arc::new(Mutex::new(()));
    let (shutdown, task) = spawn_handoff_diagnostic_retention_task(
        home.path().to_path_buf(),
        operation,
        Duration::from_millis(20),
    );

    wait_for_diagnostic_cleanup(&startup_journal.path(home.path())).await;

    let idle_journal = expired_journal(home.path()).await;
    wait_for_diagnostic_cleanup(&idle_journal.path(home.path())).await;

    let _ = shutdown.send(());
    timeout(Duration::from_secs(1), task)
        .await
        .expect("retention task should stop")
        .expect("retention task should exit cleanly");
}
