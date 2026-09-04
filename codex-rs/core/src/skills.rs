use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::sync::Arc;

use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_analytics::InvocationType;
use codex_analytics::SkillInvocation;
use codex_analytics::SkillInvocationLocation;
use codex_analytics::TrackEventsContext;
use codex_analytics::build_track_events_context;
use codex_extension_api::SkillInvocationInput;
use codex_extension_api::SkillInvocationKind;
use codex_otel::sanitize_metric_tag_value;
use codex_protocol::config_types::ModeKind;
use codex_protocol::protocol::SkillScope;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_skills_extension::InjectedHostSkillPrompts;
use codex_skills_extension::detect_implicit_skill_invocation;
use codex_skills_extension::record_plugin_turn_usage;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_utils_plugins::PluginSkillRoot;
use tokio::sync::Mutex;
use tracing::warn;

pub use codex_core_skills::SkillDependencyInfo;
pub use codex_skills::SkillMetadata;
pub use codex_skills_extension::HostSkillsLoadInput;

#[derive(Debug, Default)]
struct ImplicitSkillInvocations(Mutex<HashSet<String>>);

pub(crate) fn skill_allowed_in_mode(
    config: &Config,
    mode: ModeKind,
    skill: &SkillMetadata,
) -> bool {
    if let Some(filter) = crate::enablement::skill_enablement_filter(config, mode) {
        if filter.items.is_empty() {
            return true;
        }
        let path = skill.path_to_skills_md.as_path().to_string_lossy();
        let matches = filter.items.iter().any(|selector| {
            let selector = selector.trim();
            !selector.is_empty()
                && (skill.name == selector || path == selector || path.ends_with(selector))
        });

        return match filter.mode {
            codex_config::EnablementFilterMode::Include => matches,
            codex_config::EnablementFilterMode::Exclude => !matches,
        };
    }

    let Some(filter) = config.skills.modes.get(&mode) else {
        return true;
    };
    if filter.skills.is_empty() {
        return true;
    }

    let path = skill.path_to_skills_md.as_path().to_string_lossy();
    let matches = filter.skills.iter().any(|selector| {
        let selector = selector.trim();
        !selector.is_empty()
            && (skill.name == selector || path == selector || path.ends_with(selector))
    });

    match filter.mode {
        codex_config::types::SkillModeFilterMode::Include => matches,
        codex_config::types::SkillModeFilterMode::Exclude => !matches,
    }
}

pub(crate) fn filter_skills_for_mode<'a>(
    config: &Config,
    mode: ModeKind,
    skills: &'a [SkillMetadata],
) -> Vec<&'a SkillMetadata> {
    skills
        .iter()
        .filter(|skill| skill_allowed_in_mode(config, mode, skill))
        .collect()
}

pub(crate) fn skills_load_input_from_config(
    config: &Config,
    effective_skill_roots: Vec<PluginSkillRoot>,
) -> HostSkillsLoadInput {
    HostSkillsLoadInput::new(
        config.cwd.clone(),
        effective_skill_roots,
        config.config_layer_stack.clone(),
    )
}

pub(crate) async fn resolve_skill_dependencies_for_turn(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    dependencies: &[SkillDependencyInfo],
) {
    if dependencies.is_empty() {
        return;
    }

    let existing_env = sess.dependency_env().await;
    let mut loaded_values = HashMap::new();
    let mut missing = Vec::new();
    let mut seen_names = HashSet::new();

    for dependency in dependencies {
        let name = dependency.name.clone();
        if !seen_names.insert(name.clone()) || existing_env.contains_key(&name) {
            continue;
        }
        match env::var(&name) {
            Ok(value) => {
                loaded_values.insert(name.clone(), value);
            }
            Err(env::VarError::NotPresent) => {
                missing.push(dependency.clone());
            }
            Err(err) => {
                warn!("failed to read env var {name}: {err}");
                missing.push(dependency.clone());
            }
        }
    }

    if !loaded_values.is_empty() {
        sess.set_dependency_env(loaded_values).await;
    }

    if !missing.is_empty() {
        request_skill_dependencies(sess, turn_context, &missing).await;
    }
}

async fn request_skill_dependencies(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    dependencies: &[SkillDependencyInfo],
) {
    let questions = dependencies
        .iter()
        .map(|dependency| {
            let requirement = dependency.description.as_ref().map_or_else(
                || {
                    format!(
                        "The skill \"{}\" requires \"{}\" to be set.",
                        dependency.skill_name, dependency.name
                    )
                },
                |description| {
                    format!(
                        "The skill \"{}\" requires \"{}\" to be set ({}).",
                        dependency.skill_name, dependency.name, description
                    )
                },
            );
            RequestUserInputQuestion {
                id: dependency.name.clone(),
                header: "Skill requires environment variable".to_string(),
                question: format!(
                    "{requirement} This is an experimental internal feature. The value is stored in memory for this session only."
                ),
                is_other: false,
                is_secret: true,
                options: None,
            }
        })
        .collect::<Vec<_>>();
    if questions.is_empty() {
        return;
    }

    let response = sess
        .request_user_input(
            turn_context,
            format!("skill-deps-{}", turn_context.sub_id),
            RequestUserInputArgs {
                questions,
                auto_resolution_ms: None,
                is_blocking: turn_context.mode == ModeKind::Plan,
            },
        )
        .await
        .unwrap_or_else(|| RequestUserInputResponse {
            answers: HashMap::new(),
        });
    if response.answers.is_empty() {
        return;
    }

    let mut values = HashMap::new();
    for (name, answer) in response.answers {
        let mut user_note = None;
        for entry in &answer.answers {
            if let Some(note) = entry.strip_prefix("user_note: ")
                && !note.trim().is_empty()
            {
                user_note = Some(note.trim().to_string());
            }
        }
        if let Some(value) = user_note {
            values.insert(name, value);
        }
    }
    if values.is_empty() {
        return;
    }

    sess.set_dependency_env(values).await;
}

pub(crate) async fn emit_explicit_skill_invocations(
    sess: &Session,
    turn_context: &TurnContext,
    mentioned_skills: &[SkillMetadata],
    injected_skills: &[SkillMetadata],
    tracking: TrackEventsContext,
) {
    let injected_skill_paths = injected_skills
        .iter()
        .map(|skill| &skill.path_to_skills_md)
        .collect::<HashSet<_>>();
    let model_slug_tag = sanitize_metric_tag_value(turn_context.model_info().slug.as_str());
    let reasoning_effort = turn_context.effective_reasoning_effort_for_tracing();
    for skill in mentioned_skills {
        let skill_name_tag = sanitize_metric_tag_value(skill.name.as_str());
        let plugin_id_tag =
            sanitize_metric_tag_value(skill.plugin_id.as_deref().unwrap_or("unattributed"));
        let status = if injected_skill_paths.contains(&skill.path_to_skills_md) {
            record_plugin_turn_usage(
                turn_context.extension_data.as_ref(),
                skill.plugin_id.as_deref(),
            );
            "ok"
        } else {
            "error"
        };
        turn_context.session_telemetry.counter(
            "codex.skill.injected",
            /*inc*/ 1,
            &[
                ("status", status),
                ("skill", skill_name_tag.as_str()),
                ("invoke_type", "explicit"),
                ("plugin_id", plugin_id_tag.as_str()),
                ("model_slug", model_slug_tag.as_str()),
                ("reasoning_effort", reasoning_effort.as_str()),
            ],
        );
    }

    let injected_host_skill_prompts = turn_context
        .extension_data
        .get::<InjectedHostSkillPrompts>();
    for skill in injected_skills {
        let skill_resource = skill.path_to_skills_md.to_string_lossy();
        if injected_host_skill_prompts
            .as_ref()
            .is_some_and(|prompts| prompts.is_superseded_path(&skill_resource))
        {
            continue;
        }
        for contributor in sess.services.extensions.skill_invocation_contributors() {
            contributor
                .on_skill_invocation(SkillInvocationInput {
                    session_store: &sess.services.session_extension_data,
                    thread_store: &sess.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                    turn_id: turn_context.sub_id.as_str(),
                    skill_resource: skill_resource.as_ref(),
                    kind: SkillInvocationKind::Explicit,
                })
                .await;
        }
    }

    let invocations = injected_skills
        .iter()
        .map(|skill| SkillInvocation {
            skill_name: skill.name.clone(),
            location: SkillInvocationLocation::Host {
                path: skill.path_to_skills_md.to_path_buf(),
                scope: skill.scope,
            },
            plugin_id: skill.plugin_id.clone(),
            remote_plugin_id: skill.remote_plugin_id.clone(),
            invocation_type: InvocationType::Explicit,
        })
        .collect();
    sess.services
        .analytics_events_client
        .track_skill_invocations(tracking, invocations);
}

pub(crate) async fn maybe_emit_implicit_skill_invocation(
    sess: &Session,
    turn_context: &TurnContext,
    command: &str,
    workdir: &PathUri,
    native_workdir: Option<&AbsolutePathBuf>,
    environment_id: &str,
) {
    let Some(invocation) = detect_implicit_skill_invocation(
        turn_context.extension_data.as_ref(),
        environment_id,
        command,
        workdir,
        native_workdir,
    ) else {
        return;
    };
    let skill_name = invocation.skill_name.clone();
    let (skill_resource, seen_key) = match &invocation.location {
        SkillInvocationLocation::Host { path, scope } => {
            let skill_scope = match scope {
                SkillScope::User => "user",
                SkillScope::Repo => "repo",
                SkillScope::System => "system",
                SkillScope::Admin => "admin",
            };
            let skill_path = path.to_string_lossy().into_owned();
            let seen_key = format!("{skill_scope}:{skill_path}:{skill_name}");
            (skill_path, seen_key)
        }
        SkillInvocationLocation::Resource { id, .. } => (id.clone(), format!("resource:{id}")),
    };
    let inserted = {
        let skill_invocations = turn_context
            .extension_data
            .get_or_init(ImplicitSkillInvocations::default);
        let mut seen_skills = skill_invocations.0.lock().await;
        seen_skills.insert(seen_key)
    };
    if !inserted {
        return;
    }
    let skill_name_tag = sanitize_metric_tag_value(skill_name.as_str());
    let plugin_id_tag =
        sanitize_metric_tag_value(invocation.plugin_id.as_deref().unwrap_or("unattributed"));
    let model_slug_tag = sanitize_metric_tag_value(turn_context.model_info().slug.as_str());
    let reasoning_effort = turn_context.effective_reasoning_effort_for_tracing();
    record_plugin_turn_usage(
        turn_context.extension_data.as_ref(),
        invocation.plugin_id.as_deref(),
    );

    for contributor in sess.services.extensions.skill_invocation_contributors() {
        contributor
            .on_skill_invocation(SkillInvocationInput {
                session_store: &sess.services.session_extension_data,
                thread_store: &sess.services.thread_extension_data,
                turn_store: turn_context.extension_data.as_ref(),
                turn_id: turn_context.sub_id.as_str(),
                skill_resource: skill_resource.as_str(),
                kind: SkillInvocationKind::Implicit,
            })
            .await;
    }

    turn_context.session_telemetry.counter(
        "codex.skill.injected",
        /*inc*/ 1,
        &[
            ("status", "ok"),
            ("skill", skill_name_tag.as_str()),
            ("invoke_type", "implicit"),
            ("plugin_id", plugin_id_tag.as_str()),
            ("model_slug", model_slug_tag.as_str()),
            ("reasoning_effort", reasoning_effort.as_str()),
        ],
    );
    sess.services
        .analytics_events_client
        .track_skill_invocations(
            build_track_events_context(
                turn_context.model_info().slug.clone(),
                sess.thread_id.to_string(),
                turn_context.sub_id.clone(),
                turn_context.originator.clone(),
            ),
            vec![invocation],
        );
}
