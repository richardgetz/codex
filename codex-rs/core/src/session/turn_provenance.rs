//! Request-start decision provenance preflight.

use super::git_intent_preflight::find_git_intent_candidates;
use crate::context::DecisionProvenanceAdvisory;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_git_utils::get_git_repo_root;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use codex_state::decision_provenance::Actor;
use codex_state::decision_provenance::Crossroad;
use codex_state::decision_provenance::CrossroadOption;
use codex_state::decision_provenance::CrossroadStatus;
use codex_state::decision_provenance::NotificationCategory;
use codex_state::decision_provenance::PreferenceKind;
use codex_state::decision_provenance::PreferenceStrength;
use codex_state::decision_provenance::PrivacyClass;
use codex_state::decision_provenance::ProvenanceNotification;
use codex_state::decision_provenance::ProvenanceRelationship;
use codex_state::decision_provenance::ProvenanceWriteOptions;
use codex_state::decision_provenance::RelationshipEvidence;
use codex_state::decision_provenance::RelationshipKind;
use codex_state::decision_provenance::Scope;
use codex_state::decision_provenance::ScopeRef;
use codex_state::decision_provenance::SourceReference;
use codex_state::decision_provenance::Timestamps;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::trace;
use tracing::warn;
use uuid::Uuid;

#[path = "turn_provenance_fingerprint.rs"]
mod turn_provenance_fingerprint;
use turn_provenance_fingerprint::request_fingerprint;

pub(super) struct ProvenancePreflightOutcome {
    pub(super) advisory: Option<DecisionProvenanceAdvisory>,
}

impl ProvenancePreflightOutcome {
    fn continue_without_advisory() -> Self {
        Self { advisory: None }
    }
}

/// Records a bounded provenance observation and always allows normal model flow to continue.
pub(super) async fn record_turn_provenance_preflight(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    user_input: &[UserInput],
) -> ProvenancePreflightOutcome {
    if !turn_context.config.decision_provenance.enabled {
        return ProvenancePreflightOutcome::continue_without_advisory();
    }
    let Some(state_db) = sess.state_db() else {
        return ProvenancePreflightOutcome::continue_without_advisory();
    };
    let session_id = sess.session_id().to_string();
    let task_id = sess.thread_id().to_string();
    let task_scope = ScopeRef::new(Scope::Task, task_id.clone());
    let mut scopes = vec![ScopeRef::global(), task_scope.clone()];
    let mut repository_path = None;
    let mut project_ref = None;
    if let Some(cwd) = turn_context.environments.local_environment_cwd() {
        let git_repository_path = get_git_repo_root(cwd.as_path());
        let project_path = git_repository_path
            .clone()
            .unwrap_or_else(|| cwd.as_path().to_path_buf());
        let project_id = project_path.display().to_string();
        let repository = git_repository_path
            .clone()
            .map(|path| path.display().to_string());
        repository_path = git_repository_path;
        project_ref = Some(project_id.clone());
        for scope in [
            repository
                .as_deref()
                .map(|id| ScopeRef::new(Scope::Repo, id)),
            Some(ScopeRef::new(Scope::Project, project_id)),
        ]
        .into_iter()
        .flatten()
        {
            if !scopes.iter().any(|existing| existing == &scope) {
                scopes.push(scope);
            }
        }
    }

    let preflight = match state_db
        .preflight_preference_boundaries_for_scopes(task_scope, &scopes)
        .await
    {
        Ok(preflight) => preflight,
        Err(err) => {
            warn!("decision provenance observation unavailable: {err:#}");
            return ProvenancePreflightOutcome::continue_without_advisory();
        }
    };
    trace!(
        active_boundaries = preflight.active.len(),
        candidate_preferences = preflight.candidates.len(),
        "loaded decision provenance preference preflight"
    );
    let request_text = user_input_text_for_provenance(user_input);
    if request_text.is_empty() {
        return ProvenancePreflightOutcome::continue_without_advisory();
    }
    let request_tokens = provenance_tokens(&request_text);
    let boundaries = preflight
        .active
        .iter()
        .filter(|boundary| boundary_intersects_request(boundary, &request_tokens, &request_text))
        .collect::<Vec<_>>();
    let git_intent_candidates = if turn_context.config.decision_provenance.git_intent_bridge {
        if let Some(repository_path) = repository_path.as_deref() {
            find_git_intent_candidates(repository_path, &request_text, &request_tokens).await
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    if boundaries.is_empty() && git_intent_candidates.is_empty() {
        return ProvenancePreflightOutcome::continue_without_advisory();
    }

    let mut boundary_ids = boundaries
        .iter()
        .map(|boundary| boundary.id.clone())
        .collect::<Vec<_>>();
    boundary_ids.sort();
    let mut git_intent_commit_ids = git_intent_candidates
        .iter()
        .map(|candidate| candidate.commit.clone())
        .collect::<Vec<_>>();
    git_intent_commit_ids.sort();
    let request_key = request_fingerprint(user_input);
    let issue_key = provenance_issue_key(
        &task_id,
        &boundary_ids,
        &git_intent_commit_ids,
        &request_key,
    );
    let crossroad_id =
        stable_provenance_id("crossroad", &format!("preflight:{task_id}:{issue_key}"));
    let new_source_refs = {
        let mut refs = vec![
            SourceReference::new("session", session_id.clone()),
            SourceReference::new("turn", turn_context.sub_id.clone()),
        ];
        refs.extend(
            git_intent_candidates
                .iter()
                .map(|candidate| candidate.source_ref.clone()),
        );
        refs
    };
    let existing_crossroad = match state_db.get_crossroad(&crossroad_id).await {
        Ok(crossroad) => crossroad,
        Err(err) => {
            warn!("failed to inspect existing decision provenance crossroad: {err:#}");
            None
        }
    };
    if let Some(crossroad) = &existing_crossroad
        && !matches!(
            crossroad.status,
            CrossroadStatus::Open | CrossroadStatus::Reopened
        )
    {
        return ProvenancePreflightOutcome::continue_without_advisory();
    }
    let source_refs = existing_crossroad
        .as_ref()
        .map(|crossroad| crossroad.source_refs.clone())
        .unwrap_or(new_source_refs);
    let privacy = if boundaries
        .iter()
        .any(|boundary| boundary.privacy == PrivacyClass::Sensitive)
    {
        PrivacyClass::Sensitive
    } else {
        PrivacyClass::Private
    };
    let topic = request_topic(&request_tokens);
    let prior_guidance = match (boundaries.is_empty(), git_intent_candidates.is_empty()) {
        (false, true) => "prior recorded preference guidance",
        (true, false) => "prior recorded Git intent",
        (false, false) => "prior recorded preference guidance and Git intent",
        (true, true) => unreachable!("provenance crossroad requires a candidate"),
    };
    let crossroad = existing_crossroad.clone().unwrap_or_else(|| Crossroad {
        id: crossroad_id.clone(),
        request_ref: Some(format!("provenance-request:{request_key}")),
        task_ref: Some(task_id.clone()),
        project_ref: project_ref.clone(),
        session_id: Some(session_id.clone()),
        question: format!("Review {prior_guidance} before changing {topic}."),
        options: vec![
            CrossroadOption {
                id: "retain".to_string(),
                label: "Retain the earlier direction".to_string(),
                summary: Some("Continue with the recorded guidance.".to_string()),
                tradeoffs: vec![
                    "Preserves the earlier assumptions until a replacement is discussed."
                        .to_string(),
                ],
            },
            CrossroadOption {
                id: "new-direction".to_string(),
                label: "Discuss a new direction".to_string(),
                summary: Some(
                    "Record any replacement with its actual actor and source.".to_string(),
                ),
                tradeoffs: vec![
                    "May fit new evidence better while changing established assumptions."
                        .to_string(),
                ],
            },
        ],
        recommended_option: None,
        affected_boundary_ids: boundary_ids.clone(),
        constraint_ids: boundaries
            .iter()
            .filter(|boundary| boundary.kind == PreferenceKind::HardConstraint)
            .map(|boundary| boundary.id.clone())
            .collect(),
        expected_tradeoffs: vec![
            "This is a retrieval candidate, not a confirmed conflict or approval.".to_string(),
            "No decision is recorded by this observation.".to_string(),
        ],
        authority_required: None,
        status: CrossroadStatus::Open,
        actor: Actor::System,
        source_refs: source_refs.clone(),
        linked_scratchpad_wait_id: None,
        timestamps: Timestamps::now(),
        privacy,
    });
    let provenance_at = crossroad.timestamps.created_at;

    if existing_crossroad.is_none()
        && let Err(err) = state_db
            .record_crossroad(
                crossroad.clone(),
                ProvenanceWriteOptions {
                    idempotency_key: Some(format!("preflight-crossroad:{task_id}:{issue_key}")),
                    actor: Actor::System,
                    occurred_at: provenance_at,
                },
            )
            .await
    {
        warn!("failed to record decision provenance crossroad: {err:#}");
        return ProvenancePreflightOutcome::continue_without_advisory();
    }

    for boundary in &boundaries {
        if let Err(err) = state_db
            .record_relationship(
                ProvenanceRelationship {
                    id: stable_provenance_id(
                        "relationship",
                        &format!("{crossroad_id}:considered:boundary:{}", boundary.id),
                    ),
                    from_type: codex_state::decision_provenance::EntityType::Crossroad,
                    from_id: crossroad_id.clone(),
                    relation: RelationshipKind::ConsideredNotDecisive,
                    to_type: codex_state::decision_provenance::EntityType::PreferenceBoundary,
                    to_id: boundary.id.clone(),
                    evidence: RelationshipEvidence::Considered,
                    summary: Some(
                        "request terms retrieved this prior boundary as potentially relevant"
                            .to_string(),
                    ),
                    source_refs: vec![boundary.source.clone()],
                    created_at: provenance_at,
                    privacy,
                },
                ProvenanceWriteOptions {
                    idempotency_key: Some(format!(
                        "preflight-crossroad-boundary:{crossroad_id}:{}",
                        boundary.id
                    )),
                    actor: Actor::System,
                    occurred_at: provenance_at,
                },
            )
            .await
        {
            warn!("failed to link decision provenance boundary candidate: {err:#}");
        }
    }
    for candidate in &git_intent_candidates {
        if let Err(err) = state_db
            .record_relationship(
                ProvenanceRelationship {
                    id: stable_provenance_id(
                        "relationship",
                        &format!("{crossroad_id}:considered:git:{}", candidate.commit),
                    ),
                    from_type: codex_state::decision_provenance::EntityType::Crossroad,
                    from_id: crossroad_id.clone(),
                    relation: RelationshipKind::ConsideredNotDecisive,
                    to_type: codex_state::decision_provenance::EntityType::Commit,
                    to_id: candidate.commit.clone(),
                    evidence: RelationshipEvidence::Considered,
                    summary: Some(
                        "request terms retrieved this commit's Git intent as potentially relevant"
                            .to_string(),
                    ),
                    source_refs: vec![candidate.source_ref.clone()],
                    created_at: provenance_at,
                    privacy,
                },
                ProvenanceWriteOptions {
                    idempotency_key: Some(format!(
                        "preflight-crossroad-git-intent:{crossroad_id}:{}",
                        candidate.commit
                    )),
                    actor: Actor::System,
                    occurred_at: provenance_at,
                },
            )
            .await
        {
            warn!("failed to link decision provenance Git intent candidate: {err:#}");
        }
    }

    let notification = ProvenanceNotification {
        id: stable_provenance_id("notification", &format!("preflight:{task_id}:{issue_key}")),
        category: NotificationCategory::ReviewRecommended,
        message: format!(
            "Prior recorded guidance may be relevant to this request. This is informational; the turn continues. Review crossroad {crossroad_id} with /decisions show {crossroad_id}."
        ),
        preference_boundary_id: boundary_ids.first().cloned(),
        crossroad_id: Some(crossroad_id.clone()),
        decision_id: None,
        authority_required: None,
        choice: None,
        will_record: false,
        created_at: provenance_at,
        source_refs: source_refs.clone(),
        privacy,
    };
    let notification_inserted = match state_db
        .record_notification(
            notification.clone(),
            ProvenanceWriteOptions {
                idempotency_key: Some(format!("preflight-notification:{task_id}:{issue_key}")),
                actor: Actor::System,
                occurred_at: provenance_at,
            },
        )
        .await
    {
        Ok(result) => result.inserted,
        Err(err) => {
            warn!("failed to record decision provenance notification: {err:#}");
            false
        }
    };
    if notification_inserted {
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: notification.message,
            }),
        )
        .await;
    }

    ProvenancePreflightOutcome {
        advisory: Some(DecisionProvenanceAdvisory::new(&crossroad)),
    }
}

fn user_input_text_for_provenance(user_input: &[UserInput]) -> String {
    let mut text = String::new();
    for input in user_input {
        let UserInput::Text { text: value, .. } = input else {
            continue;
        };
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(value);
        if text.len() >= 4_096 {
            let boundary = text
                .char_indices()
                .map(|(index, character)| (index, index + character.len_utf8()))
                .take_while(|(index, _)| *index < 4_096)
                .find_map(|(index, next_index)| (next_index > 4_096).then_some(index))
                .unwrap_or(4_096);
            text.truncate(boundary);
            break;
        }
    }
    text.trim().to_string()
}

fn provenance_tokens(text: &str) -> HashSet<String> {
    const STOP_WORDS: [&str; 21] = [
        "and",
        "ask",
        "before",
        "confirm",
        "confirmation",
        "do",
        "for",
        "if",
        "must",
        "never",
        "not",
        "only",
        "pause",
        "please",
        "preference",
        "should",
        "the",
        "to",
        "unless",
        "when",
        "with",
    ];
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .filter_map(|token| {
            let mut token = token.to_ascii_lowercase();
            if token.len() < 3 || STOP_WORDS.contains(&token.as_str()) {
                return None;
            }
            if token.ends_with('s') && token.len() > 3 {
                token.pop();
            }
            Some(token)
        })
        .take(64)
        .collect()
}

fn boundary_intersects_request(
    boundary: &codex_state::decision_provenance::PreferenceBoundary,
    request_tokens: &HashSet<String>,
    request_text: &str,
) -> bool {
    if boundary.is_candidate()
        || !boundary.lifecycle_status.is_active()
        || (!matches!(
            boundary.strength,
            PreferenceStrength::Hard | PreferenceStrength::Confirmation
        ) && boundary.kind != PreferenceKind::HardConstraint)
    {
        return false;
    }
    let boundary_text = format!(
        "{} {}",
        boundary.statement,
        boundary.rationale.as_deref().unwrap_or_default()
    );
    let boundary_tokens = provenance_tokens(&boundary_text);
    let shared_tokens = boundary_tokens.intersection(request_tokens).count();
    if shared_tokens == 0 {
        return false;
    }
    if request_honors_boundary(&boundary_text, request_text) {
        return false;
    }
    shared_tokens >= 2 || (boundary_tokens.len() <= 2 && shared_tokens == 1)
}

fn request_honors_boundary(boundary_text: &str, request_text: &str) -> bool {
    let boundary_text = boundary_text.to_ascii_lowercase();
    let request_text = request_text.to_ascii_lowercase();
    let prohibition_markers = ["never ", "do not ", "don't ", "must not ", "avoid "];
    let boundary_is_prohibition = prohibition_markers
        .iter()
        .any(|marker| boundary_text.contains(marker));
    let request_repeats_prohibition = prohibition_markers
        .iter()
        .any(|marker| request_text.contains(marker));
    if boundary_is_prohibition && request_repeats_prohibition {
        return true;
    }
    let has_pause_word = |text: &str| {
        text.split(|character: char| !character.is_ascii_alphanumeric())
            .any(|word| {
                matches!(
                    word,
                    "ask" | "confirm" | "confirmation" | "approval" | "pause" | "warn"
                )
            })
    };
    has_pause_word(&boundary_text) && has_pause_word(&request_text)
}

fn request_topic(request_tokens: &HashSet<String>) -> String {
    let mut tokens = request_tokens.iter().cloned().collect::<Vec<_>>();
    tokens.sort();
    let topic = tokens.into_iter().take(8).collect::<Vec<_>>().join(" ");
    if topic.is_empty() {
        "this area".to_string()
    } else {
        topic
    }
}

fn provenance_issue_key(
    task_id: &str,
    boundary_ids: &[String],
    git_intent_commit_ids: &[String],
    request_key: &str,
) -> String {
    let mut key = String::new();
    append_issue_key_frame(&mut key, "task", task_id);
    append_issue_key_list(&mut key, "boundary", boundary_ids);
    append_issue_key_list(&mut key, "git", git_intent_commit_ids);
    append_issue_key_frame(&mut key, "request", request_key);
    key
}

fn append_issue_key_list(key: &mut String, kind: &str, values: &[String]) {
    append_issue_key_frame(key, &format!("{kind}-count"), &values.len().to_string());
    for value in values {
        append_issue_key_frame(key, kind, value);
    }
}

fn append_issue_key_frame(key: &mut String, kind: &str, value: &str) {
    key.push_str(kind);
    key.push('[');
    key.push_str(&value.len().to_string());
    key.push_str("]:");
    key.push_str(value);
    key.push(';');
}

fn stable_provenance_id(prefix: &str, key: &str) -> String {
    let name = format!("{prefix}:{key}");
    format!(
        "{prefix}_{}",
        Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes()).simple()
    )
}

#[cfg(test)]
#[path = "turn_provenance_tests.rs"]
mod tests;
