use std::path::Path;

use pretty_assertions::assert_eq;
use serde_json::Value;

use crate::apply_archive::ApplyArchiveOutcome;
use crate::apply_archive::archive_and_remove;
use crate::apply_archive::archive_and_resolve;
use crate::apply_archive::is_proven_idle_orphan;
use crate::apply_archive::read_archive;
use crate::apply_receipt::ApplyAttemptReceipt;
use crate::apply_receipt::ApplyPhase;
use crate::apply_receipt::ApplyStatus;
use crate::apply_receipt::HandoffReceipt;
use crate::apply_receipt::HandoffResolution;
use crate::apply_receipt::HandoffResolutionOutcome;

fn idle_graph(rollout: &Path) -> Vec<Value> {
    let mut nodes = Vec::new();
    for root_index in 0..3 {
        let root_id = format!("root-{root_index}");
        nodes.push(serde_json::json!({
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
            let needs_attention = child_index >= 1;
            nodes.push(serde_json::json!({
                "threadId": child_id,
                "rootThreadId": root_id,
                "parentThreadId": root_id,
                "turnId": null,
                "rolloutPath": rollout.display().to_string(),
                "wasRunning": false,
                "wasPaused": false,
                "state": if needs_attention { "needsAttention" } else { "notActive" },
                "blockers": if needs_attention { vec!["parentUnavailable"] } else { Vec::<&str>::new() },
            }));
        }
    }
    nodes
}

fn idle_attempt(nodes: Vec<Value>) -> ApplyAttemptReceipt {
    ApplyAttemptReceipt {
        handoff: HandoffReceipt {
            handoff_id: "handoff-idle-24".to_string(),
            state: "needsAttention".to_string(),
            runtime_version: "0.157.1-rick.2".to_string(),
            created_at: 1_790_690_000,
            quarantined: false,
            transfer_started: Some(true),
            nodes,
        },
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/codex/old".into(),
        managed_codex_version: Some("0.157.1-rick.2".to_string()),
        origin_codex_home: Some("/codex-home".into()),
        handoff_resolution: None,
        handoff_resolutions: Vec::new(),
        failure_kind: None,
        stop_started: Some(true),
        stop_completed: Some(true),
        failure: Some("unknown handoff".to_string()),
    }
}

#[tokio::test]
async fn known_idle_24_node_orphan_is_archived_and_removed_idempotently() {
    let directory = tempfile::tempdir().expect("temp dir");
    let rollout = directory.path().join("rollout.jsonl");
    tokio::fs::write(&rollout, "preserved rollout")
        .await
        .expect("rollout");
    let active = directory.path().join("apply-receipt.json");
    let history = directory.path().join("apply-history");
    let attempt = idle_attempt(idle_graph(&rollout));
    assert!(
        is_proven_idle_orphan(&attempt)
            .await
            .expect("classify orphan")
    );
    let mut recovering = attempt.clone();
    recovering.phase = ApplyPhase::Recovering;
    assert!(
        is_proven_idle_orphan(&recovering)
            .await
            .expect("classify orphan after recovery started")
    );
    attempt.save(&active).await.expect("save active receipt");

    archive_and_remove(
        &history,
        &active,
        &attempt,
        ApplyArchiveOutcome::RetiredIdleOrphan,
    )
    .await
    .expect("archive idle orphan");
    archive_and_remove(
        &history,
        &active,
        &attempt,
        ApplyArchiveOutcome::RetiredIdleOrphan,
    )
    .await
    .expect("retry archive idempotently");

    assert!(!active.exists());
    let archived = read_archive(&history.join("handoff-idle-24.json"))
        .await
        .expect("read archive")
        .expect("archive exists");
    assert_eq!(archived.outcome, ApplyArchiveOutcome::RetiredIdleOrphan);
    assert_eq!(archived.receipt, attempt);
    assert_eq!(archived.receipt.handoff.nodes.len(), 24);
    assert!(rollout.exists());
}

#[tokio::test]
async fn idle_orphan_retirement_fails_closed_for_active_or_ambiguous_nodes() {
    let directory = tempfile::tempdir().expect("temp dir");
    let rollout = directory.path().join("rollout.jsonl");
    tokio::fs::write(&rollout, "preserved rollout")
        .await
        .expect("rollout");
    let nodes = idle_graph(&rollout);

    let mut active = nodes.clone();
    active[1]["turnId"] = Value::String("turn-1".to_string());
    assert!(
        !is_proven_idle_orphan(&idle_attempt(active))
            .await
            .expect("classify active")
    );

    let mut paused = nodes.clone();
    paused[1]["wasPaused"] = Value::Bool(true);
    assert!(
        !is_proven_idle_orphan(&idle_attempt(paused))
            .await
            .expect("classify paused")
    );

    let mut blocker = nodes.clone();
    blocker[1]["blockers"] = serde_json::json!(["persistence"]);
    assert!(
        !is_proven_idle_orphan(&idle_attempt(blocker))
            .await
            .expect("classify other blocker")
    );

    let mut missing_field = nodes.clone();
    missing_field[1]
        .as_object_mut()
        .expect("node object")
        .remove("wasRunning");
    assert!(
        !is_proven_idle_orphan(&idle_attempt(missing_field))
            .await
            .expect("classify missing field")
    );

    let mut broken_graph = nodes.clone();
    broken_graph[1]["parentThreadId"] = Value::String("missing-parent".to_string());
    assert!(
        !is_proven_idle_orphan(&idle_attempt(broken_graph))
            .await
            .expect("classify broken graph")
    );

    let mut missing_rollout = nodes;
    missing_rollout[1]["rolloutPath"] =
        Value::String(directory.path().join("missing.jsonl").display().to_string());
    assert!(
        !is_proven_idle_orphan(&idle_attempt(missing_rollout))
            .await
            .expect("classify missing rollout")
    );

    let mut incomplete = idle_attempt(idle_graph(&rollout));
    incomplete.stop_completed = Some(false);
    assert!(
        !is_proven_idle_orphan(&incomplete)
            .await
            .expect("classify incomplete stop")
    );

    let mut no_transfer = idle_attempt(idle_graph(&rollout));
    no_transfer.handoff.transfer_started = None;
    assert!(
        !is_proven_idle_orphan(&no_transfer)
            .await
            .expect("classify missing transfer marker")
    );

    let mut stop_not_started = idle_attempt(idle_graph(&rollout));
    stop_not_started.stop_started = Some(false);
    assert!(
        !is_proven_idle_orphan(&stop_not_started)
            .await
            .expect("classify missing stop proof")
    );

    let mut duplicate = idle_graph(&rollout);
    duplicate[2]["threadId"] = duplicate[1]["threadId"].clone();
    assert!(
        !is_proven_idle_orphan(&idle_attempt(duplicate))
            .await
            .expect("classify duplicate node identity")
    );

    let mut missing_blockers = idle_graph(&rollout);
    missing_blockers[2]
        .as_object_mut()
        .expect("node object")
        .remove("blockers");
    assert!(
        !is_proven_idle_orphan(&idle_attempt(missing_blockers))
            .await
            .expect("classify missing blockers")
    );

    let mut root_blocker = idle_graph(&rollout);
    root_blocker[0]["state"] = Value::String("needsAttention".to_string());
    root_blocker[0]["blockers"] = serde_json::json!(["parentUnavailable"]);
    assert!(
        !is_proven_idle_orphan(&idle_attempt(root_blocker))
            .await
            .expect("classify blocked root")
    );
}

#[tokio::test]
async fn archive_conflict_preserves_current_receipt() {
    let directory = tempfile::tempdir().expect("temp dir");
    let rollout = directory.path().join("rollout.jsonl");
    tokio::fs::write(&rollout, "preserved rollout")
        .await
        .expect("rollout");
    let active = directory.path().join("apply-receipt.json");
    let history = directory.path().join("apply-history");
    let attempt = idle_attempt(idle_graph(&rollout));
    attempt.save(&active).await.expect("save active receipt");
    archive_and_remove(
        &history,
        &active,
        &attempt,
        ApplyArchiveOutcome::RetiredIdleOrphan,
    )
    .await
    .expect("create initial archive");

    let mut conflicting = attempt.clone();
    conflicting.handoff.runtime_version = "0.157.1-rick.3".to_string();
    tokio::fs::write(
        &active,
        serde_json::to_vec(&conflicting).expect("serialize conflict"),
    )
    .await
    .expect("write conflicting current receipt");
    assert!(
        archive_and_remove(
            &history,
            &active,
            &conflicting,
            ApplyArchiveOutcome::RetiredIdleOrphan,
        )
        .await
        .is_err()
    );
    assert!(active.exists());
}

#[tokio::test]
async fn resolved_handoff_archive_keeps_an_idempotent_active_resolution_marker() {
    let directory = tempfile::tempdir().expect("temp dir");
    let rollout = directory.path().join("rollout.jsonl");
    tokio::fs::write(&rollout, "preserved rollout")
        .await
        .expect("rollout");
    let active = directory.path().join("apply-receipt.json");
    let history = directory.path().join("apply-history");
    let attempt = idle_attempt(idle_graph(&rollout));
    attempt.save(&active).await.expect("save active receipt");
    let resolution = HandoffResolution {
        handoff_id: attempt.handoff.handoff_id.clone(),
        outcome: HandoffResolutionOutcome::RetiredIdleOrphan,
    };

    let resolved = archive_and_resolve(
        &history,
        &active,
        &attempt,
        ApplyArchiveOutcome::RetiredIdleOrphan,
        resolution,
        Path::new("/codex/new"),
        Some("0.157.1-rick.3".to_string()),
    )
    .await
    .expect("archive and mark resolution");
    let retried = archive_and_resolve(
        &history,
        &active,
        &attempt,
        ApplyArchiveOutcome::RetiredIdleOrphan,
        resolved
            .handoff_resolution
            .clone()
            .expect("resolution marker"),
        Path::new("/codex/new"),
        Some("0.157.1-rick.3".to_string()),
    )
    .await
    .expect("repeat archive and mark resolution");

    assert_eq!(retried.phase, ApplyPhase::Applied);
    assert_eq!(retried.handoff.state, "needsAttention");
    assert_eq!(retried.managed_codex_path, Path::new("/codex/new"));
    assert_eq!(
        retried.managed_codex_version.as_deref(),
        Some("0.157.1-rick.3")
    );
    assert_eq!(retried.handoff_resolution, resolved.handoff_resolution);
    assert_eq!(
        retried.output(Path::new("socket"), None, None).status,
        ApplyStatus::Applied
    );
    let archived = read_archive(&history.join("handoff-idle-24.json"))
        .await
        .expect("read archive")
        .expect("archive exists");
    assert_eq!(archived.outcome, ApplyArchiveOutcome::RetiredIdleOrphan);
    assert_eq!(archived.receipt, attempt);
    assert_eq!(archived.receipt.handoff.nodes.len(), 24);
}

#[tokio::test]
async fn resolved_handoff_keeps_prior_reconciliation_and_appends_current_event() {
    let directory = tempfile::tempdir().expect("temp dir");
    let active = directory.path().join("apply-receipt.json");
    let history = directory.path().join("apply-history");
    let mut attempt = idle_attempt(Vec::new());
    attempt.handoff.handoff_id = "handoff-current".to_string();
    attempt.handoff.state = "suspended".to_string();
    attempt.phase = ApplyPhase::Recovering;
    let prior = HandoffResolution {
        handoff_id: "handoff-orphan".to_string(),
        outcome: HandoffResolutionOutcome::RetiredIdleOrphan,
    };
    attempt.handoff_resolution = Some(prior.clone());
    attempt.handoff_resolutions = vec![prior.clone()];
    attempt.save(&active).await.expect("save current attempt");
    let current = HandoffResolution {
        handoff_id: attempt.handoff.handoff_id.clone(),
        outcome: HandoffResolutionOutcome::Recovered,
    };

    let resolved = archive_and_resolve(
        &history,
        &active,
        &attempt,
        ApplyArchiveOutcome::Recovered,
        current.clone(),
        Path::new("/codex/new"),
        Some("0.157.1-rick.3".to_string()),
    )
    .await
    .expect("archive current handoff");
    let retried = archive_and_resolve(
        &history,
        &active,
        &attempt,
        ApplyArchiveOutcome::Recovered,
        current.clone(),
        Path::new("/codex/new"),
        Some("0.157.1-rick.3".to_string()),
    )
    .await
    .expect("retry completion idempotently");
    let output = retried.output(Path::new("socket"), None, None);

    assert_eq!(resolved.handoff_resolution, Some(current.clone()));
    assert_eq!(
        resolved.handoff_resolutions,
        vec![prior.clone(), current.clone()]
    );
    assert_eq!(retried, resolved);
    let wire = serde_json::to_value(output).expect("serialize apply output");
    assert_eq!(wire["handoffResolution"]["handoffId"], "handoff-current");
    assert_eq!(
        wire["handoffResolutions"],
        serde_json::json!([
            {"handoffId": "handoff-orphan", "outcome": "retiredIdleOrphan"},
            {"handoffId": "handoff-current", "outcome": "recovered"}
        ])
    );
}
