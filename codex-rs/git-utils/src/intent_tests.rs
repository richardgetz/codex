use super::*;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::process::Command;

fn run_git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .current_dir(repo)
        .status()
        .expect("run Git command");
    assert_eq!(status.code(), Some(0), "Git command failed: {args:?}");
}

#[tokio::test]
async fn recent_git_intent_notes_reads_only_note_targets() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let repo = temp_dir.path();
    run_git(repo, &["init", "-q", "--initial-branch=main"]);
    run_git(
        repo,
        &[
            "-c",
            "user.name=Codex Tests",
            "-c",
            "user.email=codex-tests@example.com",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "intent target",
        ],
    );
    let commit = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .expect("read commit")
            .stdout,
    )
    .expect("commit should be UTF-8")
    .trim()
    .to_string();
    run_git(
        repo,
        &[
            "notes",
            "--ref=refs/notes/intention",
            "add",
            "-m",
            "intent_priority: must\ndecision: preserve the contract",
            &commit,
        ],
    );

    let notes = recent_git_intent_notes(repo, 10).await;
    let timestamp = notes
        .first()
        .map(|note| note.timestamp)
        .expect("note should be found");

    assert_eq!(
        notes,
        vec![GitIntentNote {
            commit,
            timestamp,
            subject: "intent target".to_string(),
            body: "intent_priority: must\ndecision: preserve the contract".to_string(),
        }]
    );
}

#[tokio::test]
async fn recent_git_intent_notes_returns_empty_outside_a_repository() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");

    assert!(
        recent_git_intent_notes(temp_dir.path(), 10)
            .await
            .is_empty()
    );
}
