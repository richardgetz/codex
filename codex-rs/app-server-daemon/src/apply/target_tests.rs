#![cfg(unix)]

use std::path::Path;
use std::path::PathBuf;

use crate::Daemon;
use crate::apply_receipt::ApplyAttemptReceipt;
use crate::apply_receipt::ApplyFailureKind;
use crate::apply_receipt::ApplyPhase;
use crate::apply_receipt::ApplyStatus;
use crate::apply_receipt::HandoffReceipt;
use crate::client;
use crate::settings::DaemonSettings;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

fn test_daemon(home: &Path) -> Daemon {
    Daemon {
        socket_path: home.join("app-server-control.sock"),
        pid_file: home.join("daemon.pid"),
        update_pid_file: home.join("daemon-updater.pid"),
        operation_lock_file: home.join("daemon.lock"),
        settings_file: home.join("settings.json"),
        apply_receipt_file: home.join("apply-receipt.json"),
        managed_codex_bin: home.join("standalone-codex"),
    }
}

fn short_socket_directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("cdh-")
        .tempdir_in("/tmp")
        .expect("short socket directory")
}

async fn start_initialize_server(
    socket_path: &Path,
    server_codex_home: PathBuf,
) -> JoinHandle<Option<String>> {
    std::fs::create_dir_all(socket_path.parent().expect("socket parent"))
        .expect("socket directory");
    let mut listener = codex_uds::UnixListener::bind(socket_path)
        .await
        .expect("control listener");
    tokio::spawn(async move {
        let connection = listener.accept().await.expect("control connection");
        let mut websocket = accept_async(connection).await.expect("websocket handshake");
        let initialize = websocket
            .next()
            .await
            .expect("initialize request")
            .expect("websocket frame");
        let Message::Text(initialize) = initialize else {
            panic!("expected initialize JSON-RPC request");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&initialize)
                .expect("initialize JSON")
                .get("method")
                .and_then(serde_json::Value::as_str),
            Some("initialize")
        );
        websocket
            .send(Message::Text(
                serde_json::json!({
                    "id": 1,
                    "result": {
                        "userAgent": "codex_app_server/0.157.1",
                        "codexHome": server_codex_home,
                        "platformFamily": "unix",
                        "platformOs": std::env::consts::OS,
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("initialize response");

        let mut requested_method = None;
        while let Some(Ok(frame)) = websocket.next().await {
            let Message::Text(text) = frame else {
                continue;
            };
            let message: serde_json::Value = serde_json::from_str(&text).expect("JSON-RPC message");
            if let Some(method) = message.get("method").and_then(serde_json::Value::as_str)
                && message.get("id").is_some()
            {
                requested_method = Some(method.to_string());
                websocket
                    .send(Message::Text(
                        serde_json::json!({"id": message["id"], "result": {}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .expect("method response");
                break;
            }
        }
        requested_method
    })
}

#[tokio::test]
async fn target_settings_are_unchanged_when_probe_reports_a_foreign_codex_home() {
    let directory = tempfile::tempdir().expect("daemon home");
    let foreign_home = tempfile::tempdir().expect("foreign server home");
    let socket_directory = short_socket_directory();
    let daemon = test_daemon(directory.path());
    let daemon = Daemon {
        socket_path: socket_directory.path().join("s"),
        ..daemon
    };
    let old_target = directory.path().join("old-codex");
    let selected_target = directory.path().join("new-codex");
    let original_settings = DaemonSettings {
        managed_codex_path: Some(old_target),
        ..DaemonSettings::default()
    };
    original_settings
        .save(&daemon.settings_file)
        .await
        .expect("save original settings");
    let mut settings = DaemonSettings::load(&daemon.settings_file)
        .await
        .expect("load settings");
    let server =
        start_initialize_server(&daemon.socket_path, foreign_home.path().to_path_buf()).await;

    let error = daemon
        .persist_target_after_server_home_check(&mut settings, &selected_target)
        .await
        .expect_err("foreign server home must block target persistence");
    assert!(error.to_string().contains("does not match daemon home"));
    assert_eq!(server.await.expect("server task"), None);
    assert_eq!(settings, original_settings);
    assert_eq!(
        DaemonSettings::load(&daemon.settings_file)
            .await
            .expect("reload settings"),
        original_settings
    );
}

#[tokio::test]
async fn coordinator_request_is_not_sent_to_a_foreign_codex_home() {
    let expected_home = tempfile::tempdir().expect("selected home");
    let foreign_home = tempfile::tempdir().expect("foreign server home");
    let socket_directory = short_socket_directory();
    let socket_path = socket_directory.path().join("server.sock");
    let server = start_initialize_server(&socket_path, foreign_home.path().to_path_buf()).await;
    let daemon = test_daemon(expected_home.path());
    let daemon = Daemon {
        socket_path,
        ..daemon
    };

    let error = match daemon
        .request_handoff("thread/handoff/prepare", "test-handoff", None)
        .await
    {
        Ok(_) => panic!("foreign server home must block handoff prepare"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("refusing handoff mutation"));
    assert_eq!(
        error.server_codex_home,
        Some(std::fs::canonicalize(foreign_home.path()).expect("canonical home"))
    );
    assert_eq!(server.await.expect("server task"), None);
}

#[tokio::test]
async fn recovery_launcher_override_keeps_settings_when_receipt_origin_is_foreign() {
    let directory = tempfile::tempdir().expect("daemon home");
    let foreign_home = tempfile::tempdir().expect("foreign receipt home");
    let old_target = directory.path().join("old-codex");
    let selected_target = directory.path().join("new-codex");
    tokio::fs::write(&old_target, "old launcher")
        .await
        .expect("old launcher");
    tokio::fs::write(&selected_target, "selected launcher")
        .await
        .expect("selected launcher");
    let original_settings = DaemonSettings {
        managed_codex_path: Some(old_target),
        ..DaemonSettings::default()
    };
    let daemon = test_daemon(directory.path());
    original_settings
        .save(&daemon.settings_file)
        .await
        .expect("save original settings");
    let mut attempt = ApplyAttemptReceipt::new(
        HandoffReceipt {
            handoff_id: "foreign-origin-handoff".to_string(),
            state: "suspended".to_string(),
            runtime_version: "test".to_string(),
            created_at: 1,
            quarantined: false,
            transfer_started: Some(true),
            nodes: Vec::new(),
        },
        directory.path().join("old-codex"),
        None,
    );
    attempt.phase = ApplyPhase::Recovering;
    attempt.origin_codex_home = Some(foreign_home.path().to_path_buf());
    attempt
        .save(&daemon.apply_receipt_file)
        .await
        .expect("save unresolved receipt");

    let output = daemon
        .recover_with_resolution_locked(None, Some(&selected_target))
        .await
        .expect("foreign origin should be reported as a blocked recovery");

    assert_eq!(output.status, ApplyStatus::NeedsAttention);
    assert_eq!(
        output.failure_kind,
        Some(ApplyFailureKind::HandoffStorageMismatch)
    );
    assert!(output.error.as_deref().is_some_and(|error| {
        error.contains("originated in Codex home") && error.contains("refusing recovery")
    }));
    assert_eq!(
        DaemonSettings::load(&daemon.settings_file)
            .await
            .expect("reload settings"),
        original_settings
    );
}

#[tokio::test]
async fn recovery_preserves_storage_mismatch_when_socket_switches_homes() {
    let expected_home = tempfile::tempdir().expect("selected home");
    let foreign_home = tempfile::tempdir().expect("foreign server home");
    let socket_directory = short_socket_directory();
    let daemon = Daemon {
        socket_path: socket_directory.path().join("s"),
        ..test_daemon(expected_home.path())
    };
    let server =
        start_initialize_server(&daemon.socket_path, foreign_home.path().to_path_buf()).await;
    let mut attempt = ApplyAttemptReceipt::new(
        HandoffReceipt {
            handoff_id: "same-connection-home-check".to_string(),
            state: "suspended".to_string(),
            runtime_version: "test".to_string(),
            created_at: 1,
            quarantined: false,
            transfer_started: Some(true),
            nodes: Vec::new(),
        },
        expected_home.path().join("codex"),
        None,
    );
    attempt.phase = ApplyPhase::Recovering;
    attempt.origin_codex_home = Some(expected_home.path().to_path_buf());
    let managed_codex_bin = expected_home.path().join("codex");
    let info = client::ProbeInfo {
        app_server_version: "0.157.1".to_string(),
        codex_home: expected_home.path().to_path_buf(),
    };

    let output = daemon
        .recover_attempt(attempt, &managed_codex_bin, info, None)
        .await
        .expect("home switch should be reported as blocked recovery");

    assert_eq!(output.status, ApplyStatus::NeedsAttention);
    assert_eq!(
        output.failure_kind,
        Some(ApplyFailureKind::HandoffStorageMismatch)
    );
    assert!(
        output
            .error
            .as_deref()
            .is_some_and(|error| { error.contains("does not match selected Codex home") })
    );
    assert_eq!(server.await.expect("server task"), None);
}
