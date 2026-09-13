use std::path::Path;

use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use pretty_assertions::assert_eq;
use serde_json::Value;

use super::ensure_transferable_handoff;
use super::parse_handoff_response;
use super::sanitize_failure;
use super::ApplyAttemptReceipt;
use super::ApplyPhase;
use super::ApplyStatus;
use super::HandoffReceipt;

fn receipt(state: &str, nodes: Vec<Value>) -> HandoffReceipt {
    HandoffReceipt {
        handoff_id: "handoff-1".to_string(),
        state: state.to_string(),
        runtime_version: "codex-0.2.0".to_string(),
        created_at: 1_757_712_000,
        nodes,
    }
}

fn node(state: &str, turn_id: Option<&str>) -> Value {
    let mut node = serde_json::json!({
        "threadId": "thread-1",
        "state": state,
    });
    if let Some(turn_id) = turn_id {
        node["turnId"] = Value::String(turn_id.to_string());
    }
    node
}

#[test]
fn transferability_requires_a_fully_suspended_graph() {
    let safe = receipt(
        "suspended",
        vec![node("suspended", Some("turn-1")), node("notActive", None)],
    );
    assert!(ensure_transferable_handoff(&safe).is_ok());

    let partial = receipt("draining", vec![node("suspended", Some("turn-1"))]);
    let error = ensure_transferable_handoff(&partial).expect_err("draining graph must not transfer");
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
        result: serde_json::json!({ "receipt": handoff.clone() }),
    });
    let parsed = parse_handoff_response(success, "thread/handoff/status").expect("receipt");
    assert_eq!(parsed, handoff);

    let error_receipt = receipt("needsAttention", vec![node("blocked", None)]);
    let error_message = JSONRPCMessage::Error(JSONRPCError {
        id: RequestId::Integer(2),
        error: JSONRPCErrorError {
            code: -32000,
            message: "handoff blocked".to_string(),
            data: Some(serde_json::json!({ "receipt": error_receipt.clone() })),
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
        failure: None,
    };

    expected.save(&path).await.expect("save receipt");
    let actual = ApplyAttemptReceipt::load(&path)
        .await
        .expect("load receipt")
        .expect("saved receipt");
    assert_eq!(actual, expected);
    assert_eq!(
        expected.output(Path::new("socket"), None, None).status,
        ApplyStatus::InProgress
    );
}

#[test]
fn sanitizes_persisted_failures_to_single_line_bounded_text() {
    assert_eq!(sanitize_failure("connect\nfailed\t"), "connect failed ");
    assert_eq!(sanitize_failure(&"x".repeat(600)).len(), 512);
}
