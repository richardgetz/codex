//! Adapter from user-preferences memory observations to structured provenance boundaries.
//!
//! The memory extension remains the source for generic reusable preference guidance. The
//! provenance boundary is the canonical lifecycle record for a boundary, while this adapter gives
//! it a stable link to the memory observation without copying the memory files or changing their
//! aggregation and consolidation behavior.

use super::types::CandidateMemoryItem;
use super::types::MemoryBucket;
use super::types::MemoryOperation;
use super::types::MemoryScope;
use super::types::MemoryScopeKind;
use super::types::MemorySignal;
use crate::session::session::Session;
use codex_state::decision_provenance::Actor;
use codex_state::decision_provenance::Authority;
use codex_state::decision_provenance::BoundaryTransition;
use codex_state::decision_provenance::BoundaryTransitionKind;
use codex_state::decision_provenance::LifecycleStatus;
use codex_state::decision_provenance::PreferenceBoundary;
use codex_state::decision_provenance::PreferenceBoundaryFilter;
use codex_state::decision_provenance::PreferenceKind;
use codex_state::decision_provenance::PreferenceStrength;
use codex_state::decision_provenance::PrivacyClass;
use codex_state::decision_provenance::ProvenanceWriteOptions;
use codex_state::decision_provenance::Scope;
use codex_state::decision_provenance::ScopeRef;
use codex_state::decision_provenance::SourceReference;
use codex_state::decision_provenance::Timestamps;
use codex_state::decision_provenance::now;
use uuid::Uuid;

const MEMORY_RECORD_SOURCE: &str = "user_preferences_memory";

pub(super) async fn withdraw_memory_boundaries(
    session: &Session,
    needle: Option<&str>,
) -> anyhow::Result<()> {
    let Some(state_db) = session.state_db() else {
        return Ok(());
    };
    let needle_key = needle
        .map(super::heuristics::normalized_key)
        .filter(|needle| !needle.is_empty());
    let boundaries = state_db
        .list_preference_boundaries(PreferenceBoundaryFilter {
            limit: codex_state::decision_provenance::MAX_QUERY_RESULTS,
            ..PreferenceBoundaryFilter::default()
        })
        .await?;
    let forget_id = needle_key.as_deref().map(|needle| {
        Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("memory-forget:{needle}").as_bytes(),
        )
        .simple()
        .to_string()
    });
    for boundary in boundaries.into_iter().filter(|boundary| {
        boundary.related_memory_record_id.is_some()
            && matches!(
                boundary.lifecycle_status,
                LifecycleStatus::Candidate | LifecycleStatus::Active | LifecycleStatus::Confirmed
            )
            && needle_key
                .as_deref()
                .is_none_or(|needle| boundary_matches_needle(boundary, needle))
    }) {
        let actor = match boundary.authority {
            Authority::Agent if boundary.lifecycle_status == LifecycleStatus::Candidate => {
                Actor::Agent
            }
            Authority::User | Authority::Default => Actor::User,
            Authority::Agent
            | Authority::System
            | Authority::Developer
            | Authority::Safety
            | Authority::Legal
            | Authority::Privacy
            | Authority::Security
            | Authority::Repository
            | Authority::Product => continue,
        };
        let operation = needle_key.as_deref().map_or("clear", |_| "forget");
        let idempotency_key = format!(
            "memory-boundary-{operation}:{}:{}",
            boundary.id,
            forget_id.as_deref().unwrap_or("all")
        );
        state_db
            .transition_preference_boundary(
                BoundaryTransition {
                    boundary_id: boundary.id,
                    transition: BoundaryTransitionKind::Withdraw,
                    replacement: None,
                    actor,
                    source: Some(SourceReference::new(
                        MEMORY_RECORD_SOURCE,
                        format!("{operation}:provenance-sync"),
                    )),
                },
                ProvenanceWriteOptions {
                    idempotency_key: Some(idempotency_key),
                    actor,
                    occurred_at: now(),
                },
            )
            .await?;
    }
    Ok(())
}

fn boundary_matches_needle(boundary: &PreferenceBoundary, needle: &str) -> bool {
    let statement = format!(
        "{} {}",
        boundary.statement,
        boundary.rationale.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    statement.contains(&needle)
        || needle
            .split_whitespace()
            .filter(|token| token.len() > 2)
            .all(|token| statement.contains(token))
}

pub(super) async fn record_candidate_boundaries(
    session: &Session,
    candidates: &[CandidateMemoryItem],
) -> anyhow::Result<()> {
    let Some(state_db) = session.state_db() else {
        return Ok(());
    };

    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.bucket == MemoryBucket::DurablePreference)
    {
        let memory_record_id = memory_record_id(candidate);
        let boundary_id = format!("memory-boundary-{memory_record_id}");
        let explicit = candidate.signal == MemorySignal::Explicit;
        let existing = state_db
            .get_preference_boundary_for_memory_record(&memory_record_id)
            .await?
            .or(state_db.get_preference_boundary(&boundary_id).await?);
        let candidate_scope = scope_ref(&candidate.scope);
        match (candidate.operation, existing) {
            (MemoryOperation::Upsert, None) => {
                state_db
                    .record_preference_boundary(
                        boundary(candidate, &boundary_id, &memory_record_id, explicit),
                        ProvenanceWriteOptions {
                            idempotency_key: Some(format!(
                                "memory-boundary-record:{memory_record_id}"
                            )),
                            actor: if explicit { Actor::User } else { Actor::Agent },
                            occurred_at: now(),
                        },
                    )
                    .await?;
            }
            (MemoryOperation::Upsert, Some(existing))
                if (existing.statement != candidate.candidate
                    || existing.scope != candidate_scope)
                    && (explicit
                        || (existing.is_candidate() && existing.authority == Authority::Agent)) =>
            {
                let replacement_id = replacement_boundary_id(&memory_record_id, candidate);
                let actor = if explicit { Actor::User } else { Actor::Agent };
                state_db
                    .transition_preference_boundary(
                        BoundaryTransition {
                            boundary_id: existing.id.clone(),
                            transition: BoundaryTransitionKind::Supersede,
                            replacement: Some(boundary(
                                candidate,
                                &replacement_id,
                                &memory_record_id,
                                explicit,
                            )),
                            actor,
                            source: Some(SourceReference::new(
                                MEMORY_RECORD_SOURCE,
                                memory_record_id.clone(),
                            )),
                        },
                        ProvenanceWriteOptions {
                            idempotency_key: Some(format!(
                                "memory-boundary-supersede:{memory_record_id}:{replacement_id}"
                            )),
                            actor,
                            occurred_at: now(),
                        },
                    )
                    .await?;
            }
            (MemoryOperation::Upsert, Some(existing)) if explicit => {
                let (transition, key) = if existing.is_candidate() {
                    (BoundaryTransitionKind::Confirm, "confirm")
                } else if !existing.lifecycle_status.is_active() {
                    (BoundaryTransitionKind::Activate, "activate")
                } else {
                    continue;
                };
                state_db
                    .transition_preference_boundary(
                        BoundaryTransition {
                            boundary_id: existing.id.clone(),
                            transition,
                            replacement: None,
                            actor: Actor::User,
                            source: Some(SourceReference::new(
                                MEMORY_RECORD_SOURCE,
                                memory_record_id.clone(),
                            )),
                        },
                        ProvenanceWriteOptions {
                            idempotency_key: Some(format!(
                                "memory-boundary-{key}:{memory_record_id}"
                            )),
                            actor: Actor::User,
                            occurred_at: now(),
                        },
                    )
                    .await?;
            }
            (MemoryOperation::Upsert, Some(_)) => {}
            (MemoryOperation::Forget, Some(existing))
                if existing.lifecycle_status.is_active() || existing.is_candidate() =>
            {
                let actor = if explicit { Actor::User } else { Actor::Agent };
                if let Err(err) = state_db
                    .transition_preference_boundary(
                        BoundaryTransition {
                            boundary_id: existing.id.clone(),
                            transition: BoundaryTransitionKind::Withdraw,
                            replacement: None,
                            actor,
                            source: Some(SourceReference::new(
                                MEMORY_RECORD_SOURCE,
                                memory_record_id.clone(),
                            )),
                        },
                        ProvenanceWriteOptions {
                            idempotency_key: Some(format!(
                                "memory-boundary-withdraw:{memory_record_id}"
                            )),
                            actor,
                            occurred_at: now(),
                        },
                    )
                    .await
                {
                    if explicit {
                        return Err(err);
                    }
                    tracing::debug!(
                        "skipping agent memory-forget transition for {}: {err:#}",
                        existing.id
                    );
                }
            }
            (MemoryOperation::Forget, Some(_)) | (MemoryOperation::Forget, None) => {}
        }
    }
    Ok(())
}

fn boundary(
    candidate: &CandidateMemoryItem,
    boundary_id: &str,
    memory_record_id: &str,
    explicit: bool,
) -> PreferenceBoundary {
    let timestamp = now();
    PreferenceBoundary {
        id: boundary_id.to_string(),
        kind: if explicit {
            PreferenceKind::PreferenceBoundary
        } else {
            PreferenceKind::CandidatePreference
        },
        statement: candidate.candidate.clone(),
        scope: scope_ref(&candidate.scope),
        strength: if explicit {
            PreferenceStrength::Confirmation
        } else {
            PreferenceStrength::Soft
        },
        authority: if explicit {
            Authority::User
        } else {
            Authority::Agent
        },
        source: SourceReference::new(MEMORY_RECORD_SOURCE, memory_record_id),
        rationale: None,
        confidence: (!explicit).then(|| {
            (candidate.confidence.clamp(0.0, 1.0) * 100.0)
                .round()
                .clamp(0.0, 100.0) as u8
        }),
        lifecycle_status: if explicit {
            LifecycleStatus::Active
        } else {
            LifecycleStatus::Candidate
        },
        timestamps: Timestamps {
            created_at: timestamp,
            observed_at: Some(timestamp),
            recorded_at: timestamp,
            effective_at: explicit.then_some(timestamp),
            superseded_at: None,
            updated_at: Some(timestamp),
        },
        related_memory_record_id: Some(memory_record_id.to_string()),
        superseded_by: None,
        privacy: PrivacyClass::Private,
    }
}

fn replacement_boundary_id(memory_record_id: &str, candidate: &CandidateMemoryItem) -> String {
    let key = format!(
        "memory-boundary-replacement:{memory_record_id}:{}:{}:{}",
        candidate.scope.kind.as_str(),
        candidate.scope.id,
        candidate.candidate
    );
    format!(
        "memory-boundary-{}",
        Uuid::new_v5(&Uuid::NAMESPACE_OID, key.as_bytes()).simple()
    )
}

fn scope_ref(scope: &MemoryScope) -> ScopeRef {
    let kind = match scope.kind {
        MemoryScopeKind::Global => Scope::Global,
        MemoryScopeKind::Repo => Scope::Repo,
        MemoryScopeKind::Project => Scope::Project,
        MemoryScopeKind::Task => Scope::Task,
        MemoryScopeKind::Person => Scope::Person,
        MemoryScopeKind::Process => Scope::Process,
        MemoryScopeKind::Skill => Scope::Skill,
        MemoryScopeKind::Command => Scope::Command,
        MemoryScopeKind::Tool => Scope::Tool,
    };
    ScopeRef::new(kind, scope.id.clone())
}

/// Derive a stable private identifier from the memory aggregation key.
///
/// The memory files remain canonical for reusable guidance. The identifier lets provenance link
/// back to that record without persisting a second editable copy of the memory event stream.
fn memory_record_id(candidate: &CandidateMemoryItem) -> String {
    let key = format!(
        "{}:{}:{}:{}",
        candidate.bucket.as_str(),
        candidate.scope.kind.as_str(),
        candidate.scope.id,
        candidate.key
    );
    Uuid::new_v5(&Uuid::NAMESPACE_OID, key.as_bytes())
        .simple()
        .to_string()
}
