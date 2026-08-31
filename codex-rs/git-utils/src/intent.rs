use std::path::Path;

const MAX_GIT_INTENT_NOTE_COMMITS: usize = 64;
const MAX_GIT_INTENT_NOTE_BYTES: usize = 16 * 1024;

/// A bounded commit and note pair read from `refs/notes/intention`.
///
/// The note body is returned to the caller for local, request-scoped parsing.
/// Callers must avoid persisting the body when a smaller source reference is
/// sufficient for the decision record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitIntentNote {
    pub commit: String,
    pub timestamp: i64,
    pub subject: String,
    pub body: String,
}

/// Read recent commit intent notes without modifying the repository.
///
/// Git remains the source of truth for the note. This helper only returns a
/// bounded snapshot so a caller can decide whether a note is relevant to the
/// current request; timeouts and Git failures produce an empty result.
pub async fn recent_git_intent_notes(cwd: &Path, limit: usize) -> Vec<GitIntentNote> {
    let limit = limit.min(MAX_GIT_INTENT_NOTE_COMMITS);
    if limit == 0 {
        return Vec::new();
    }

    let limit_arg = limit.to_string();
    let args = [
        "log",
        "--notes=refs/notes/intention",
        "--format=%H%x1f%ct%x1f%s%x1f%N%x1e",
        "-n",
        limit_arg.as_str(),
    ];

    let Some(output) = crate::info::run_git_command_with_timeout_from(
        Path::new("git"),
        &args,
        cwd,
        crate::FsmonitorOverride::Disabled,
    )
    .await
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .split('\u{001e}')
        .filter_map(parse_git_intent_note)
        .collect()
}

fn parse_git_intent_note(record: &str) -> Option<GitIntentNote> {
    let mut fields = record.trim_matches('\n').splitn(4, '\u{001f}');
    let commit = fields.next()?.trim();
    let timestamp = fields.next()?.trim().parse().ok()?;
    let subject = fields.next()?.trim();
    let body = bounded_note_body(fields.next()?.trim());
    if commit.is_empty() || body.is_empty() {
        return None;
    }

    Some(GitIntentNote {
        commit: commit.to_string(),
        timestamp,
        subject: subject.to_string(),
        body,
    })
}

fn bounded_note_body(body: &str) -> String {
    if body.len() <= MAX_GIT_INTENT_NOTE_BYTES {
        return body.to_string();
    }

    body.char_indices()
        .take_while(|(index, _)| *index < MAX_GIT_INTENT_NOTE_BYTES)
        .map(|(_, character)| character)
        .collect()
}

#[cfg(test)]
#[path = "intent_tests.rs"]
mod tests;
