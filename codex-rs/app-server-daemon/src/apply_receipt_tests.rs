use std::path::Path;

use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use pretty_assertions::assert_eq;
use serde_json::Value;

use super::ApplyAttemptReceipt;
use super::ApplyPhase;
use super::ApplyStatus;
use super::HandoffReceipt;
use super::ensure_transferable_handoff;
use super::parse_handoff_response;
use super::sanitize_failure;

fn receipt(state: &str, nodes: Vec<Value>) -> HandoffReceipt {
    HandoffReceipt {
        handoff_id: "handoff-1".to_string(),
        state: state.to_string(),
        runtime_version: "codex-0.2.0".to_string(),
        created_at: 1_757_712_000,
        quarantined: false,
        transfer_started: None,
        nodes,
    }
}

fn node(state: &str, turn_id: Option<&str>) -> Value {
    let mut node = serde_json::json!({
        "threadId": "thread-1",
        "rootThreadId": "thread-1",
        "state": state,
    });
    if let Some(turn_id) = turn_id {
        node["turnId"] = Value::String(turn_id.to_string());
    }
    node
}

fn failed_preparation_node() -> Value {
    serde_json::json!({
        "threadId": "thread-1",
        "rootThreadId": "thread-1",
        "state": "needsAttention",
        "turnId": null,
        "wasRunning": false,
    })
}

#[test]
fn transferability_requires_a_fully_suspended_graph() {
    let safe = receipt(
        "suspended",
        vec![node("suspended", Some("turn-1")), node("notActive", None)],
    );
    assert!(ensure_transferable_handoff(&safe).is_ok());

    let partial = receipt("draining", vec![node("suspended", Some("turn-1"))]);
    let error =
        ensure_transferable_handoff(&partial).expect_err("draining graph must not transfer");
    assert!(error.to_string().contains("not safely suspended"));

    let missing_turn = receipt("suspended", vec![node("suspended", None)]);
    let error =
        ensure_transferable_handoff(&missing_turn).expect_err("suspended node needs exact turn");
    assert!(error.to_string().contains("exact turn id"));
}

#[test]
fn parses_wrapped_success_and_structured_error_receipts() {
    let handoff = receipt("needsAttention", vec![node("blocked", None)]);
    let success = JSONRPCMessage::Response(JSONRPCResponse {
        id: RequestId::Integer(2),
        result: serde_json::json!({ "receipt": handoff }),
    });
    let parsed = parse_handoff_response(success, "thread/handoff/status").expect("receipt");
    assert_eq!(parsed, handoff);

    let error_receipt = receipt("needsAttention", vec![node("blocked", None)]);
    let error_message = JSONRPCMessage::Error(JSONRPCError {
        id: RequestId::Integer(2),
        error: JSONRPCErrorError {
            code: -32000,
            message: "handoff blocked".to_string(),
            data: Some(serde_json::json!({ "receipt": error_receipt })),
        },
    });
    let error = parse_handoff_response(error_message, "thread/handoff/prepare")
        .expect_err("structured error should be returned");
    assert_eq!(error.receipt, Some(error_receipt));
    assert_eq!(error.method, "thread/handoff/prepare");
}

#[tokio::test]
async fn apply_receipt_round_trips_atomically() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("apply-receipt.json");
    let expected = ApplyAttemptReceipt {
        handoff: receipt("suspended", vec![node("suspended", Some("turn-1"))]),
        phase: ApplyPhase::Recovering,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: Some("0.154.0-rick.2".to_string()),
        stop_started: Some(true),
        stop_completed: Some(true),
        failure: None,
    };

    expected.save(&path).await.expect("save receipt");
    let actual = ApplyAttemptReceipt::load(&path)
        .await
        .expect("load receipt")
        .expect("saved receipt");
    assert_eq!(actual, expected);
    assert_eq!(
        expected
            .output(
                Path::new("socket"),
                /*app_server_version*/ None,
                /*error*/ None
            )
            .status,
        ApplyStatus::InProgress
    );
    assert_eq!(
        expected
            .output(
                Path::new("socket"),
                /*app_server_version*/ None,
                /*error*/ None
            )
            .running_managed_codex_version,
        None
    );
    assert_eq!(
        expected
            .output_with_running_version(
                Path::new("socket"),
                /*app_server_version*/ None,
                /*error*/ None,
                Some("0.154.0-rick.2".to_string()),
            )
            .running_managed_codex_version,
        Some("0.154.0-rick.2".to_string())
    );
    assert!(
        !expected
            .output(
                Path::new("socket"),
                /*app_server_version*/ None,
                /*error*/ None
            )
            .can_retry
    );
}

#[test]
fn failed_preparation_can_be_retried_without_reusing_an_old_receipt() {
    let receipt = ApplyAttemptReceipt {
        handoff: receipt("needsAttention", vec![failed_preparation_node()]),
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: Some(false),
        stop_completed: Some(false),
        failure: Some("preparation failed".to_string()),
    };
    assert!(!receipt.blocks_new_apply());
    assert!(
        receipt
            .output(
                Path::new("socket"),
                /*app_server_version*/ None,
                /*error*/ None
            )
            .can_retry
    );
    assert!(
        !receipt
            .output(Path::new("socket"), None, None)
            .can_quarantine
    );
}

#[test]
fn preflight_failure_marker_allows_retry_without_replacing_the_old_runtime() {
    let receipt = ApplyAttemptReceipt {
        handoff: HandoffReceipt {
            transfer_started: Some(false),
            ..receipt(
                "needsAttention",
                vec![node("needsAttention", Some("still-running"))],
            )
        },
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: Some(false),
        stop_completed: Some(false),
        failure: Some("pending approval".to_string()),
    };
    assert!(!receipt.blocks_new_apply());
    let output = receipt.output(Path::new("socket"), None, None);
    assert!(output.can_retry);
    assert!(!output.can_quarantine);
}

#[test]
fn legacy_failed_receipts_remain_conservative() {
    let receipt = ApplyAttemptReceipt {
        handoff: receipt("needsAttention", vec![node("needsAttention", None)]),
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: None,
        stop_completed: None,
        failure: Some("unknown failure".to_string()),
    };
    assert!(receipt.blocks_new_apply());
}

#[test]
fn completed_legacy_stop_allows_recovery_to_restart_missing_replacement() {
    let mut receipt = ApplyAttemptReceipt {
        handoff: receipt(
            "needsAttention",
            vec![node("needsAttention", Some("turn-1"))],
        ),
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: Some(true),
        stop_completed: None,
        failure: Some("recovery failed after stopping the old runtime".to_string()),
    };
    assert!(receipt.should_hold_legacy_owner());

    receipt.stop_completed = Some(true);
    assert!(!receipt.should_hold_legacy_owner());
    assert!(receipt.blocks_new_apply());
}

#[test]
fn legacy_failed_preparation_receipts_with_no_running_nodes_can_retry() {
    let receipt = ApplyAttemptReceipt {
        handoff: HandoffReceipt {
            nodes: vec![serde_json::json!({
                "threadId": "thread-1",
                "rootThreadId": "thread-1",
                "state": "needsAttention",
                "turnId": null,
                "wasRunning": false,
            })],
            ..receipt("needsAttention", Vec::new())
        },
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: None,
        stop_completed: None,
        failure: Some("parentUnavailable".to_string()),
    };
    assert!(!receipt.blocks_new_apply());
    assert!(
        receipt
            .output(
                Path::new("socket"),
                /*app_server_version*/ None,
                /*error*/ None
            )
            .can_retry
    );
}

#[test]
fn malformed_legacy_failed_preparation_receipts_remain_fenced() {
    let receipt = ApplyAttemptReceipt {
        handoff: HandoffReceipt {
            nodes: vec![serde_json::json!({
                "state": "needsAttention",
                "turnId": null,
                "wasRunning": false,
            })],
            ..HandoffReceipt {
                handoff_id: "handoff-1".to_string(),
                state: "needsAttention".to_string(),
                runtime_version: "codex-0.2.0".to_string(),
                created_at: 1_757_712_000,
                quarantined: false,
                transfer_started: None,
                nodes: Vec::new(),
            }
        },
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: None,
        stop_completed: None,
        failure: Some("malformed receipt".to_string()),
    };

    assert!(receipt.blocks_new_apply());
    assert!(!receipt.output(Path::new("socket"), None, None).can_retry);
}

#[test]
fn legacy_failed_receipts_without_nodes_cannot_retry() {
    let receipt = ApplyAttemptReceipt {
        handoff: receipt("needsAttention", Vec::new()),
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: None,
        stop_completed: None,
        failure: Some("missing receipt nodes".to_string()),
    };
    assert!(receipt.blocks_new_apply());
    assert!(
        !receipt
            .output(
                Path::new("socket"),
                /*app_server_version*/ None,
                /*error*/ None
            )
            .can_retry
    );
}

#[test]
fn quarantined_receipts_allow_a_new_apply_without_claiming_completion() {
    let receipt = ApplyAttemptReceipt {
        handoff: HandoffReceipt {
            quarantined: true,
            ..receipt("needsAttention", vec![node("suspended", Some("turn-1"))])
        },
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: Some(true),
        stop_completed: Some(true),
        failure: Some("parentUnavailable".to_string()),
    };
    assert!(!receipt.blocks_new_apply());
    let output = receipt.output(Path::new("socket"), None, None);
    assert_eq!(output.status, ApplyStatus::NeedsAttention);
    assert!(output.quarantined);
    assert!(output.can_retry);
    assert!(!output.can_quarantine);
}

#[test]
fn active_needs_attention_receipts_remain_fenced_before_stop_completes() {
    let receipt = ApplyAttemptReceipt {
        handoff: HandoffReceipt {
            nodes: vec![serde_json::json!({
                "threadId": "thread-1",
                "rootThreadId": "thread-1",
                "state": "needsAttention",
                "turnId": "turn-1",
                "wasRunning": true,
            })],
            ..receipt("needsAttention", Vec::new())
        },
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: Some(false),
        stop_completed: Some(false),
        failure: Some("stop failed".to_string()),
    };
    assert!(receipt.blocks_new_apply());
    let output = receipt.output(Path::new("socket"), None, None);
    assert!(!output.can_retry);
    assert!(output.can_quarantine);
    let serialized = serde_json::to_value(output).expect("serialize apply output");
    assert_eq!(serialized["canQuarantine"], true);
    assert!(serialized.get("can_quarantine").is_none());
}

#[test]
fn malformed_quarantine_receipts_do_not_advertise_an_unusable_action() {
    let receipt = ApplyAttemptReceipt {
        handoff: HandoffReceipt {
            nodes: vec![serde_json::json!({
                "state": "needsAttention",
                "turnId": "turn-1",
                "wasRunning": true,
            })],
            ..receipt("needsAttention", Vec::new())
        },
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: Some(false),
        stop_completed: Some(false),
        failure: Some("malformed receipt".to_string()),
    };
    let output = receipt.output(Path::new("socket"), None, None);
    assert!(receipt.blocks_new_apply());
    assert!(!output.can_retry);
    assert!(!output.can_quarantine);
}

#[test]
fn malformed_quarantined_receipts_remain_fenced() {
    let malformed_state = ApplyAttemptReceipt {
        handoff: HandoffReceipt {
            state: "suspended".to_string(),
            quarantined: true,
            ..receipt("needsAttention", vec![node("suspended", Some("turn-1"))])
        },
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: Some(true),
        stop_completed: Some(true),
        failure: Some("malformed state".to_string()),
    };
    assert!(malformed_state.blocks_new_apply());
    assert!(
        !malformed_state
            .output(Path::new("socket"), None, None)
            .can_retry
    );

    let receipt = ApplyAttemptReceipt {
        handoff: HandoffReceipt {
            quarantined: true,
            ..receipt("suspended", vec![node("suspended", Some("turn-1"))])
        },
        phase: ApplyPhase::Prepared,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: Some(true),
        stop_completed: Some(true),
        failure: None,
    };
    assert!(receipt.blocks_new_apply());

    let malformed_needs_attention = ApplyAttemptReceipt {
        handoff: HandoffReceipt {
            quarantined: true,
            nodes: vec![serde_json::json!({
                "threadId": " ",
                "rootThreadId": "root-1",
                "state": "needsAttention",
            })],
            ..HandoffReceipt {
                handoff_id: "handoff-1".to_string(),
                state: "needsAttention".to_string(),
                runtime_version: "codex-0.2.0".to_string(),
                created_at: 1_757_712_000,
                quarantined: false,
                transfer_started: None,
                nodes: Vec::new(),
            }
        },
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: Some(true),
        stop_completed: Some(true),
        failure: Some("malformed quarantined receipt".to_string()),
    };
    assert!(malformed_needs_attention.blocks_new_apply());
    assert!(
        !malformed_needs_attention
            .output(Path::new("socket"), None, None)
            .can_retry
    );

    let unknown_node_state = ApplyAttemptReceipt {
        handoff: HandoffReceipt {
            quarantined: true,
            nodes: vec![serde_json::json!({
                "threadId": "thread-1",
                "rootThreadId": "thread-1",
                "state": "garbage",
            })],
            ..HandoffReceipt {
                handoff_id: "handoff-1".to_string(),
                state: "needsAttention".to_string(),
                runtime_version: "codex-0.2.0".to_string(),
                created_at: 1_757_712_000,
                quarantined: false,
                transfer_started: None,
                nodes: Vec::new(),
            }
        },
        phase: ApplyPhase::NeedsAttention,
        managed_codex_path: "/opt/homebrew/bin/codex-rick".into(),
        managed_codex_version: None,
        stop_started: Some(true),
        stop_completed: Some(true),
        failure: Some("unknown quarantined node state".to_string()),
    };
    assert!(unknown_node_state.blocks_new_apply());
}

#[test]
fn sanitizes_persisted_failures_to_single_line_bounded_text() {
    assert_eq!(sanitize_failure("connect\nfailed\t"), "connect failed ");
    assert_eq!(sanitize_failure(&"x".repeat(600)).len(), 512);
}
