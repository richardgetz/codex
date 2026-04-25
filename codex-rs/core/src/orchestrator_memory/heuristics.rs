use super::types::ASSISTANT_ACKNOWLEDGED_CONFIDENCE;
use super::types::CandidateMemoryItem;
use super::types::EXPLICIT_CONFIDENCE;
use super::types::MemoryBucket;
use super::types::MemoryOperation;
use super::types::MemorySignal;
use super::types::REPEATED_STEERING_CONFIDENCE;
use crate::content_items_to_text;
use crate::context_manager::is_user_turn_boundary;
use codex_protocol::models::ResponseItem;
use std::cmp::Reverse;
use std::collections::HashSet;

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "be", "but", "by", "for", "from", "i", "if", "in", "into", "is",
    "it", "me", "my", "of", "on", "or", "so", "that", "the", "things", "to", "we", "when", "with",
    "you", "your",
];
const HIGH_SIGNAL_PREFIXES: &[&str] = &[
    "remember",
    "remember that",
    "i prefer",
    "from now on",
    "always",
    "never",
];
const DIRECTIVE_PREFIXES: &[&str] = &[
    "i want",
    "i expect",
    "you should",
    "you will",
    "don't",
    "do not",
    "focus on",
    "just focus on",
    "skip",
];
const WORKFLOW_TERMS: &[&str] = &[
    "abstraction layer",
    "agent",
    "agents",
    "ambigu",
    "assistant",
    "clarif",
    "communicat",
    "delegat",
    "middle man",
    "orchestrator",
    "preference",
    "session",
    "steer",
    "unclear",
    "workflow",
];
const EXPLICIT_REMEMBER_MARKERS: &[&str] = &[
    "remember this",
    "remember that",
    "remember it",
    "can you remember",
    "could you remember",
    "please remember",
    "keep this",
    "keep that",
    "keep it",
    "keep this for later",
    "keep that for later",
    "keep it for later",
    "bookmark this",
    "bookmark that",
    "bookmark it",
    "save this",
    "save that",
    "save it",
    "you should remember",
    "you should keep",
    "you should bookmark",
    "you should save",
    "you will remember",
    "you will keep",
    "you will bookmark",
    "you will save",
    "don't forget this",
    "do not forget this",
    "don't forget that",
    "do not forget that",
    "don't forget it",
    "do not forget it",
];
const EXPLICIT_FORGET_MARKERS: &[&str] = &[
    "forget this",
    "forget that",
    "don't remember that",
    "do not remember that",
    "drop that",
    "clear that",
    "remove that from memory",
    "don't carry that forward",
    "do not carry that forward",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ForcedMemoryTrigger {
    Remember,
    Forget,
}

pub(super) fn extract_candidate_preferences(
    current_turn_user_texts: &[String],
    recent_user_turns: &[String],
    last_agent_message: Option<&str>,
    min_observations: usize,
) -> Vec<CandidateMemoryItem> {
    let mut candidates = current_turn_user_texts
        .iter()
        .flat_map(|text| extract_explicit_preferences(text))
        .collect::<Vec<_>>();
    if let Some(last_agent_message) = last_agent_message {
        candidates.extend(extract_acknowledged_preferences(last_agent_message));
    }

    let repeated = extract_repeated_steering_preferences(recent_user_turns, min_observations);
    let repeated_keys = repeated
        .iter()
        .map(|candidate| candidate.key.clone())
        .collect::<HashSet<_>>();
    let filtered_repeated = repeated
        .into_iter()
        .filter(|candidate| {
            !candidates
                .iter()
                .any(|existing| existing.key == candidate.key)
        })
        .collect::<Vec<_>>();
    candidates.extend(filtered_repeated);

    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for candidate in candidates {
        if seen.insert((
            candidate.bucket,
            candidate.operation,
            candidate.signal,
            candidate.key.clone(),
            candidate.candidate.clone(),
        )) {
            deduped.push(candidate);
        }
    }

    if !repeated_keys.is_empty() {
        deduped.sort_by_key(|candidate| Reverse(candidate.signal == MemorySignal::Explicit));
    }

    deduped
}

pub(super) fn extract_acknowledged_preferences(text: &str) -> Vec<CandidateMemoryItem> {
    let lowered = text.to_ascii_lowercase();
    let acknowledgement_markers = [
        "i'll treat",
        "i will treat",
        "i'll apply",
        "i will apply",
        "going forward",
        "hard rule",
        "policy going forward",
        "i'll use this",
        "i will use this",
    ];
    if !acknowledgement_markers
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        return Vec::new();
    }

    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim().trim_start_matches(|ch: char| {
                matches!(ch, '-' | '*' | '•' | ' ' | '\t')
                    || ch.is_ascii_digit()
                    || matches!(ch, '.' | ')')
            });
            if trimmed.is_empty() || !looks_like_preference_sentence(trimmed) {
                return None;
            }
            let candidate = candidate_text_from_sentence(trimmed);
            let key = normalized_key(&candidate);
            (!key.is_empty()).then_some(CandidateMemoryItem {
                bucket: MemoryBucket::DurablePreference,
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::AssistantAcknowledged,
                key,
                candidate,
                source_excerpt: normalize_sentence(trimmed),
                confidence: ASSISTANT_ACKNOWLEDGED_CONFIDENCE,
            })
        })
        .collect()
}

pub(super) fn extract_directive_sentences(text: &str) -> Vec<String> {
    split_sentences(text)
        .into_iter()
        .map(|sentence| normalize_sentence(&sentence))
        .filter(|sentence| looks_like_preference_sentence(sentence))
        .collect()
}

pub(super) fn collect_recent_user_turns(items: &[ResponseItem], limit: usize) -> Vec<String> {
    items
        .iter()
        .rev()
        .filter_map(|item| {
            let ResponseItem::Message { role, content, .. } = item else {
                return None;
            };
            if role != "user" || !is_user_turn_boundary(item) {
                return None;
            }
            content_items_to_text(content)
        })
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

pub(super) fn collect_current_turn_user_texts(items: &[ResponseItem]) -> Vec<String> {
    let mut collected = Vec::new();
    let mut saw_current_turn_assistant = false;

    for item in items.iter().rev() {
        let ResponseItem::Message { role, content, .. } = item else {
            if saw_current_turn_assistant && !collected.is_empty() {
                continue;
            }
            continue;
        };

        if role == "assistant" {
            if !saw_current_turn_assistant {
                saw_current_turn_assistant = true;
                continue;
            }
            if !collected.is_empty() {
                break;
            }
            continue;
        }

        if role == "user"
            && saw_current_turn_assistant
            && is_user_turn_boundary(item)
            && let Some(text) = content_items_to_text(content)
        {
            collected.push(text);
        }
    }

    collected.into_iter().rev().collect()
}

pub(super) fn detect_forced_memory_trigger(
    current_turn_user_texts: &[String],
) -> Option<ForcedMemoryTrigger> {
    let mut saw_remember = false;
    for text in current_turn_user_texts {
        let lowered = text.to_ascii_lowercase();
        if EXPLICIT_FORGET_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
        {
            return Some(ForcedMemoryTrigger::Forget);
        }
        if EXPLICIT_REMEMBER_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
        {
            saw_remember = true;
        }
    }

    saw_remember.then_some(ForcedMemoryTrigger::Remember)
}

pub(super) fn extract_explicit_preferences(text: &str) -> Vec<CandidateMemoryItem> {
    split_sentences(text)
        .into_iter()
        .filter_map(|sentence| extract_explicit_preference(&sentence))
        .collect()
}

fn extract_explicit_preference(sentence: &str) -> Option<CandidateMemoryItem> {
    let sentence = normalize_sentence(sentence);
    if sentence.is_empty() || !looks_like_preference_sentence(&sentence) {
        return None;
    }

    let lowered = sentence.to_lowercase();
    let is_explicit = HIGH_SIGNAL_PREFIXES
        .iter()
        .chain(DIRECTIVE_PREFIXES)
        .any(|prefix| lowered.starts_with(prefix));
    if !is_explicit {
        return None;
    }
    if contains_memory_intent(&lowered) {
        return None;
    }

    let candidate = candidate_text_from_sentence(&sentence);
    let key = normalized_key(&candidate);
    if key.is_empty() || is_low_quality_memory_candidate(&key) {
        return None;
    }

    Some(CandidateMemoryItem {
        bucket: MemoryBucket::DurablePreference,
        operation: MemoryOperation::Upsert,
        signal: MemorySignal::Explicit,
        key,
        candidate,
        source_excerpt: sentence,
        confidence: EXPLICIT_CONFIDENCE,
    })
}

fn contains_memory_intent(lowered: &str) -> bool {
    EXPLICIT_REMEMBER_MARKERS
        .iter()
        .chain(EXPLICIT_FORGET_MARKERS)
        .any(|marker| lowered.contains(marker))
}

fn is_low_quality_memory_candidate(key: &str) -> bool {
    matches!(
        key,
        "it" | "that" | "this" | "them" | "those" | "these" | "he" | "she" | "they"
    )
}

pub(super) fn extract_repeated_steering_preferences(
    recent_user_turns: &[String],
    min_observations: usize,
) -> Vec<CandidateMemoryItem> {
    let directives = recent_user_turns
        .iter()
        .flat_map(|turn| {
            extract_directive_sentences(turn)
                .into_iter()
                .map(|sentence| {
                    let candidate = candidate_text_from_sentence(&sentence);
                    let tokens = similarity_tokens(&candidate);
                    (sentence, candidate, tokens)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let mut results = Vec::new();
    let mut emitted = HashSet::new();
    for (index, (_sentence, candidate, tokens)) in directives.iter().enumerate() {
        if tokens.is_empty() {
            continue;
        }
        let mut cluster_size = 1usize;
        for (other_index, (_other_sentence, other_candidate, other_tokens)) in
            directives.iter().enumerate()
        {
            if index == other_index || candidate == other_candidate {
                continue;
            }
            if token_similarity(tokens, other_tokens) >= 0.5 {
                cluster_size += 1;
            }
        }
        if cluster_size < min_observations {
            continue;
        }
        let key = normalized_key(candidate);
        if emitted.insert(key.clone()) {
            results.push(CandidateMemoryItem {
                bucket: MemoryBucket::DurablePreference,
                operation: MemoryOperation::Upsert,
                signal: MemorySignal::RepeatedSteering,
                key,
                candidate: candidate.clone(),
                source_excerpt: candidate.clone(),
                confidence: REPEATED_STEERING_CONFIDENCE,
            });
        }
    }

    results
}

fn looks_like_preference_sentence(sentence: &str) -> bool {
    let lowered = sentence.to_lowercase();
    if HIGH_SIGNAL_PREFIXES
        .iter()
        .chain(DIRECTIVE_PREFIXES)
        .any(|prefix| lowered.starts_with(prefix))
    {
        return true;
    }

    let imperative_starts = [
        "clarify", "ask", "focus", "skip", "delegate", "please", "when",
    ];
    WORKFLOW_TERMS.iter().any(|term| lowered.contains(term))
        && (lowered.contains("you")
            || imperative_starts
                .iter()
                .any(|prefix| lowered.starts_with(prefix)))
}

fn candidate_text_from_sentence(sentence: &str) -> String {
    let normalized = normalize_sentence(sentence);
    let lowered = normalized.to_lowercase();
    for prefix in [
        "remember that ",
        "remember ",
        "from now on ",
        "i prefer ",
        "i want ",
        "i expect ",
        "you should ",
        "you will ",
        "just focus on ",
        "focus on ",
        "skip ",
    ] {
        if let Some(rest) = lowered.strip_prefix(prefix) {
            let rest = normalize_sentence(rest);
            return match prefix {
                "i prefer " => format!("Prefer {rest}"),
                "just focus on " | "focus on " => format!("Focus on {rest}"),
                "skip " => format!("Skip {rest}"),
                _ => candidate_text_from_sentence(&rest),
            };
        }
    }
    if let Some(rest) = lowered.strip_prefix("don't ") {
        return format!("Do not {}", normalize_sentence(rest));
    }
    if let Some(rest) = lowered.strip_prefix("do not ") {
        return format!("Do not {}", normalize_sentence(rest));
    }

    capitalize_sentence(&normalized)
}

pub(super) fn normalized_key(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch.is_whitespace() {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn similarity_tokens(text: &str) -> HashSet<String> {
    normalized_key(text)
        .split_whitespace()
        .filter(|token| token.len() > 2 && !STOPWORDS.contains(token))
        .map(normalize_similarity_token)
        .collect()
}

fn token_similarity(left: &HashSet<String>, right: &HashSet<String>) -> f32 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let overlap = left.intersection(right).count() as f32;
    let max_len = left.len().max(right.len()) as f32;
    overlap / max_len
}

fn normalize_similarity_token(token: &str) -> String {
    let mut normalized = token.to_string();
    for suffix in ["ing", "ed", "es", "s", "ity"] {
        if normalized.len() > suffix.len() + 2 && normalized.ends_with(suffix) {
            normalized.truncate(normalized.len() - suffix.len());
            break;
        }
    }
    normalized
}

fn capitalize_sentence(text: &str) -> String {
    let trimmed = normalize_sentence(text);
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn normalize_sentence(text: &str) -> String {
    text.trim()
        .trim_matches(|ch: char| ch.is_ascii_punctuation() || ch.is_whitespace())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut current = String::new();
    let mut sentences = Vec::new();
    for ch in text.chars() {
        if matches!(ch, '.' | '!' | '?' | '\n') {
            let sentence = normalize_sentence(&current);
            if !sentence.is_empty() {
                sentences.push(sentence);
            }
            current.clear();
        } else {
            current.push(ch);
        }
    }

    let sentence = normalize_sentence(&current);
    if !sentence.is_empty() {
        sentences.push(sentence);
    }
    sentences
}
