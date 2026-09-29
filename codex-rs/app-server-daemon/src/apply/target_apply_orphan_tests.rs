use std::path::Path;
use std::path::PathBuf;

use crate::ApplyOptions;
use crate::Daemon;
use crate::apply_receipt::ApplyAttemptReceipt;
use crate::apply_receipt::ApplyFailureKind;
use crate::apply_receipt::ApplyOutput;
use crate::apply_receipt::ApplyPhase;
use crate::apply_receipt::ApplyStatus;
use crate::apply_receipt::HandoffReceipt;
use crate::apply_receipt::HandoffResolutionOutcome;
use crate::backend::BackendPaths;
use crate::backend::LaunchIdentity;
use crate::backend::pid_backend;
use crate::client;
use crate::settings::DaemonSettings;
use anyhow::Context;
use anyhow::Result;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio::time::sleep;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const INCIDENT_HANDOFF_ID: &str = "incident-handoff-24";
const FRESH_HANDOFF_ID: &str = "fresh-target-handoff";

fn incident_nodes(rollout: &Path) -> Vec<Value> {
    let mut nodes = Vec::with_capacity(24);
    for root_index in 0..3 {
        let root_id = format!("root-{root_index}");
        nodes.push(json!({
            "threadId": root_id,
            "rootThreadId": root_id,
            "parentThreadId": null,
            "turnId": null,
            "rolloutPath": rollout.display().to_string(),
            "wasRunning": false,
            "wasPaused": false,
            "state": "notActive",
            "blockers": [],
        }));
        for child_index in 0..7 {
            let child_id = format!("child-{root_index}-{child_index}");
            nodes.push(json!({
                "threadId": child_id,
                "rootThreadId": root_id,
                "parentThreadId": root_id,
                "turnId": null,
                "rolloutPath": rollout.display().to_string(),
                "wasRunning": false,
                "wasPaused": false,
                "state": "needsAttention",
                "blockers": ["parentUnavailable"],
            }));
        }
    }
    nodes
}

fn handoff_receipt(id: &str, state: &str) -> Value {
    json!({
        "handoffId": id,
        "state": state,
        "runtimeVersion": "0.157.1-rick.2",
        "createdAt": 1_790_690_000_i64,
        "quarantined": false,
        "transferStarted": true,
        "nodes": [],
    })
}

fn write_fake_launcher(path: &Path, version: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let script = format!(
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then printf 'codex %s\n' '{version}'; exit 0; fi
if [ "$1" = "app-server" ] && [ "$2" = "--managed-daemon" ] && [ "$3" = "--help" ]; then exit 1; fi
exec /bin/sleep 600
"#,
    );
    std::fs::write(path, script)
        .with_context(|| format!("write fake launcher {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("make fake launcher executable {}", path.display()))
}

fn managed_backend(daemon: &Daemon, launcher: &Path) -> crate::backend::PidBackend {
    pid_backend(BackendPaths {
        codex_bin: launcher.to_path_buf(),
        pid_file: daemon.pid_file.clone(),
        update_pid_file: daemon.update_pid_file.clone(),
        remote_control_enabled: false,
        reload_enabled: true,
        feature_overrides: Default::default(),
    })
}

async fn serve_connection(
    connection: codex_uds::UnixStream,
    codex_home: &Path,
    calls: &Path,
) -> Result<()> {
    let mut websocket = accept_async(connection).await?;
    let Some(Ok(Message::Text(initialize))) = websocket.next().await else {
        return Ok(());
    };
    let initialize: Value = serde_json::from_str(&initialize)?;
    anyhow::ensure!(
        initialize.get("method").and_then(Value::as_str) == Some("initialize"),
        "expected initialize request"
    );
    websocket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": initialize.get("id").cloned().unwrap_or(json!(1)),
                "result": {
                    "userAgent": "codex_app_server/0.157.1",
                    "codexHome": codex_home,
                    "platformFamily": "unix",
                    "platformOs": std::env::consts::OS,
                }
            })
            .to_string()
            .into(),
        ))
        .await?;

    while let Some(frame) = websocket.next().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(tokio_tungstenite::tungstenite::Error::ConnectionClosed)
            | Err(tokio_tungstenite::tungstenite::Error::AlreadyClosed) => return Ok(()),
            Err(tokio_tungstenite::tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::UnexpectedEof
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let Message::Text(text) = frame else {
            continue;
        };
        let request: Value = serde_json::from_str(&text)?;
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            continue;
        };
        if method == "initialized" {
            continue;
        }
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let handoff_id = request
            .get("params")
            .and_then(|params| params.get("handoffId"))
            .and_then(Value::as_str);
        let call = format!("{method}:{}\n", handoff_id.unwrap_or(""));
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(calls)
            .await?
            .write_all(call.as_bytes())
            .await?;
        let response = match (method, handoff_id) {
            ("thread/handoff/status", Some(INCIDENT_HANDOFF_ID)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": format!("unknown handoff id {INCIDENT_HANDOFF_ID}"),
                }
            }),
            ("thread/handoff/prepare", _) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": handoff_receipt(FRESH_HANDOFF_ID, "suspended"),
            }),
            ("thread/handoff/status", Some(FRESH_HANDOFF_ID)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": handoff_receipt(FRESH_HANDOFF_ID, "suspended"),
            }),
            ("thread/handoff/recover", Some(FRESH_HANDOFF_ID)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": handoff_receipt(FRESH_HANDOFF_ID, "completed"),
            }),
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("unexpected test RPC {method}"),
                }
            }),
        };
        websocket
            .send(Message::Text(response.to_string().into()))
            .await?;
        return Ok(());
    }
    Ok(())
}

async fn start_control_server(
    codex_home: PathBuf,
    calls: PathBuf,
) -> Result<JoinHandle<Result<()>>> {
    let socket_path = codex_home.join("app-server-control.sock");
    let mut listener = codex_uds::UnixListener::bind(&socket_path)
        .await
        .context("bind test app-server control socket")?;
    Ok(tokio::spawn(async move {
        loop {
            let connection = listener.accept().await?;
            serve_connection(connection, &codex_home, &calls).await?;
        }
        #[allow(unreachable_code)]
        Ok(())
    }))
}

async fn wait_for_server(socket_path: &Path) -> Result<client::ProbeInfo> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(info) = client::probe(socket_path).await {
                return Ok(info);
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("test app server did not become ready")?
}

struct ApplyEvidence {
    old_pid: u32,
    old_identity: LaunchIdentity,
    first: ApplyOutput,
    first_calls: String,
    archive: Value,
    current_receipt: ApplyAttemptReceipt,
    settings_after: DaemonSettings,
    second: ApplyOutput,
    second_calls: String,
    archive_bytes: Vec<u8>,
    archived_again: Vec<u8>,
    running_identity: LaunchIdentity,
}

async fn run_target_apply(
    daemon: &Daemon,
    selected_launcher: &Path,
    old_version: &str,
    rpc_log: &Path,
    start_result: Result<Option<u32>>,
) -> Result<ApplyEvidence> {
    let old_pid = start_result
        .context("start test-owned old daemon")?
        .context("old daemon was not started")?;
    let probe = wait_for_server(&daemon.socket_path).await?;
    let expected_home = daemon
        .settings_file
        .parent()
        .context("test daemon settings path has no home")?;
    anyhow::ensure!(probe.codex_home == expected_home);
    let old_identity = crate::backend::running_launch_identity(&daemon.pid_file)
        .await
        .context("read old process launch identity")?
        .context("old launch identity")?;
    let first = daemon
        .apply(ApplyOptions {
            managed_codex_path: Some(selected_launcher.to_path_buf()),
        })
        .await
        .context("apply selected launcher")?;
    let first_calls = tokio::fs::read_to_string(rpc_log).await.with_context(|| {
        format!(
            "read RPC log after selected target apply (status={:?}, failure={:?}, kind={:?})",
            first.status, first.error, first.failure_kind
        )
    })?;
    let archive_path = daemon
        .apply_history_dir()?
        .join(format!("{INCIDENT_HANDOFF_ID}.json"));
    let archive_bytes = tokio::fs::read(&archive_path)
        .await
        .context("read idle orphan archive")?;
    let archive: Value = serde_json::from_slice(&archive_bytes)?;
    let current_receipt = ApplyAttemptReceipt::load(&daemon.apply_receipt_file)
        .await
        .context("load resolved current apply receipt")?
        .context("resolved current apply receipt")?;
    let settings_after = DaemonSettings::load(&daemon.settings_file)
        .await
        .context("load settings after target apply")?;
    let second = daemon
        .apply(ApplyOptions {
            managed_codex_path: Some(selected_launcher.to_path_buf()),
        })
        .await
        .context("repeat selected launcher apply")?;
    let second_calls = tokio::fs::read_to_string(rpc_log)
        .await
        .context("read RPC log after repeated apply")?;
    let archived_again = tokio::fs::read(&archive_path)
        .await
        .context("read archived receipt after repeated apply")?;
    let running_identity = crate::backend::running_launch_identity(&daemon.pid_file)
        .await
        .context("read selected process launch identity")?
        .context("selected launch identity")?;
    anyhow::ensure!(old_identity.version.as_deref() == Some(old_version));
    Ok(ApplyEvidence {
        old_pid,
        old_identity,
        first,
        first_calls,
        archive,
        current_receipt,
        settings_after,
        second,
        second_calls,
        archive_bytes,
        archived_again,
        running_identity,
    })
}

#[tokio::test]
async fn public_target_apply_retires_incident_orphan_then_verifies_and_repeats_idempotently() {
    let directory = super::short_socket_directory();
    let home = directory.path();
    let daemon = super::test_daemon(home);
    let old_launcher = home.join("old-codex");
    let selected_launcher = home.join("selected-codex");
    let rollout = home.join("thread-rollout.jsonl");
    let rpc_log = home.join("handoff-rpc.log");
    let old_version = "0.157.1-rick.2";
    let selected_version = "0.157.1-rick.3";
    write_fake_launcher(&old_launcher, old_version).expect("old launcher");
    write_fake_launcher(&selected_launcher, selected_version).expect("selected launcher");
    tokio::fs::write(&rollout, "retained incident rollout\n")
        .await
        .expect("write rollout");
    let original_settings = DaemonSettings {
        remote_control_enabled: false,
        shutdown_grace_seconds: 1,
        managed_codex_path: Some(old_launcher.clone()),
        ..DaemonSettings::default()
    };
    original_settings
        .save(&daemon.settings_file)
        .await
        .expect("save settings");
    let origin = std::fs::canonicalize(home).expect("canonical test home");
    let mut incident = ApplyAttemptReceipt::new(
        HandoffReceipt {
            handoff_id: INCIDENT_HANDOFF_ID.to_string(),
            state: "needsAttention".to_string(),
            runtime_version: old_version.to_string(),
            created_at: 1_790_690_000,
            quarantined: false,
            transfer_started: Some(true),
            nodes: incident_nodes(&rollout),
        },
        old_launcher.clone(),
        Some(old_version.to_string()),
    );
    incident.phase = ApplyPhase::NeedsAttention;
    incident.origin_codex_home = Some(origin);
    incident.failure_kind = Some(ApplyFailureKind::HandoffJournalMissing);
    incident.stop_started = Some(true);
    incident.stop_completed = Some(true);
    incident.failure = Some("unknown handoff id incident-handoff-24".to_string());
    incident
        .save(&daemon.apply_receipt_file)
        .await
        .expect("save incident receipt");

    let mut server = start_control_server(home.to_path_buf(), rpc_log.clone())
        .await
        .expect("start app-server protocol fixture");
    let old_backend = managed_backend(&daemon, &old_launcher);
    let start_result = old_backend.start().await;
    let cleanup_backend = managed_backend(&daemon, &selected_launcher);
    let outcome = run_target_apply(
        &daemon,
        &selected_launcher,
        old_version,
        &rpc_log,
        start_result,
    )
    .await;
    let server_result = if server.is_finished() {
        format!("finished: {:?}", (&mut server).await)
    } else {
        "still accepting protocol connections".to_string()
    };
    let cleanup = cleanup_backend.stop().await;
    cleanup.expect("stop test-owned daemon process");
    server.abort();
    let evidence = outcome.unwrap_or_else(|error| {
        panic!("target apply orchestration: {error:#}; protocol fixture {server_result}")
    });
    assert!(
        server_result == "still accepting protocol connections",
        "protocol fixture exited during public apply: {server_result}"
    );

    assert!(evidence.old_pid > 0);
    assert_eq!(evidence.old_identity.path, old_launcher);
    assert_eq!(evidence.old_identity.version.as_deref(), Some(old_version));
    assert_eq!(evidence.first.status, ApplyStatus::Applied);
    assert_eq!(
        evidence.first.managed_codex_path.as_deref(),
        Some(selected_launcher.as_path())
    );
    assert_eq!(
        evidence.first.managed_codex_version.as_deref(),
        Some(selected_version)
    );
    assert_eq!(
        evidence.first.running_managed_codex_version.as_deref(),
        Some(selected_version)
    );
    assert_eq!(
        evidence.first.app_server_version.as_deref(),
        Some("0.157.1")
    );
    assert_eq!(evidence.first.handoff_id.as_deref(), Some(FRESH_HANDOFF_ID));
    assert_eq!(
        evidence
            .first
            .handoff_resolutions
            .iter()
            .map(|resolution| (&*resolution.handoff_id, resolution.outcome))
            .collect::<Vec<_>>(),
        vec![
            (
                INCIDENT_HANDOFF_ID,
                HandoffResolutionOutcome::RetiredIdleOrphan
            ),
            (FRESH_HANDOFF_ID, HandoffResolutionOutcome::Recovered),
        ]
    );
    assert_eq!(evidence.archive["outcome"], "retiredIdleOrphan");
    assert_eq!(
        evidence.archive["receipt"]["handoff"]["nodes"]
            .as_array()
            .map(Vec::len),
        Some(24)
    );
    assert_eq!(
        tokio::fs::read_to_string(&rollout)
            .await
            .expect("retained rollout"),
        "retained incident rollout\n"
    );
    assert_eq!(evidence.current_receipt.phase, ApplyPhase::Applied);
    assert_eq!(
        evidence.current_receipt.handoff.handoff_id,
        FRESH_HANDOFF_ID
    );
    assert_eq!(
        evidence.current_receipt.handoff_resolutions,
        evidence.first.handoff_resolutions
    );
    assert_eq!(
        evidence.settings_after.managed_codex_path.as_deref(),
        Some(selected_launcher.as_path())
    );
    assert_eq!(evidence.second.status, ApplyStatus::Applied);
    assert_eq!(
        evidence.second.handoff_resolutions,
        evidence.first.handoff_resolutions
    );
    assert_eq!(
        evidence.second_calls, evidence.first_calls,
        "repeat apply must make no handoff RPCs"
    );
    assert_eq!(
        evidence.first_calls.lines().collect::<Vec<_>>(),
        vec![
            "thread/handoff/status:incident-handoff-24",
            "thread/handoff/prepare:",
            "thread/handoff/status:fresh-target-handoff",
            "thread/handoff/status:fresh-target-handoff",
            "thread/handoff/recover:fresh-target-handoff",
        ]
    );
    assert_eq!(evidence.archived_again, evidence.archive_bytes);
    assert_eq!(evidence.running_identity.path, selected_launcher);
    assert_eq!(
        evidence.running_identity.version.as_deref(),
        Some(selected_version)
    );
}
