use codex_git_utils::GitIntentNote;
use codex_git_utils::recent_git_intent_notes;
use codex_state::decision_provenance::PrivacyClass;
use codex_state::decision_provenance::SourceReference;
use std::collections::HashSet;
use std::path::Path;

const MAX_RELEVANT_GIT_INTENT_NOTES: usize = 4;

pub(super) struct GitIntentConflict {
    pub(super) commit: String,
    pub(super) source_ref: SourceReference,
}

pub(super) async fn find_git_intent_conflicts(
    cwd: &Path,
    request_text: &str,
    request_tokens: &HashSet<String>,
) -> Vec<GitIntentConflict> {
    if !looks_like_code_change_request(request_text) {
        return Vec::new();
    }

    recent_git_intent_notes(cwd, 64)
        .await
        .into_iter()
        .filter_map(|note| relevant_git_intent_conflict(note, request_tokens))
        .take(MAX_RELEVANT_GIT_INTENT_NOTES)
        .collect()
}

fn relevant_git_intent_conflict(
    note: GitIntentNote,
    request_tokens: &HashSet<String>,
) -> Option<GitIntentConflict> {
    if !has_must_priority(&note.body) || !note_matches_request(&note.body, request_tokens) {
        return None;
    }

    let source_ref = SourceReference {
        source_type: "git_intent_note".to_string(),
        reference: format!("refs/notes/intention@{}", note.commit),
        label: Some("must-level Git intent note".to_string()),
        privacy: PrivacyClass::Private,
    };
    Some(GitIntentConflict {
        commit: note.commit,
        source_ref,
    })
}

fn looks_like_code_change_request(request_text: &str) -> bool {
    const CODE_CHANGE_MARKERS: [&str; 18] = [
        " add ",
        " change ",
        " delete ",
        " fix ",
        " implement ",
        " modify ",
        " refactor ",
        " remove ",
        " rename ",
        " replace ",
        " update ",
        " behavior ",
        " contract ",
        " generated ",
        " invariant ",
        " api ",
        " commit ",
        " pull request ",
    ];
    let normalized = format!(" {} ", request_text.to_ascii_lowercase());
    CODE_CHANGE_MARKERS
        .iter()
        .any(|marker| normalized.contains(marker))
}

fn has_must_priority(note: &str) -> bool {
    note.lines().any(|line| {
        let Some((key, value)) = line.split_once(':') else {
            return false;
        };
        key.trim() == "intent_priority"
            && value
                .trim()
                .trim_matches(['"', '\''])
                .eq_ignore_ascii_case("must")
    })
}

fn note_matches_request(note: &str, request_tokens: &HashSet<String>) -> bool {
    let note_tokens = intent_tokens(note);
    note_tokens.intersection(request_tokens).count() >= 2
}

pub(super) fn request_has_explicit_git_intent_override(request_text: &str) -> bool {
    let request_text = request_text.to_ascii_lowercase();
    [
        "override the prior git intent",
        "override the git intent",
        "override this git intent",
        "approve overriding the prior git intent",
        "approve overriding the git intent",
        "proceed despite the prior git intent",
        "proceed despite the git intent",
    ]
    .iter()
    .any(|marker| request_text.contains(marker))
}

fn intent_tokens(text: &str) -> HashSet<String> {
    const STOP_WORDS: [&str; 33] = [
        "add",
        "agent",
        "and",
        "change",
        "codex",
        "commit",
        "current",
        "decision",
        "do",
        "for",
        "git",
        "intent",
        "is",
        "keep",
        "must",
        "not",
        "note",
        "only",
        "preserve",
        "request",
        "repository",
        "scope",
        "should",
        "the",
        "this",
        "to",
        "user",
        "with",
        "without",
        "you",
        "your",
        "refs",
        "notes",
    ];
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .filter_map(|token| {
            let token = token.to_ascii_lowercase();
            if token.len() < 3 || STOP_WORDS.contains(&token.as_str()) {
                None
            } else {
                Some(token)
            }
        })
        .take(96)
        .collect()
}

#[cfg(test)]
#[path = "git_intent_preflight_tests.rs"]
mod tests;
