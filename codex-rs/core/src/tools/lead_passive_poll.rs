use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolPayload;
use crate::tools::router::ToolCall;

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
    if call.tool_name.is_default_namespace() && call.tool_name.name == "exec_command" {
        if let ToolPayload::Function { arguments } = &call.payload
            && serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|value| value.get("cmd")?.as_str().map(str::to_string))
                .as_deref()
                .is_some_and(is_read_only_gh_run_view)
        {
            return LeadPassivePollObservation::StatusProbe;
        }
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
    (duration_ms >= 30_000).then_some(duration_ms)
}

fn is_read_only_gh_run_view(command: &str) -> bool {
    let Some(tokens) = shlex::split(command) else {
        return false;
    };
    if tokens.len() < 4 || tokens[0] != "gh" || tokens[1] != "run" || tokens[2] != "view" {
        return false;
    }
    let pipeline_index = tokens.iter().position(|token| token == "|");
    if let Some(index) = pipeline_index
        && (tokens.len() - index != 3
            || tokens[index + 1] != "tail"
            || tokens[index + 2] != "-30")
    {
        return false;
    }
    let command_tokens = pipeline_index.map_or(tokens.as_slice(), |index| &tokens[..index]);
    !command_tokens
        .iter()
        .any(|token| matches!(token.as_str(), ";" | "&&" | "||" | ">" | "<" | "--web"))
}
