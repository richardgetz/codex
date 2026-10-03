use std::future::pending;
use std::path::PathBuf;

use crate::Daemon;
use crate::apply_receipt::ApplyStatus;
use crate::settings::DaemonSettings;
use anyhow::anyhow;
use pretty_assertions::assert_eq;
use tokio::time::Duration;
use tokio::time::timeout;

use super::ensure_apply_launcher;
use super::stop_backend_with_receipt;
use crate::apply_receipt::ApplyAttemptReceipt;
use crate::apply_receipt::ApplyFailureKind;
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

fn test_daemon(home: &std::path::Path) -> Daemon {
    Daemon {
        log_diagnostics: false,
        socket_path: home.join("app-server-control.sock"),
        pid_file: home.join("daemon.pid"),
        update_pid_file: home.join("daemon-updater.pid"),
        operation_lock_file: home.join("daemon.lock"),
        settings_file: home.join("settings.json"),
        apply_receipt_file: home.join("apply-receipt.json"),
        managed_codex_bin: home.join("standalone-codex"),
    }
}

#[test]
fn launcher_update_requires_exact_fork_launcher_version_identity() {
    let managed_codex_bin = PathBuf::from("/codex/selected");
    assert_eq!(
        super::launcher_update_required(
            &managed_codex_bin,
            /*managed_backend_is_running*/ true,
            Some("0.157.1-rick.2".to_string()),
            Some("0.157.1-rick.3".to_string()),
        )
        .expect("different fork patch builds require a refresh"),
        true,
    );
    assert_eq!(
        super::launcher_update_required(
            &managed_codex_bin,
            /*managed_backend_is_running*/ true,
            Some("0.157.1-rick.2".to_string()),
            Some("0.157.1-rick.2".to_string()),
        )
        .expect("matching versions need no apply"),
        false,
    );
    assert_eq!(
        super::launcher_update_required(
            &managed_codex_bin,
            /*managed_backend_is_running*/ false,
            /*running_managed_codex_version*/ None,
            /*managed_codex_version*/ None,
        )
        .expect("a stopped daemon needs no startup reconciliation"),
        false,
    );
    let error = super::launcher_update_required(
        &managed_codex_bin,
        /*managed_backend_is_running*/ true,
        /*running_managed_codex_version*/ None,
        Some("0.157.1-rick.2".to_string()),
    )
    .expect_err("an unverified running version must fail closed");
    assert!(error.to_string().contains("/codex/selected"));
}

#[tokio::test]
async fn target_apply_skips_when_no_daemon_is_configured() {
    let directory = tempfile::tempdir().expect("temp dir");
    let daemon = test_daemon(directory.path());
    let output = daemon
        .apply_to_target(directory.path().join("new-codex"))
        .await
        .expect("unconfigured daemon is a clean skip");

    assert_eq!(output.status, ApplyStatus::NotConfigured);
    assert_eq!(output.managed_codex_path, None);
    assert!(
        daemon
            .reconcile_launcher_update()
            .await
            .expect("default standalone selection needs no local reconciliation")
            .is_none()
    );
    assert!(!daemon.settings_file.exists());
    assert!(!daemon.pid_file.exists());
    let pinned = directory.path().join("pinned-codex");
    let requested = directory.path().join("requested-codex");
    tokio::fs::write(&pinned, "pinned")
        .await
        .expect("pinned launcher");
    tokio::fs::write(&requested, "requested")
        .await
        .expect("requested launcher");
    let mut attempt = test_attempt();
    attempt.managed_codex_path = pinned.clone();
    attempt.phase = ApplyPhase::Recovering;
    attempt
        .save(&daemon.apply_receipt_file)
        .await
        .expect("save unresolved receipt");

    let output = daemon
        .apply_to_target(requested.clone())
        .await
        .expect("unresolved receipt should be reported");

    assert_eq!(output.status, ApplyStatus::NeedsAttention);
    assert!(output.error.as_deref().is_some_and(|error| {
        error.contains(&pinned.display().to_string())
            && error.contains(&requested.display().to_string())
    }));
    assert!(!daemon.settings_file.exists());
    assert_eq!(
        ApplyAttemptReceipt::load(&daemon.apply_receipt_file)
            .await
            .expect("load receipt")
            .expect("receipt"),
        attempt
    );
}

#[tokio::test]
async fn handoff_home_validation_accepts_canonical_aliases_and_rejects_other_homes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let daemon = test_daemon(directory.path());
    let expected = directory.path().canonicalize().expect("canonical home");
    assert_eq!(
        daemon
            .validate_server_home(None, &expected)
            .await
            .expect("same daemon home"),
        expected
    );

    let foreign = tempfile::tempdir().expect("foreign home");
    let error = daemon
        .validate_server_home(None, foreign.path())
        .await
        .expect_err("foreign server home must be rejected");
    assert!(error.to_string().contains("does not match daemon home"));

    let mut attempt = test_attempt();
    attempt.origin_codex_home = Some(foreign.path().to_path_buf());
    let error = daemon
        .validate_server_home(Some(&attempt), &expected)
        .await
        .expect_err("foreign receipt origin must be rejected");
    assert!(error.to_string().contains("originated in Codex home"));
}

#[cfg(unix)]
#[tokio::test]
async fn handoff_home_validation_accepts_symlink_equivalent_home() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let alias = directory.path().join("codex-home-alias");
    std::os::unix::fs::symlink(directory.path(), &alias).expect("home alias");
    let daemon = test_daemon(directory.path());

    assert_eq!(
        daemon
            .validate_server_home(None, &alias)
            .await
            .expect("canonical home alias"),
        directory.path().canonicalize().expect("canonical home")
    );
}

#[tokio::test]
async fn target_apply_persists_selected_launcher_without_starting_stopped_daemon() {
    let directory = tempfile::tempdir().expect("temp dir");
    let daemon = test_daemon(directory.path());
    let previous = directory.path().join("previous-codex");
    let target = directory.path().join("updated-codex");
    tokio::fs::write(&previous, "previous")
        .await
        .expect("previous launcher");
    tokio::fs::write(&target, "updated")
        .await
        .expect("selected launcher");
    tokio::fs::write(
        &daemon.settings_file,
        serde_json::json!({
            "remoteControlEnabled": true,
            "shutdownGraceSeconds": 45,
            "updater": {"autoUpdateEnabled": false, "updateIntervalMinutes": 17},
            "managedCodexPath": previous,
            "futureSetting": "preserved"
        })
        .to_string(),
    )
    .await
    .expect("configured settings");

    let output = daemon
        .apply_to_target(target.clone())
        .await
        .expect("stopped daemon should defer target activation");

    assert_eq!(output.status, ApplyStatus::Deferred);
    assert_eq!(output.managed_codex_path, Some(target.clone()));
    let wire_output = serde_json::to_value(&output).expect("serialize deferred result");
    assert_eq!(wire_output["status"], "deferred");
    assert_eq!(wire_output["handoffId"], serde_json::Value::Null);
    assert_eq!(
        wire_output["managedCodexPath"],
        target.to_string_lossy().as_ref()
    );
    assert!(!daemon.pid_file.exists());
    assert!(!daemon.apply_receipt_file.exists());
    let settings = DaemonSettings::load(&daemon.settings_file)
        .await
        .expect("reloaded settings");
    assert_eq!(
        settings,
        DaemonSettings {
            remote_control_enabled: true,
            auto_update_enabled: false,
            update_interval_minutes: 17,
            shutdown_grace_seconds: 45,
            managed_codex_path: Some(target),
            ..DaemonSettings::default()
        }
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &tokio::fs::read(&daemon.settings_file)
                .await
                .expect("read settings")
        )
        .expect("parse settings")["futureSetting"],
        "preserved"
    );
}

#[tokio::test]
async fn target_apply_recognizes_default_launcher_configuration_when_stopped() {
    let directory = tempfile::tempdir().expect("temp dir");
    let daemon = test_daemon(directory.path());
    let target = directory.path().join("updated-codex");
    tokio::fs::write(&target, "updated")
        .await
        .expect("selected launcher");
    tokio::fs::write(
        &daemon.settings_file,
        serde_json::json!({
            "remoteControlEnabled": true,
            "shutdownGraceSeconds": 45,
            "updater": {"autoUpdateEnabled": false, "updateIntervalMinutes": 17}
        })
        .to_string(),
    )
    .await
    .expect("default-launcher daemon settings");

    let output = daemon
        .apply_to_target(target.clone())
        .await
        .expect("configured daemon should persist its selected target");

    assert_eq!(output.status, ApplyStatus::Deferred);
    assert_eq!(output.managed_codex_path, Some(target.clone()));
    assert!(!daemon.pid_file.exists());
    let settings = DaemonSettings::load(&daemon.settings_file)
        .await
        .expect("reloaded settings");
    assert_eq!(settings.managed_codex_path, Some(target));
    assert!(settings.remote_control_enabled);
    assert!(!settings.auto_update_enabled);
    assert_eq!(settings.update_interval_minutes, 17);
    assert_eq!(settings.shutdown_grace_seconds, 45);
}

#[tokio::test]
async fn target_apply_keeps_the_saved_launcher_until_unresolved_handoff_recovers() {
    let directory = tempfile::tempdir().expect("temp dir");
    let daemon = test_daemon(directory.path());
    let pinned = directory.path().join("pinned-codex");
    let requested = directory.path().join("requested-codex");
    tokio::fs::write(&pinned, "pinned")
        .await
        .expect("pinned launcher");
    tokio::fs::write(&requested, "requested")
        .await
        .expect("requested launcher");
    DaemonSettings {
        managed_codex_path: Some(pinned.clone()),
        ..DaemonSettings::default()
    }
    .save(&daemon.settings_file)
    .await
    .expect("save settings");
    let mut attempt = test_attempt();
    attempt.managed_codex_path = pinned.clone();
    attempt.handoff.state = "needsAttention".to_string();
    attempt.handoff.nodes = vec![serde_json::json!({
        "threadId": "thread-1",
        "rootThreadId": "thread-1",
        "state": "suspended",
        "wasRunning": true
    })];
    attempt.phase = ApplyPhase::NeedsAttention;
    attempt.stop_started = Some(true);
    attempt.stop_completed = Some(true);
    assert!(attempt.blocks_new_apply());
    attempt
        .save(&daemon.apply_receipt_file)
        .await
        .expect("save unresolved receipt");

    let output = daemon
        .apply_to_target(requested.clone())
        .await
        .expect("conflicting target should be a reported blocker");

    assert_eq!(output.status, ApplyStatus::NeedsAttention);
    assert!(output.error.as_deref().is_some_and(|error| {
        error.contains(&pinned.display().to_string())
            && error.contains(&requested.display().to_string())
    }));
    assert_eq!(
        DaemonSettings::load(&daemon.settings_file)
            .await
            .expect("reload settings")
            .managed_codex_path,
        Some(pinned)
    );
    let persisted = ApplyAttemptReceipt::load(&daemon.apply_receipt_file)
        .await
        .expect("load receipt")
        .expect("receipt");
    assert_eq!(persisted.managed_codex_path, attempt.managed_codex_path);
    assert_eq!(persisted.handoff, attempt.handoff);
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
async fn apply_status_rechecks_the_running_full_fork_version_for_resolved_receipts() {
    let directory = tempfile::tempdir().expect("temp dir");
    let daemon = test_daemon(directory.path());
    let mut attempt = test_attempt();
    attempt.phase = ApplyPhase::Applied;
    attempt.handoff.state = "completed".to_string();
    attempt.managed_codex_path = directory.path().join("updated-codex");
    attempt.managed_codex_version = Some("0.157.1-rick.3".to_string());
    attempt
        .save(&daemon.apply_receipt_file)
        .await
        .expect("save receipt");

    let output = daemon
        .apply_status()
        .await
        .expect("status should report an unverifiable running launcher");

    assert_eq!(output.status, ApplyStatus::NeedsAttention);
    assert_eq!(
        output.failure_kind,
        Some(ApplyFailureKind::RunningLauncherMismatch)
    );
    assert!(output.error.as_deref().is_some_and(|error| {
        error.contains("updated-codex") && error.contains("full fork versions must match")
    }));
    let persisted = ApplyAttemptReceipt::load(&daemon.apply_receipt_file)
        .await
        .expect("load persisted diagnostic")
        .expect("receipt");
    assert_eq!(
        persisted.failure_kind,
        Some(ApplyFailureKind::RunningLauncherMismatch)
    );
    assert_eq!(persisted.failure, output.error);
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
