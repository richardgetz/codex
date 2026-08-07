use crate::context::RealtimeDelegationSource;
use codex_protocol::openai_models::ReasoningEffort;

const REALTIME_HANDOFF_CLASSIFICATION_MAX_BYTES: usize = 4_096;
const MUTATING_WORDS: &[&str] = &[
    "add",
    "apply",
    "build",
    "change",
    "commit",
    "create",
    "delete",
    "deploy",
    "dispatch",
    "edit",
    "fix",
    "implement",
    "install",
    "merge",
    "modify",
    "remove",
    "refactor",
    "release",
    "run",
    "send",
    "update",
    "write",
];
const READ_ONLY_WORDS: &[&str] = &[
    "are",
    "can",
    "check",
    "describe",
    "does",
    "do",
    "explain",
    "hear",
    "how",
    "inspect",
    "is",
    "know",
    "list",
    "look",
    "read",
    "show",
    "status",
    "summarize",
    "tell",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
];

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
    if source != RealtimeDelegationSource::Handoff || !is_conservative_read_only_request(input) {
        return None;
    }
    configured_effort.cloned()
}

fn is_conservative_read_only_request(input: &str) -> bool {
    if input.len() > REALTIME_HANDOFF_CLASSIFICATION_MAX_BYTES {
        return false;
    }

    let normalized = input.to_ascii_lowercase();
    let words = normalized
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    if words.is_empty() || words.iter().any(|word| MUTATING_WORDS.contains(word)) {
        return false;
    }

    normalized.trim_end().ends_with('?') || words.iter().any(|word| READ_ONLY_WORDS.contains(word))
}

#[cfg(test)]
#[path = "realtime_handoff_tests.rs"]
mod tests;
