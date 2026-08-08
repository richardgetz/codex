//! Shared classification for GPT-Live handoffs.
//!
//! The core router and the TUI's opt-in debug display both use this predicate so the displayed
//! effort decision describes the same client-side rule that selects a transient effort.

use crate::openai_models::ReasoningEffort;

const REALTIME_HANDOFF_CLASSIFICATION_MAX_BYTES: usize = 4_096;
const MUTATING_WORDS: &[&str] = &[
    "add",
    "apply",
    "build",
    "change",
    "commit",
    "choose",
    "create",
    "delete",
    "deploy",
    "dispatch",
    "edit",
    "enable",
    "execute",
    "fix",
    "implement",
    "install",
    "make",
    "merge",
    "migrate",
    "modify",
    "pick",
    "remove",
    "rename",
    "refactor",
    "release",
    "run",
    "select",
    "send",
    "set",
    "start",
    "stop",
    "switch",
    "test",
    "turn",
    "update",
    "use",
    "write",
];
const READ_ONLY_QUESTION_WORDS: &[&str] = &[
    "are", "how", "is", "what", "when", "where", "which", "who", "why",
];
const READ_ONLY_VERBS: &[&str] = &[
    "check",
    "describe",
    "explain",
    "hear",
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
];

/// Returns whether a GPT-Live handoff is conservative enough to use a read-only effort override.
pub fn is_conservative_read_only_request(input: &str) -> bool {
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

    let starts_with_read_only_question = words
        .first()
        .is_some_and(|word| READ_ONLY_QUESTION_WORDS.contains(word));
    let contains_read_only_verb = words.iter().any(|word| READ_ONLY_VERBS.contains(word));
    starts_with_read_only_question || contains_read_only_verb
}

/// Applies the configured read-only effort when the handoff qualifies, otherwise preserving the
/// caller's normal session effort by returning `None`.
pub fn configured_read_only_effort(
    input: &str,
    configured_effort: Option<&ReasoningEffort>,
) -> Option<ReasoningEffort> {
    is_conservative_read_only_request(input)
        .then(|| configured_effort.cloned())
        .flatten()
}

#[cfg(test)]
#[path = "realtime_handoff_tests.rs"]
mod tests;
