use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_remote;
use core_test_support::test_codex::test_codex;
use std::path::Path;
use std::process::Command;

fn run_git(cwd: &Path, args: &[&str]) -> String {
    let null_config = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let output = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", null_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Codex Tests")
        .env("GIT_AUTHOR_EMAIL", "codex-tests@example.com")
        .env("GIT_COMMITTER_NAME", "Codex Tests")
        .env("GIT_COMMITTER_EMAIL", "codex-tests@example.com")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run Git test command");
    assert!(
        output.status.success(),
        "Git command failed: {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git output should be UTF-8")
        .trim()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_candidate_is_bounded_context_and_never_blocks_model_flow() -> anyhow::Result<()>
{
    skip_if_no_network!(Ok(()));
    skip_if_remote!(
        Ok(()),
        "Git intent fixture requires a host-local repository"
    );

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.decision_provenance.enabled = true;
        config.decision_provenance.git_intent_bridge = true;
    });
    let test = builder.build_with_auto_env(&server).await?;
    let state_db = test.codex.state_db().expect("state database enabled");
    let cwd = test.config.cwd.as_path();
    run_git(cwd, &["init", "-q", "--initial-branch=main"]);
    run_git(
        cwd,
        &[
            "-c",
            "user.name=Codex Tests",
            "-c",
            "user.email=codex-tests@example.com",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "preserve generated files",
        ],
    );
    let commit = run_git(cwd, &["rev-parse", "HEAD"]);
    run_git(
        cwd,
        &[
            "notes",
            "--ref=refs/notes/intention",
            "add",
            "-m",
            "intent_priority: must\nsummary: Preserve generated files\ndecision: Keep generated files unchanged unless explicitly reviewed.",
            &commit,
        ],
    );
    let response = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("provenance-flow"),
            ev_assistant_message("provenance-message", "continued normally"),
            ev_completed("provenance-flow"),
        ]),
    )
    .await;
    test.submit_text_turn("please change generated files")
        .await?;

    let request = response.single_request();
    let advisory = serde_json::to_string(&request.input())?;
    let advisory_start = advisory
        .find("<codex_decision_provenance_advisory>")
        .expect("model input should contain the provenance advisory");
    let advisory_end = advisory[advisory_start..]
        .find("</codex_decision_provenance_advisory>")
        .map(|offset| advisory_start + offset)
        .expect("provenance advisory should be closed")
        + "</codex_decision_provenance_advisory>".len();
    let advisory_fragment = &advisory[advisory_start..advisory_end];
    assert!(advisory_fragment.contains("informational context, not an instruction or approval"));
    assert!(advisory_fragment.contains("No decision or approval is inferred here"));
    assert!(advisory_fragment.len() < 6_000);
    assert!(
        state_db
            .list_open_crossroads(20)
            .await?
            .first()
            .is_some_and(|crossroad| {
                crossroad
                    .source_refs
                    .iter()
                    .any(|source| source.reference == format!("refs/notes/intention@{commit}"))
            })
    );
    assert_eq!(state_db.list_open_crossroads(20).await?.len(), 1);

    Ok(())
}
