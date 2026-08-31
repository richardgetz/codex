//! Request-start decision provenance preflight.

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_git_utils::get_git_repo_root;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use codex_state::decision_provenance::Actor;
use codex_state::decision_provenance::ApprovalState;
use codex_state::decision_provenance::Authority;
use codex_state::decision_provenance::Crossroad;
use codex_state::decision_provenance::CrossroadOption;
use codex_state::decision_provenance::CrossroadStatus;
use codex_state::decision_provenance::Decision;
use codex_state::decision_provenance::DecisionStatus;
use codex_state::decision_provenance::NotificationCategory;
use codex_state::decision_provenance::PreferenceBoundary;
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
use codex_state::decision_provenance::Warrant;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::trace;
use tracing::warn;
use uuid::Uuid;

pub(super) enum ProvenancePreflightOutcome {
    Continue,
    Blocked,
}

pub(super) async fn record_turn_provenance_preflight(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    user_input: &[UserInput],
) -> ProvenancePreflightOutcome {
    if !turn_context.config.decision_provenance.enabled {
        return ProvenancePreflightOutcome::Continue;
    }
    let Some(state_db) = sess.state_db() else {
        return ProvenancePreflightOutcome::Continue;
    };
    let session_id = sess.session_id().to_string();
    let task_id = sess.thread_id().to_string();
    let task_scope = ScopeRef::new(Scope::Task, task_id.clone());
    let mut scopes = vec![ScopeRef::global(), task_scope.clone()];
    let mut repository = None;
    let mut project_ref = None;
    if let Some(cwd) = turn_context.environments.local_environment_cwd() {
        let repository_path = get_git_repo_root(cwd.as_path());
        let project_path = repository_path
            .clone()
            .unwrap_or_else(|| cwd.as_path().to_path_buf());
        let project_id = project_path.display().to_string();
        repository = repository_path.map(|path| path.display().to_string());
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
            warn!("decision provenance preference preflight unavailable: {err:#}");
            return ProvenancePreflightOutcome::Continue;
        }
    };
    trace!(
        active_boundaries = preflight.active.len(),
        candidate_preferences = preflight.candidates.len(),
        "loaded decision provenance preference preflight"
    );
    let request_text = user_input_text_for_provenance(user_input);
    if request_text.is_empty() {
        return ProvenancePreflightOutcome::Continue;
    }
    let request_tokens = provenance_tokens(&request_text);
    let conflicts = preflight
        .active
        .iter()
        .filter(|boundary| boundary_intersects_request(boundary, &request_tokens, &request_text))
        .collect::<Vec<_>>();
    if conflicts.is_empty() {
        return ProvenancePreflightOutcome::Continue;
    }

    let mut boundary_ids = conflicts
        .iter()
        .map(|boundary| boundary.id.clone())
        .collect::<Vec<_>>();
    boundary_ids.sort();
    let boundary_key = boundary_ids.join(",");
    let crossroad_id = stable_provenance_id(
        "crossroad",
        &format!("preflight:{task_id}:{}:{boundary_key}", turn_context.sub_id),
    );
    let request_ref = format!("turn:{}", turn_context.sub_id);
    let linked_scratchpad_wait_id =
        crate::session::active_thread_scratchpad(&turn_context.config.codex_home, sess.thread_id)
            .and_then(|scratchpad| {
                scratchpad
                    .get("pending_waits")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|waits| {
                        waits.iter().find_map(|wait| {
                            wait.get("wait_id")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                        })
                    })
            });
    let source_refs = vec![
        SourceReference::new("session", session_id.clone()),
        SourceReference::new("turn", turn_context.sub_id.clone()),
    ];
    let existing_crossroad = match state_db.get_crossroad(&crossroad_id).await {
        Ok(crossroad) => crossroad,
        Err(err) => {
            warn!("failed to inspect existing decision provenance crossroad: {err:#}");
            return ProvenancePreflightOutcome::Blocked;
        }
    };
    let privacy = if conflicts
        .iter()
        .any(|boundary| boundary.privacy == PrivacyClass::Sensitive)
    {
        PrivacyClass::Sensitive
    } else {
        PrivacyClass::Private
    };
    let highest_authority = conflicts
        .iter()
        .map(|boundary| boundary.authority)
        .max_by_key(|authority| authority.precedence())
        .unwrap_or(Authority::User);
    let explicit_override = request_has_explicit_override(&request_text);
    let can_override = conflicts
        .iter()
        .all(|boundary| boundary.authority.precedence() <= Authority::User.precedence());
    let category = if !can_override {
        NotificationCategory::Blocked
    } else if explicit_override {
        NotificationCategory::PreferenceBoundaryCrossed
    } else {
        NotificationCategory::ApprovalRequired
    };
    let crossroad = existing_crossroad.clone().unwrap_or_else(|| Crossroad {
        id: crossroad_id.clone(),
        request_ref: Some(request_ref.clone()),
        task_ref: Some(task_id.clone()),
        project_ref: project_ref.clone(),
        session_id: Some(session_id.clone()),
        question: "Does this request honor the prior boundary, or is an explicit scoped override intended?"
            .to_string(),
        options: vec![
            CrossroadOption {
                id: "honor".to_string(),
                label: "Honor the prior boundary and pause".to_string(),
                summary: Some("Keep the earlier boundary active for this request.".to_string()),
                tradeoffs: vec!["May require clarification before proceeding.".to_string()],
            },
            CrossroadOption {
                id: "proceed".to_string(),
                label: "Proceed with an explicit current-user override".to_string(),
                summary: Some("Apply a new decision only within the current request scope.".to_string()),
                tradeoffs: vec!["The prior boundary remains active for later requests.".to_string()],
            },
        ],
        recommended_option: Some("honor".to_string()),
        affected_boundary_ids: boundary_ids.clone(),
        constraint_ids: conflicts
            .iter()
            .filter(|boundary| boundary.kind == PreferenceKind::HardConstraint)
            .map(|boundary| boundary.id.clone())
            .collect(),
        expected_tradeoffs: vec![
            "Honoring the boundary preserves the earlier user-defined pause point.".to_string(),
            "An override permits the current request without rewriting historical guidance.".to_string(),
        ],
        authority_required: Some(highest_authority),
        status: CrossroadStatus::Open,
        actor: Actor::System,
        source_refs: source_refs.clone(),
        linked_scratchpad_wait_id,
        timestamps: Timestamps::now(),
        privacy,
    });
    let provenance_at = crossroad.timestamps.created_at;
    if existing_crossroad.is_none()
        && let Err(err) = state_db
            .record_crossroad(
                crossroad,
                ProvenanceWriteOptions {
                    idempotency_key: Some(format!(
                        "preflight-crossroad:{task_id}:{}:{boundary_key}",
                        turn_context.sub_id
                    )),
                    actor: Actor::System,
                    occurred_at: provenance_at,
                },
            )
            .await
    {
        warn!("failed to record decision provenance crossroad: {err:#}");
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: format!(
                    "This request intersects preference boundary {boundary_key}, but its crossroad could not be persisted. The turn is paused until the conflict can be reviewed."
                ),
            }),
        )
        .await;
        return ProvenancePreflightOutcome::Blocked;
    }

    let notification_message = if !can_override {
        format!(
            "Current request intersects higher-authority preference boundary {boundary_key}; it cannot be overridden by a user request."
        )
    } else if explicit_override {
        format!(
            "Current request explicitly crosses preference boundary {boundary_key}; a scoped user override was recorded and the prior boundary remains historical."
        )
    } else {
        format!(
            "Current request intersects preference boundary {boundary_key}; review crossroad {crossroad_id} before treating the choice as an override."
        )
    };
    let notification_id = stable_provenance_id(
        "notification",
        &format!("preflight:{task_id}:{}:{boundary_key}", turn_context.sub_id),
    );
    if let Err(err) = state_db
        .record_notification(
            ProvenanceNotification {
                id: notification_id,
                category,
                message: notification_message.clone(),
                preference_boundary_id: boundary_ids.first().cloned(),
                crossroad_id: Some(crossroad_id.clone()),
                decision_id: None,
                authority_required: Some(highest_authority),
                choice: Some(
                    "Honor the prior boundary or explicitly override it for this request."
                        .to_string(),
                ),
                will_record: true,
                created_at: provenance_at,
                source_refs: source_refs.clone(),
                privacy,
            },
            ProvenanceWriteOptions {
                idempotency_key: Some(format!(
                    "preflight-notification:{task_id}:{}:{boundary_key}",
                    turn_context.sub_id
                )),
                actor: Actor::System,
                occurred_at: provenance_at,
            },
        )
        .await
    {
        warn!("failed to record decision provenance notification: {err:#}");
    }

    if explicit_override && can_override {
        let decision_id = stable_provenance_id(
            "decision",
            &format!(
                "preflight-override:{task_id}:{}:{boundary_key}",
                turn_context.sub_id
            ),
        );
        let warrant_id = stable_provenance_id("warrant", &format!("{decision_id}:warrant"));
        let timestamps = Timestamps {
            created_at: provenance_at,
            observed_at: Some(provenance_at),
            recorded_at: provenance_at,
            effective_at: None,
            superseded_at: None,
            updated_at: Some(provenance_at),
        };
        let warrant = Warrant {
            id: warrant_id.clone(),
            decision_id: decision_id.clone(),
            observations: vec![
                "The current user instruction explicitly requested proceeding across a prior scoped boundary."
                    .to_string(),
            ],
            assumptions: vec![
                "The override applies only to the current request scope.".to_string(),
            ],
            priorities: vec!["Current explicit user instruction within its allowed scope.".to_string()],
            evidence_refs: source_refs.clone(),
            tradeoffs: vec![
                "The earlier boundary remains active and discoverable for later requests.".to_string(),
            ],
            uncertainty: None,
            qualifier: Some("This is a scoped override, not a withdrawal of the prior boundary.".to_string()),
            timestamps: timestamps.clone(),
            privacy,
        };
        if let Err(err) = state_db
            .record_warrant(
                warrant,
                ProvenanceWriteOptions {
                    idempotency_key: Some(format!("preflight-warrant:{decision_id}")),
                    actor: Actor::User,
                    occurred_at: timestamps.recorded_at,
                },
            )
            .await
        {
            warn!("failed to record decision provenance warrant: {err:#}");
            sess.send_event(
                turn_context,
                EventMsg::Warning(WarningEvent {
                    message: format!(
                        "This request crosses preference boundary {boundary_key}, but its override warrant could not be recorded. The turn is paused."
                    ),
                }),
            )
            .await;
            return ProvenancePreflightOutcome::Blocked;
        }

        if let Err(err) = state_db
            .record_decision(
                Decision {
                    id: decision_id.clone(),
                    parent_crossroad_id: Some(crossroad_id.clone()),
                    selected_option: "proceed".to_string(),
                    unselected_options: vec!["honor".to_string()],
                    actor: Actor::User,
                    approval_state: ApprovalState::Approved,
                    authority_basis: Authority::User,
                    summary: "The current user instruction overrides the prior boundary for this request scope."
                        .to_string(),
                    rationale: Some(
                        "The prior boundary is preserved as historical guidance; this decision records the explicit current pivot."
                            .to_string(),
                    ),
                    assumptions: vec![
                        "The override is limited to the current request and does not rewrite the boundary.".to_string(),
                    ],
                    tradeoffs: vec![
                        "Proceeding avoids the earlier pause for this request while preserving the future pause point.".to_string(),
                    ],
                    request_ref: Some(request_ref),
                    task_ref: Some(task_id.clone()),
                    project_ref,
                    repository,
                    source_session_id: Some(session_id.clone()),
                    source_turn_id: Some(turn_context.sub_id.clone()),
                    related_preference_boundary_ids: boundary_ids.clone(),
                    related_constraint_ids: Vec::new(),
                    warrant_id: Some(warrant_id),
                    change_set_ids: Vec::new(),
                    status: DecisionStatus::Accepted,
                    timestamps,
                    superseded_by: None,
                    reopened_as: None,
                    privacy,
                },
                ProvenanceWriteOptions {
                    idempotency_key: Some(format!("preflight-decision:{decision_id}")),
                    actor: Actor::User,
                    occurred_at: provenance_at,
                },
            )
            .await
        {
            warn!("failed to record decision provenance override: {err:#}");
            sess.send_event(
                turn_context,
                EventMsg::Warning(WarningEvent {
                    message: format!(
                        "This request crosses preference boundary {boundary_key}, but its override decision could not be recorded. The turn is paused."
                    ),
                }),
            )
            .await;
            return ProvenancePreflightOutcome::Blocked;
        }

        for boundary_id in &boundary_ids {
            if let Err(err) = state_db
                .record_relationship(
                    ProvenanceRelationship {
                        id: stable_provenance_id(
                            "relationship",
                            &format!("{decision_id}:conflicts:{boundary_id}"),
                        ),
                        from_type: codex_state::decision_provenance::EntityType::Decision,
                        from_id: decision_id.clone(),
                        relation: RelationshipKind::ConflictsWith,
                        to_type: codex_state::decision_provenance::EntityType::PreferenceBoundary,
                        to_id: boundary_id.clone(),
                        evidence: RelationshipEvidence::Explicit,
                        summary: Some(
                            "the current user decision explicitly crossed this prior boundary"
                                .to_string(),
                        ),
                        source_refs: source_refs.clone(),
                        created_at: provenance_at,
                        privacy,
                    },
                    ProvenanceWriteOptions {
                        idempotency_key: Some(format!(
                            "preflight-decision-conflict:{decision_id}:{boundary_id}"
                        )),
                        actor: Actor::User,
                        occurred_at: provenance_at,
                    },
                )
                .await
            {
                warn!("failed to link decision provenance override: {err:#}");
            }
        }
        if let Err(err) = state_db
            .transition_crossroad(
                &crossroad_id,
                CrossroadStatus::Resolved,
                ProvenanceWriteOptions {
                    idempotency_key: Some(format!(
                        "preflight-crossroad-resolved:{task_id}:{}:{boundary_key}",
                        turn_context.sub_id
                    )),
                    actor: Actor::User,
                    occurred_at: provenance_at,
                },
            )
            .await
        {
            warn!("failed to resolve decision provenance crossroad: {err:#}");
        }
    }

    sess.send_event(
        turn_context,
        EventMsg::Warning(WarningEvent {
            message: notification_message,
        }),
    )
    .await;

    if !can_override || !explicit_override {
        ProvenancePreflightOutcome::Blocked
    } else {
        ProvenancePreflightOutcome::Continue
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
            text.truncate(4_096);
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
    boundary: &PreferenceBoundary,
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
    if request_has_explicit_override(request_text) {
        return true;
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

fn request_has_explicit_override(request_text: &str) -> bool {
    let request_text = request_text.to_ascii_lowercase();
    [
        "override",
        "ignore the previous",
        "ignore that preference",
        "skip confirmation",
        "skip the confirmation",
        "do not ask",
        "don't ask",
        "proceed without asking",
        "go ahead without asking",
        "instead of asking",
    ]
    .iter()
    .any(|marker| request_text.contains(marker))
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
