//! Shared classification for GPT-Live handoffs.
//!
//! The core router and the TUI's opt-in debug display both use this predicate so the displayed
//! effort decision describes the same client-side rule that selects a transient effort.

use crate::openai_models::ReasoningEffort;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Identifies which client-side classifier decided whether a realtime handoff was read-only.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum RealtimeHandoffClassifierKind {
    Text,
    Model,
}

/// Identifies why an optional model classifier fell back to text classification.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum RealtimeHandoffClassifierFallback {
    InputTooLong,
    InvalidOutput,
    RequestFailed,
    TimedOut,
}

/// Details about the classifier used for one realtime handoff.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RealtimeHandoffClassifier {
    pub kind: RealtimeHandoffClassifierKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub fallback: Option<RealtimeHandoffClassifierFallback>,
}

/// The classification and transient effort selected for one realtime handoff.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RealtimeHandoffRouting {
    pub classifier: RealtimeHandoffClassifier,
    pub classification: RealtimeHandoffClassification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub selected_effort: Option<ReasoningEffort>,
}

/// Result of classifying a realtime handoff's user request.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum RealtimeHandoffClassification {
    ReadOnly,
    Substantive,
}

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

/// Returns whether a request contains a word that should always prevent a model classifier from
/// selecting the transient read-only effort override.
pub fn contains_explicit_mutation_signal(input: &str) -> bool {
    if input.len() > REALTIME_HANDOFF_CLASSIFICATION_MAX_BYTES {
        return true;
    }

    let normalized = input.to_ascii_lowercase();
    normalized
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .any(|word| MUTATING_WORDS.contains(&word))
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
