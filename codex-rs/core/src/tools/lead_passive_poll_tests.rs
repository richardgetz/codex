use super::LeadPassivePollObservation;
use super::lead_passive_poll_observation;
use super::long_clock_sleep_duration_ms;
use crate::tools::context::ToolPayload;
use crate::tools::router::ToolCall;
use codex_tools::ToolName;

fn tool_call(tool_name: ToolName, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        tool_name,
        call_id: "test-call".to_string(),
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
        encrypted_function_args: None,
    }
}

fn exec_command(command: &str) -> ToolCall {
    tool_call(
        ToolName::plain("exec_command"),
        serde_json::json!({ "cmd": command }),
    )
}

fn clock_sleep(duration_ms: u64) -> ToolCall {
    tool_call(
        ToolName::namespaced("clock", "sleep"),
        serde_json::json!({ "duration_ms": duration_ms }),
    )
}

#[test]
fn status_tools_only_match_expected_namespaces() {
    assert_eq!(
        lead_passive_poll_observation(
            &tool_call(ToolName::plain("list_agents"), serde_json::json!({})),
            None,
        ),
        LeadPassivePollObservation::StatusProbe,
    );
    assert_eq!(
        lead_passive_poll_observation(
            &tool_call(
                ToolName::namespaced("collaboration", "list_agents"),
                serde_json::json!({}),
            ),
            Some("collaboration"),
        ),
        LeadPassivePollObservation::StatusProbe,
    );
    assert_eq!(
        lead_passive_poll_observation(
            &tool_call(
                ToolName::namespaced("other", "list_agents"),
                serde_json::json!({}),
            ),
            Some("collaboration"),
        ),
        LeadPassivePollObservation::SubstantiveWork,
    );
    assert_eq!(
        lead_passive_poll_observation(
            &tool_call(
                ToolName::namespaced("multi_agent_v1", "worker_capacity"),
                serde_json::json!({}),
            ),
            Some("collaboration"),
        ),
        LeadPassivePollObservation::StatusProbe,
    );
    assert_eq!(
        lead_passive_poll_observation(
            &tool_call(ToolName::plain("worker_capacity"), serde_json::json!({})),
            Some("collaboration"),
        ),
        LeadPassivePollObservation::StatusProbe,
    );
    assert_eq!(
        lead_passive_poll_observation(
            &tool_call(
                ToolName::namespaced("collaboration", "worker_capacity"),
                serde_json::json!({}),
            ),
            Some("collaboration"),
        ),
        LeadPassivePollObservation::SubstantiveWork,
    );
}

#[test]
fn only_observed_read_only_gh_run_commands_are_status_probes() {
    assert_eq!(
        lead_passive_poll_observation(
            &exec_command(
                "gh run view 123456 --json jobs --jq '.jobs[] | select(.status == \"completed\") | .name'",
            ),
            None,
        ),
        LeadPassivePollObservation::StatusProbe,
    );
    assert_eq!(
        lead_passive_poll_observation(
            &exec_command("gh run view --job 987654 --log | tail -30"),
            None,
        ),
        LeadPassivePollObservation::StatusProbe,
    );

    for command in [
        "pwd",
        "gh run list",
        "gh run view 123456 --json jobs --jq '.jobs[]' ; gh run rerun 123456",
        "gh run view 123456 --json jobs --jq .jobs[]|touch /tmp/x",
        "gh run view 123456 --json jobs --jq '$(touch /tmp/x)'",
        "gh run view 123456 --json jobs --jq '`touch /tmp/x`'",
        "gh run view --job 987654 --log && touch /tmp/x",
        "gh run rerun 123456",
    ] {
        assert_eq!(
            lead_passive_poll_observation(&exec_command(command), None),
            LeadPassivePollObservation::SubstantiveWork,
            "command should not count as a status probe: {command}"
        );
    }
}

#[test]
fn only_valid_long_clock_sleeps_are_tracked() {
    assert_eq!(long_clock_sleep_duration_ms(&clock_sleep(29_999)), None);
    assert_eq!(
        long_clock_sleep_duration_ms(&clock_sleep(30_000)),
        Some(30_000)
    );
    assert_eq!(
        long_clock_sleep_duration_ms(&clock_sleep(43_200_000)),
        Some(43_200_000)
    );
    assert_eq!(long_clock_sleep_duration_ms(&clock_sleep(43_200_001)), None);
}
