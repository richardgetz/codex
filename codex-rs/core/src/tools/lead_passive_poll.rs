use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolPayload;
use crate::tools::router::ToolCall;

// Keep aligned with SleepHandler's accepted maximum in handlers/sleep.rs.
const MAX_TRACKED_SLEEP_DURATION_MS: u64 = 12 * 60 * 60 * 1000;

#[cfg(test)]
#[path = "lead_passive_poll_tests.rs"]
mod tests;

pub(crate) fn observe_lead_passive_poll_dispatch(
    turn_context: &TurnContext,
    call: &ToolCall,
    sample_id: u64,
) {
    let state = &turn_context.lead_passive_poll;
    if long_clock_sleep_duration_ms(call).is_some() {
        state.schedule_sleep(&call.call_id, sample_id);
        return;
    }
    match lead_passive_poll_observation(
        call,
        turn_context.config.multi_agent_v2.tool_namespace.as_deref(),
    ) {
        LeadPassivePollObservation::Transparent => {}
        LeadPassivePollObservation::StatusProbe => state.observed_status_probe(sample_id),
        LeadPassivePollObservation::SubstantiveWork => {
            state.observed_substantive_work(sample_id)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LeadPassivePollObservation {
    Transparent,
    StatusProbe,
    SubstantiveWork,
}

fn lead_passive_poll_observation(
    call: &ToolCall,
    configured_v2_namespace: Option<&str>,
) -> LeadPassivePollObservation {
    if call.tool_name.namespace.as_deref() == Some("clock") && call.tool_name.name == "sleep" {
        return LeadPassivePollObservation::Transparent;
    }
    if call.tool_name.is_default_namespace()
        && call.tool_name.name == codex_code_mode::PUBLIC_TOOL_NAME
        && matches!(&call.payload, ToolPayload::Custom { .. })
    {
        // The top-level Code Mode cell is only a wrapper. Its nested Core dispatches below
        // classify the actual operation and command.
        return LeadPassivePollObservation::Transparent;
    }
    let is_list_agents = call.tool_name.name == "list_agents"
        && (call.tool_name.is_default_namespace()
            || configured_v2_namespace.is_some_and(|namespace| {
                call.tool_name.namespace.as_deref() == Some(namespace)
            }));
    let is_worker_capacity = call.tool_name.name == "worker_capacity"
        && (call.tool_name.is_default_namespace()
            || call.tool_name.namespace.as_deref() == Some("multi_agent_v1"));
    if is_list_agents || is_worker_capacity {
        return LeadPassivePollObservation::StatusProbe;
    }
    if call.tool_name.is_default_namespace() && call.tool_name.name == "exec_command"
        && let ToolPayload::Function { arguments } = &call.payload
            && serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|value| value.get("cmd")?.as_str().map(str::to_string))
                .as_deref()
                .is_some_and(is_read_only_gh_run_view)
        {
            return LeadPassivePollObservation::StatusProbe;
        }
    LeadPassivePollObservation::SubstantiveWork
}

fn long_clock_sleep_duration_ms(call: &ToolCall) -> Option<u64> {
    if call.tool_name.namespace.as_deref() != Some("clock") || call.tool_name.name != "sleep" {
        return None;
    }
    let ToolPayload::Function { arguments } = &call.payload else {
        return None;
    };
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let duration_ms = value.get("duration_ms")?.as_u64()?;
    (30_000..=MAX_TRACKED_SLEEP_DURATION_MS)
        .contains(&duration_ms)
        .then_some(duration_ms)
}

fn is_read_only_gh_run_view(command: &str) -> bool {
    if command.chars().any(char::is_control) {
        return false;
    }
    let Some(tokens) = shlex::split(command) else {
        return false;
    };
    match tokens.as_slice() {
        [gh, run, view, run_id, json, jobs, jq, filter]
            if gh == "gh"
                && run == "run"
                && view == "view"
                && is_numeric_id(run_id)
                && json == "--json"
                && jobs == "jobs"
                && jq == "--jq"
                && is_safe_jq_filter(filter)
                && is_quoted_jq_filter_command(command, run_id, filter) =>
        {
            true
        }
        [gh, run, view, job, job_id, log, pipe, tail, count]
            if gh == "gh"
                && run == "run"
                && view == "view"
                && job == "--job"
                && is_numeric_id(job_id)
                && log == "--log"
                && pipe == "|"
                && tail == "tail"
                && count == "-30"
                && command == format!("gh run view --job {job_id} --log | tail -30").as_str() =>
        {
            true
        }
        _ => false,
    }
}

fn is_numeric_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_safe_jq_filter(filter: &str) -> bool {
    !filter.is_empty()
        && filter.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == ' '
                || matches!(
                    character,
                    '_' | '.' | '[' | ']' | '{' | '}' | '(' | ')' | '?' | ':' | ',' | '|'
                        | '=' | '+' | '-' | '*' | '/' | '"' | '\''
                )
        })
}

fn is_quoted_jq_filter_command(command: &str, run_id: &str, filter: &str) -> bool {
    let prefix = format!("gh run view {run_id} --json jobs --jq ");
    let Some(quoted_filter) = command.strip_prefix(&prefix) else {
        return false;
    };
    let Some(quote) = quoted_filter.as_bytes().first().copied() else {
        return false;
    };
    if !matches!(quote, b'\'' | b'"') || quoted_filter.as_bytes().last() != Some(&quote) {
        return false;
    }
    let Some(raw_filter) = quoted_filter.get(1..quoted_filter.len() - 1) else {
        return false;
    };
    raw_filter == filter
        && !raw_filter
            .chars()
            .any(|character| character == '\\' || character == quote as char)
}
