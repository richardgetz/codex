use std::future::pending;
use std::path::PathBuf;

use crate::settings::DaemonSettings;
use anyhow::anyhow;
use tokio::time::Duration;
use tokio::time::timeout;

use super::ensure_apply_launcher;
use super::stop_backend_with_receipt;
use crate::apply_receipt::ApplyAttemptReceipt;
use crate::apply_receipt::ApplyPhase;
use crate::apply_receipt::HandoffReceipt;

#[test]
fn apply_requires_an_explicit_launcher() {
    let settings = DaemonSettings {
        remote_control_enabled: false,
        managed_codex_path: None,
        ..DaemonSettings::default()
    };

    let error = ensure_apply_launcher(&settings).expect_err("standalone apply must be rejected");
    assert!(error.to_string().contains("bootstrap --codex-bin PATH"));
}

fn test_attempt() -> ApplyAttemptReceipt {
    ApplyAttemptReceipt::new(
        HandoffReceipt {
            handoff_id: "handoff-1".to_string(),
            state: "suspended".to_string(),
            runtime_version: "test".to_string(),
            created_at: 1,
            quarantined: false,
            transfer_started: Some(true),
            nodes: Vec::new(),
        },
        PathBuf::from("/codex"),
        None,
    )
}

#[tokio::test]
async fn stop_failure_keeps_receipt_retryable() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("apply-receipt.json");
    let mut attempt = test_attempt();
    attempt.save(&path).await.expect("initial receipt");

    let error = stop_backend_with_receipt(&mut attempt, &path, async {
        Err(anyhow!("backend stop failed"))
    })
    .await
    .expect_err("stop should fail");
    assert_eq!(error.to_string(), "backend stop failed");

    let saved = ApplyAttemptReceipt::load(&path)
        .await
        .expect("load receipt")
        .expect("receipt");
    assert_eq!(saved.stop_started, Some(true));
    assert_eq!(saved.stop_completed, Some(false));
    assert_eq!(saved.phase, ApplyPhase::Prepared);
}

#[tokio::test]
async fn cancellation_after_stop_marker_keeps_receipt_retryable() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("apply-receipt.json");
    let mut attempt = test_attempt();
    attempt.save(&path).await.expect("initial receipt");

    timeout(
        Duration::from_millis(25),
        stop_backend_with_receipt(&mut attempt, &path, pending()),
    )
    .await
    .expect_err("pending stop should be cancelled");

    let saved = ApplyAttemptReceipt::load(&path)
        .await
        .expect("load receipt")
        .expect("receipt");
    assert_eq!(saved.stop_started, Some(true));
    assert_eq!(saved.stop_completed, Some(false));
    assert_eq!(saved.phase, ApplyPhase::Prepared);
}

#[tokio::test]
async fn successful_stop_marks_replacement_start_boundary() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("apply-receipt.json");
    let mut attempt = test_attempt();
    attempt.save(&path).await.expect("initial receipt");

    stop_backend_with_receipt(&mut attempt, &path, async { Ok(()) })
        .await
        .expect("stop");

    let saved = ApplyAttemptReceipt::load(&path)
        .await
        .expect("load receipt")
        .expect("receipt");
    assert_eq!(saved.stop_started, Some(true));
    assert_eq!(saved.stop_completed, Some(true));
    assert_eq!(saved.phase, ApplyPhase::Starting);
}
