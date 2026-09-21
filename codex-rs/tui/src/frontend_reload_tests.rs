use super::*;
use clap::Parser;
use pretty_assertions::assert_eq;
use serial_test::serial;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn frontend_reload_command_preserves_context_and_client_arguments() {
    let context = FrontendReloadContext {
        thread_id: "019e72f4-e09a-70f2-b2c2-a153a57b8cc0".to_string(),
        account_alias: Some("work".to_string()),
        cwd: PathBuf::from("/workspace/current"),
        model_provider: Some("openai".to_string()),
        model: "gpt-6".to_string(),
        reasoning_effort: Some("high".to_string()),
        service_tier: Some("fast".to_string()),
        handoff_id: Some("handoff-1".to_string()),
        launcher: Some(PathBuf::from("/opt/codex-rick")),
    };
    let mut command = build_frontend_reload_command(
        Path::new("/opt/codex-rick"),
        &context,
        frontend_reload_args([
            std::ffi::OsString::from("codex"),
            std::ffi::OsString::from("--profile"),
            std::ffi::OsString::from("work"),
        ]),
    );

    assert_eq!(command.get_program(), Path::new("/opt/codex-rick"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [
            std::ffi::OsStr::new("--profile"),
            std::ffi::OsStr::new("work")
        ]
    );
    let env = command
        .get_envs()
        .filter_map(|(key, value)| value.map(|value| (key, value)))
        .collect::<Vec<_>>();
    assert!(env.contains(&(
        std::ffi::OsStr::new(FRONTEND_RELOAD_THREAD_ENV),
        std::ffi::OsStr::new("019e72f4-e09a-70f2-b2c2-a153a57b8cc0")
    )));
    assert!(env.contains(&(
        std::ffi::OsStr::new(FRONTEND_RELOAD_ACCOUNT_ENV),
        std::ffi::OsStr::new("work")
    )));
    assert!(env.contains(&(
        std::ffi::OsStr::new(FRONTEND_RELOAD_CWD_ENV),
        std::ffi::OsStr::new("/workspace/current")
    )));
    assert!(env.contains(&(
        std::ffi::OsStr::new(FRONTEND_RELOAD_PROVIDER_ENV),
        std::ffi::OsStr::new("openai")
    )));
    assert!(env.contains(&(
        std::ffi::OsStr::new(FRONTEND_RELOAD_MODEL_ENV),
        std::ffi::OsStr::new("gpt-6")
    )));
    assert!(env.contains(&(
        std::ffi::OsStr::new(FRONTEND_RELOAD_REASONING_ENV),
        std::ffi::OsStr::new("high")
    )));
    assert!(env.contains(&(
        std::ffi::OsStr::new(FRONTEND_RELOAD_SERVICE_TIER_ENV),
        std::ffi::OsStr::new("fast")
    )));
    assert!(env.contains(&(
        std::ffi::OsStr::new(FRONTEND_RELOAD_HANDOFF_ENV),
        std::ffi::OsStr::new("handoff-1")
    )));
    assert!(env.contains(&(
        std::ffi::OsStr::new(FRONTEND_RELOAD_LAUNCHER_ENV),
        std::ffi::OsStr::new("/opt/codex-rick")
    )));

    // Keep the command alive until all borrowed iterators have been consumed; this is a
    // fake-launcher assertion only and never starts a process.
    command.args(["--no-alt-screen"]);
}

#[test]
fn frontend_reload_recovery_cli_parses_and_selects_exact_thread() {
    let mut cli = Cli::try_parse_from([
        "codex",
        "--recover-handoff",
        "handoff-1",
        "--recover-handoff-thread",
        "019e72f4-e09a-70f2-b2c2-a153a57b8cc0",
        "--account",
        "work",
        "--cd",
        "/workspace/current",
        "--model",
        "gpt-6",
    ])
    .expect("recovery CLI should parse");

    apply_frontend_reload_cli_args(&mut cli).expect("recovery CLI should validate");
    assert_eq!(cli.frontend_reload_handoff_id.as_deref(), Some("handoff-1"));
    assert_eq!(
        cli.resume_session_id.as_deref(),
        Some("019e72f4-e09a-70f2-b2c2-a153a57b8cc0")
    );
    assert_eq!(cli.frontend_reload_thread_id, None);
    assert_eq!(cli.startup_account_alias.as_deref(), Some("work"));
    assert_eq!(cli.cwd, Some(PathBuf::from("/workspace/current")));
    assert_eq!(cli.model.as_deref(), Some("gpt-6"));
}

#[test]
fn frontend_reload_recovery_command_preserves_session_context() {
    let command = frontend_reload_recovery_command(
        Path::new("/opt/codex-rick"),
        "handoff-1",
        ThreadId::from_string("019e72f4-e09a-70f2-b2c2-a153a57b8cc0").expect("valid thread id"),
        Some("work"),
        Path::new("/workspace/current project"),
        Some("openai"),
        "gpt-6",
        Some("high"),
        Some("fast"),
    );

    assert_eq!(
        command,
        "/opt/codex-rick --recover-handoff handoff-1 --recover-handoff-thread 019e72f4-e09a-70f2-b2c2-a153a57b8cc0 --cd \"/workspace/current project\" --account work --model gpt-6 -c \"model_provider=\\\"openai\\\"\" -c \"model_reasoning_effort=\\\"high\\\"\" -c \"service_tier=\\\"fast\\\"\""
    );
}

#[test]
fn frontend_reload_context_drops_replayed_prompt_and_images() {
    let mut cli = Cli::try_parse_from([
        "codex",
        "old prompt",
        "--image",
        "/tmp/old.png",
        "--model",
        "old-model",
    ])
    .expect("test CLI should parse");
    cli.agents_overview = true;
    cli.fork_picker = true;
    cli.fork_last = true;
    cli.fork_session_id = Some("old-thread".to_string());
    cli.fork_show_all = true;
    apply_frontend_reload_context(
        &mut cli,
        FrontendReloadContext {
            thread_id: "019e72f4-e09a-70f2-b2c2-a153a57b8cc0".to_string(),
            account_alias: None,
            cwd: PathBuf::from("/workspace/current"),
            model_provider: None,
            model: "current-model".to_string(),
            reasoning_effort: Some("high".to_string()),
            service_tier: Some("fast".to_string()),
            handoff_id: None,
            launcher: None,
        },
    );

    assert_eq!(cli.prompt, None);
    assert!(cli.images.is_empty());
    assert_eq!(
        cli.resume_session_id.as_deref(),
        Some("019e72f4-e09a-70f2-b2c2-a153a57b8cc0")
    );
    assert!(!cli.agents_overview);
    assert!(!cli.fork_picker);
    assert!(!cli.fork_last);
    assert_eq!(cli.fork_session_id, None);
    assert!(!cli.fork_show_all);
    assert_eq!(cli.cwd.as_deref(), Some(Path::new("/workspace/current")));
    assert_eq!(cli.startup_account_alias, None);
    assert_eq!(cli.frontend_reload_handoff_id, None);
    assert_eq!(cli.frontend_launcher, None);
    assert_eq!(cli.model.as_deref(), Some("current-model"));
    assert!(
        cli.config_overrides
            .raw_overrides
            .iter()
            .any(|override_value| override_value == "model_reasoning_effort=\"high\"")
    );
    assert!(
        cli.config_overrides
            .raw_overrides
            .iter()
            .any(|override_value| override_value == "service_tier=\"fast\"")
    );
}

#[test]
fn frontend_reload_context_clears_new_worktree_and_oss_selection() {
    let mut cli =
        Cli::try_parse_from(["codex", "--worktree", "--oss", "--local-provider", "ollama"])
            .expect("test CLI should parse");
    apply_frontend_reload_context(
        &mut cli,
        FrontendReloadContext {
            thread_id: "019e72f4-e09a-70f2-b2c2-a153a57b8cc0".to_string(),
            account_alias: None,
            cwd: PathBuf::from("/workspace/current"),
            model_provider: Some("ollama".to_string()),
            model: "current-model".to_string(),
            reasoning_effort: None,
            service_tier: None,
            handoff_id: Some("handoff-1".to_string()),
            launcher: Some(PathBuf::from("/opt/codex-rick")),
        },
    );
    assert!(!cli.shared.worktree);
    assert!(!cli.shared.oss);
    assert_eq!(cli.shared.oss_provider, None);
}

#[test]
fn frontend_reload_context_preserves_explicit_default_account() {
    let mut cli =
        Cli::try_parse_from(["codex", "--account", "default"]).expect("test CLI should parse");
    apply_frontend_reload_context(
        &mut cli,
        FrontendReloadContext {
            thread_id: "019e72f4-e09a-70f2-b2c2-a153a57b8cc0".to_string(),
            account_alias: Some("default".to_string()),
            cwd: PathBuf::from("/workspace/current"),
            model_provider: Some("openai".to_string()),
            model: "current-model".to_string(),
            reasoning_effort: None,
            service_tier: None,
            handoff_id: Some("handoff-1".to_string()),
            launcher: Some(PathBuf::from("/opt/codex-rick")),
        },
    );
    assert_eq!(cli.startup_account_alias.as_deref(), Some("default"));
    assert!(
        cli.config_overrides
            .raw_overrides
            .iter()
            .any(|value| { value == "model_provider=\"openai\"" })
    );
}

#[test]
fn frontend_reload_context_escapes_toml_overrides() {
    let mut cli = Cli::try_parse_from(["codex"]).expect("test CLI should parse");
    apply_frontend_reload_context(
        &mut cli,
        FrontendReloadContext {
            thread_id: "019e72f4-e09a-70f2-b2c2-a153a57b8cc0".to_string(),
            account_alias: None,
            cwd: PathBuf::from("/workspace/current"),
            model_provider: None,
            model: "current-model".to_string(),
            reasoning_effort: Some("custom\"effort\nline".to_string()),
            service_tier: Some("tier\\value\nline".to_string()),
            handoff_id: None,
            launcher: None,
        },
    );

    assert!(cli.config_overrides.raw_overrides.iter().any(
        |override_value| override_value == "model_reasoning_effort=\"custom\\\"effort\\nline\""
    ));
    assert!(
        cli.config_overrides
            .raw_overrides
            .iter()
            .any(|override_value| override_value == "service_tier=\"tier\\\\value\\nline\"")
    );
}

#[test]
fn frontend_reload_launcher_requires_an_executable_file() {
    let temp_dir = tempfile::tempdir().expect("temporary launcher directory");
    let launcher = temp_dir.path().join("codex");
    std::fs::write(&launcher, b"#!/bin/sh\n").expect("write launcher");
    assert!(!launcher_is_executable(&launcher));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))
            .expect("make launcher executable");
        assert!(launcher_is_executable(&launcher));
    }
}

#[test]
#[serial]
fn frontend_reload_launcher_rejects_relative_configured_path() {
    let previous = std::env::var_os(FRONTEND_LAUNCHER_ENV);
    unsafe {
        std::env::set_var(FRONTEND_LAUNCHER_ENV, "./codex");
    }
    assert_eq!(resolve_frontend_launcher(), None);
    unsafe {
        match previous {
            Some(value) => std::env::set_var(FRONTEND_LAUNCHER_ENV, value),
            None => std::env::remove_var(FRONTEND_LAUNCHER_ENV),
        }
    }
}

#[test]
#[serial]
fn frontend_reload_marker_rejects_partial_handoff_without_clearing_retry_context() {
    let marker_names = [
        FRONTEND_RELOAD_THREAD_ENV,
        FRONTEND_RELOAD_ACCOUNT_ENV,
        FRONTEND_RELOAD_CWD_ENV,
        FRONTEND_RELOAD_PROVIDER_ENV,
        FRONTEND_RELOAD_MODEL_ENV,
        FRONTEND_RELOAD_REASONING_ENV,
        FRONTEND_RELOAD_SERVICE_TIER_ENV,
        FRONTEND_RELOAD_HANDOFF_ENV,
        FRONTEND_RELOAD_LAUNCHER_ENV,
    ];
    unsafe {
        for name in marker_names {
            std::env::remove_var(name);
        }
        std::env::set_var(
            FRONTEND_RELOAD_THREAD_ENV,
            "019e72f4-e09a-70f2-b2c2-a153a57b8cc0",
        );
        std::env::set_var(FRONTEND_RELOAD_ACCOUNT_ENV, "work");
        std::env::set_var(FRONTEND_RELOAD_CWD_ENV, "/workspace/current");
        std::env::set_var(FRONTEND_RELOAD_PROVIDER_ENV, "openai");
        std::env::set_var(FRONTEND_RELOAD_MODEL_ENV, "gpt-6");
        std::env::set_var(FRONTEND_RELOAD_HANDOFF_ENV, "handoff-1");
    }

    let result = take_frontend_reload_context();
    assert!(result.is_err());
    assert_eq!(
        std::env::var_os(FRONTEND_RELOAD_HANDOFF_ENV),
        Some(std::ffi::OsString::from("handoff-1"))
    );

    unsafe {
        for name in marker_names {
            std::env::remove_var(name);
        }
    }
}

#[test]
#[serial]
fn frontend_reload_marker_rejects_invalid_handoff_id_without_clearing_context() {
    let marker_names = [
        FRONTEND_RELOAD_THREAD_ENV,
        FRONTEND_RELOAD_ACCOUNT_ENV,
        FRONTEND_RELOAD_CWD_ENV,
        FRONTEND_RELOAD_PROVIDER_ENV,
        FRONTEND_RELOAD_MODEL_ENV,
        FRONTEND_RELOAD_REASONING_ENV,
        FRONTEND_RELOAD_SERVICE_TIER_ENV,
        FRONTEND_RELOAD_HANDOFF_ENV,
        FRONTEND_RELOAD_LAUNCHER_ENV,
    ];
    unsafe {
        for name in marker_names {
            std::env::remove_var(name);
        }
        std::env::set_var(
            FRONTEND_RELOAD_THREAD_ENV,
            "019e72f4-e09a-70f2-b2c2-a153a57b8cc0",
        );
        std::env::set_var(FRONTEND_RELOAD_ACCOUNT_ENV, "work");
        std::env::set_var(FRONTEND_RELOAD_CWD_ENV, "/workspace/current");
        std::env::set_var(FRONTEND_RELOAD_PROVIDER_ENV, "openai");
        std::env::set_var(FRONTEND_RELOAD_MODEL_ENV, "gpt-6");
        std::env::set_var(FRONTEND_RELOAD_HANDOFF_ENV, "../other");
        std::env::set_var(FRONTEND_RELOAD_LAUNCHER_ENV, "/opt/codex-rick");
    }

    let result = take_frontend_reload_context();
    assert!(result.is_err());
    assert_eq!(
        std::env::var_os(FRONTEND_RELOAD_HANDOFF_ENV),
        Some(std::ffi::OsString::from("../other"))
    );

    unsafe {
        for name in marker_names {
            std::env::remove_var(name);
        }
    }
}

#[test]
#[serial]
fn frontend_reload_marker_rejects_non_executable_launcher_without_clearing_context() {
    let marker_names = [
        FRONTEND_RELOAD_THREAD_ENV,
        FRONTEND_RELOAD_ACCOUNT_ENV,
        FRONTEND_RELOAD_CWD_ENV,
        FRONTEND_RELOAD_PROVIDER_ENV,
        FRONTEND_RELOAD_MODEL_ENV,
        FRONTEND_RELOAD_REASONING_ENV,
        FRONTEND_RELOAD_SERVICE_TIER_ENV,
        FRONTEND_RELOAD_HANDOFF_ENV,
        FRONTEND_RELOAD_LAUNCHER_ENV,
    ];
    let temp_dir = TempDir::new().expect("temporary launcher directory");
    let launcher = temp_dir.path().join("codex");
    std::fs::write(&launcher, b"#!/bin/sh\n").expect("write launcher");
    unsafe {
        for name in marker_names {
            std::env::remove_var(name);
        }
        std::env::set_var(
            FRONTEND_RELOAD_THREAD_ENV,
            "019e72f4-e09a-70f2-b2c2-a153a57b8cc0",
        );
        std::env::set_var(FRONTEND_RELOAD_ACCOUNT_ENV, "work");
        std::env::set_var(FRONTEND_RELOAD_CWD_ENV, "/workspace/current");
        std::env::set_var(FRONTEND_RELOAD_PROVIDER_ENV, "openai");
        std::env::set_var(FRONTEND_RELOAD_MODEL_ENV, "gpt-6");
        std::env::set_var(FRONTEND_RELOAD_HANDOFF_ENV, "handoff-1");
        std::env::set_var(FRONTEND_RELOAD_LAUNCHER_ENV, &launcher);
    }

    let result = take_frontend_reload_context();
    assert!(result.is_err());
    assert_eq!(
        std::env::var_os(FRONTEND_RELOAD_LAUNCHER_ENV),
        Some(launcher.into_os_string())
    );

    unsafe {
        for name in marker_names {
            std::env::remove_var(name);
        }
    }
}
