use super::*;
use codex_core::config::Constrained;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::ThreadActivity;
use codex_protocol::protocol::ThreadActivityWaitReason;
use codex_protocol::protocol::ThreadPauseState;
use core_test_support::skip_if_wine_exec;
use pretty_assertions::assert_eq;
use std::fs;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_retains_approval_wait_without_duplicate_tool_or_model_calls() -> Result<()> {
    skip_if_wine_exec!(Ok(()), "command approval requires host-native paths");
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "activity-approval-call";
    let marker_name = "activity-side-effect.txt";
    let command = if cfg!(windows) {
        format!("echo x>>{marker_name}")
    } else {
        format!("printf x >> {marker_name}")
    };
    let tool_args = serde_json::to_string(&json!({
        "cmd": command,
        "yield_time_ms": 1_000,
    }))?;
    let first_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("activity-response-1"),
            ev_function_call(call_id, "exec_command", &tool_args),
            ev_completed("activity-response-1"),
        ]),
    )
    .await;
    let second_response = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("activity-message-2", "continued once"),
            ev_completed("activity-response-2"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
        config.permissions.approval_policy = Constrained::allow_any(AskForApproval::UnlessTrusted);
        config
            .permissions
            .set_permission_profile(PermissionProfile::workspace_write())
            .expect("set workspace-write permissions");
        config.approvals_reviewer = ApprovalsReviewer::User;
    });
    let test = builder.build(&server).await?;
    let marker_path = test.workspace_path(marker_name);
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "run one approved side effect, then finish".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let approval = match wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::ExecApprovalRequest(_) | EventMsg::TurnComplete(_)
        )
    })
    .await
    {
        EventMsg::ExecApprovalRequest(approval) => approval,
        EventMsg::TurnComplete(_) => panic!("expected approval request before completion"),
        other => panic!("unexpected event: {other:?}"),
    };

    test.codex.submit(Op::PauseActivity).await?;
    wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::ThreadActivityUpdated(state)
                if state.pause_state == ThreadPauseState::Paused
                    && state.activity == ThreadActivity::Waiting
                    && state.wait_reason == Some(ThreadActivityWaitReason::Approval)
        )
    })
    .await;
    assert_eq!(first_response.requests().len(), 1);
    assert!(
        !marker_path.exists(),
        "approval wait must not execute the side effect"
    );
    assert!(
        second_response.requests().is_empty(),
        "paused approval wait must not issue a follow-up model request"
    );

    test.codex
        .submit(Op::ExecApproval {
            id: approval.effective_approval_id(),
            turn_id: None,
            decision: ReviewDecision::Approved,
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::ThreadActivityUpdated(state)
                if state.pause_state == ThreadPauseState::Paused
                    && state.activity == ThreadActivity::Waiting
                    && state.wait_reason.is_none()
                    && state.in_flight_operations == 0
        )
    })
    .await;
    assert!(
        !marker_path.exists(),
        "approving while paused must not execute the side effect"
    );
    assert!(
        second_response.requests().is_empty(),
        "approval while paused must not start the side effect or model continuation"
    );

    test.codex.submit(Op::ContinueActivity).await?;
    wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::ThreadActivityUpdated(state)
                if state.pause_state == ThreadPauseState::Running
        )
    })
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert_eq!(first_response.requests().len(), 1);
    assert_eq!(second_response.requests().len(), 1);
    let continuation_request = second_response.single_request();
    assert_eq!(
        continuation_request.function_call_output(call_id)["call_id"],
        call_id
    );
    let output_count = continuation_request
        .input()
        .iter()
        .filter(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .count();
    assert_eq!(output_count, 1);
    let expected_marker = if cfg!(windows) { "x\n" } else { "x" };
    assert_eq!(fs::read_to_string(&marker_path)?, expected_marker);

    Ok(())
}
