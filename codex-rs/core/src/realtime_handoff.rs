use crate::context::RealtimeDelegationSource;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::realtime_handoff::configured_read_only_effort;

/// Selects a configured lower effort only for conservative, read-only realtime handoffs.
///
/// GPT-Live does not currently emit a typed effort or intent classification in its delegation
/// event. The client therefore opts in only when the handoff is clearly informational and keeps
/// the normal session effort for ambiguous or mutating requests.
pub(crate) fn non_substantive_realtime_reasoning_effort(
    source: RealtimeDelegationSource,
    input: &str,
    configured_effort: Option<&ReasoningEffort>,
) -> Option<ReasoningEffort> {
    if source != RealtimeDelegationSource::Handoff {
        return None;
    }

    configured_read_only_effort(input, configured_effort)
}

#[cfg(test)]
#[path = "realtime_handoff_tests.rs"]
mod tests;
