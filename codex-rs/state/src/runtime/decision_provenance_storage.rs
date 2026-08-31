//! Materialization and event-storage helpers for decision provenance.

use crate::decision_provenance::Actor;
use crate::decision_provenance::Authority;
use crate::decision_provenance::BoundaryTransition;
use crate::decision_provenance::BoundaryTransitionKind;
use crate::decision_provenance::ChangeSet;
use crate::decision_provenance::Crossroad;
use crate::decision_provenance::CrossroadStatus;
use crate::decision_provenance::Decision;
use crate::decision_provenance::DecisionStatus;
use crate::decision_provenance::EntityType;
use crate::decision_provenance::PreferenceBoundary;
use crate::decision_provenance::ProvenanceEvent;
use crate::decision_provenance::ProvenanceEventPayload;
use crate::decision_provenance::ProvenanceEventType;
use crate::decision_provenance::ProvenanceNotification;
use crate::decision_provenance::ProvenanceRelationship;
use crate::decision_provenance::ProvenanceWriteOptions;
use crate::decision_provenance::RelationshipEvidence;
use crate::decision_provenance::RelationshipKind;
use crate::decision_provenance::Warrant;
use crate::decision_provenance::new_id;
use crate::decision_provenance::now;
use anyhow::Context;
use chrono::DateTime;
use chrono::TimeZone;
use chrono::Utc;
use serde::de::DeserializeOwned;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::Transaction;
use sqlx::sqlite::SqliteRow;

pub(super) fn event_for(
    event_type: ProvenanceEventType,
    aggregate_type: EntityType,
    aggregate_id: String,
    privacy: crate::decision_provenance::PrivacyClass,
    options: ProvenanceWriteOptions,
    payload: ProvenanceEventPayload,
) -> ProvenanceEvent {
    let idempotency_key = options.idempotency_key.or_else(|| {
        matches!(
            event_type,
            ProvenanceEventType::PreferenceBoundaryRecorded
                | ProvenanceEventType::CrossroadRecorded
                | ProvenanceEventType::DecisionRecorded
                | ProvenanceEventType::WarrantRecorded
                | ProvenanceEventType::ChangeSetLinked
                | ProvenanceEventType::RelationshipRecorded
                | ProvenanceEventType::NotificationRecorded
        )
        .then(|| {
            format!(
                "{}:{}:{aggregate_id}",
                event_type.as_str(),
                aggregate_type.as_str()
            )
        })
    });
    ProvenanceEvent {
        schema_version: crate::decision_provenance::PROVENANCE_EVENT_VERSION,
        event_id: new_id("event"),
        idempotency_key,
        event_type,
        aggregate_type,
        aggregate_id,
        occurred_at: options.occurred_at,
        actor: options.actor,
        privacy,
        payload,
    }
}

pub(super) fn validate_boundary_transition(
    current: &PreferenceBoundary,
    transition: &BoundaryTransition,
) -> anyhow::Result<()> {
    if matches!(transition.actor, Actor::Agent)
        && (!current.is_candidate() || current.authority != Authority::Agent)
    {
        anyhow::bail!("an agent may only change its own unconfirmed candidate preference boundary");
    }
    if current.authority.precedence() > Authority::User.precedence()
        && !matches!(transition.actor, Actor::System)
    {
        anyhow::bail!(
            "{} authority cannot be overridden by a {} boundary transition",
            current.authority.as_str(),
            transition.actor.as_str()
        );
    }
    if current.is_candidate()
        && matches!(
            transition.transition,
            BoundaryTransitionKind::Confirm | BoundaryTransitionKind::Activate
        )
        && !matches!(transition.actor, Actor::User)
    {
        anyhow::bail!("an inferred preference must be explicitly confirmed by the user");
    }
    Ok(())
}

pub(super) struct EventInsertOutcome {
    pub inserted: bool,
    pub event_id: String,
}

struct StoredEvent {
    event_id: String,
    idempotency_key: Option<String>,
    schema_version: i64,
    event_type: String,
    aggregate_type: String,
    aggregate_id: String,
    occurred_at_ms: i64,
    actor: String,
    privacy_class: String,
    payload_json: String,
}

/// Insert an event row and validate every unique identity when SQLite reports a conflict.
///
/// Event IDs and idempotency keys are both caller-visible identities. SQLite's `ON CONFLICT`
/// clause does not tell us which unique constraint won, so accepting the first row returned by an
/// `OR` query could conflate two different events. Returning the persisted ID also makes retries
/// safe when a caller generated a fresh event ID for an already-used idempotency key.
pub(super) async fn insert_event_row(
    tx: &mut Transaction<'_, Sqlite>,
    event: &ProvenanceEvent,
    payload_json: &str,
) -> anyhow::Result<EventInsertOutcome> {
    let inserted = sqlx::query(
        r#"
INSERT INTO provenance_events (
    event_id,
    schema_version,
    idempotency_key,
    event_type,
    aggregate_type,
    aggregate_id,
    occurred_at_ms,
    actor,
    privacy_class,
    payload_json,
    recorded_at_ms
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT DO NOTHING
        "#,
    )
    .bind(&event.event_id)
    .bind(i64::from(event.schema_version))
    .bind(event.idempotency_key.as_deref())
    .bind(event.event_type.as_str())
    .bind(event.aggregate_type.as_str())
    .bind(&event.aggregate_id)
    .bind(timestamp_millis(event.occurred_at))
    .bind(event.actor.as_str())
    .bind(event.privacy.as_str())
    .bind(payload_json)
    .bind(timestamp_millis(now()))
    .execute(&mut **tx)
    .await?
    .rows_affected()
        > 0;
    if inserted {
        ensure_event_is_not_backdated(tx, event).await?;
        return Ok(EventInsertOutcome {
            inserted: true,
            event_id: event.event_id.clone(),
        });
    }

    let by_event_id = load_event_by_event_id(tx, &event.event_id).await?;
    let by_idempotency_key = match event.idempotency_key.as_deref() {
        Some(idempotency_key) => load_event_by_idempotency_key(tx, idempotency_key).await?,
        None => None,
    };
    let (existing, match_occurred_at) = match (by_event_id, by_idempotency_key) {
        (Some(by_event_id), Some(by_idempotency_key)) => {
            if by_event_id.event_id != by_idempotency_key.event_id {
                anyhow::bail!("event id and idempotency key identify different provenance events");
            }
            (by_event_id, true)
        }
        (Some(existing), None) => (existing, true),
        (None, Some(existing)) => (existing, false),
        (None, None) => {
            anyhow::bail!("provenance insert was ignored but no existing event was found")
        }
    };
    ensure_event_matches(&existing, event, payload_json, match_occurred_at)?;
    Ok(EventInsertOutcome {
        inserted: false,
        event_id: existing.event_id,
    })
}

async fn ensure_event_is_not_backdated(
    tx: &mut Transaction<'_, Sqlite>,
    event: &ProvenanceEvent,
) -> anyhow::Result<()> {
    let latest: Option<i64> = sqlx::query_scalar(
        r#"
SELECT MAX(occurred_at_ms)
FROM provenance_events
WHERE aggregate_type = ? AND aggregate_id = ? AND event_id <> ?
        "#,
    )
    .bind(event.aggregate_type.as_str())
    .bind(&event.aggregate_id)
    .bind(&event.event_id)
    .fetch_one(&mut **tx)
    .await?;
    if let Some(latest) = latest
        && timestamp_millis(event.occurred_at) < latest
    {
        anyhow::bail!(
            "provenance event {} is older than the latest event for {}:{}; historical changes must be replayed with an explicit historical view",
            event.event_id,
            event.aggregate_type.as_str(),
            event.aggregate_id
        );
    }
    Ok(())
}

async fn load_event_by_event_id(
    tx: &mut Transaction<'_, Sqlite>,
    event_id: &str,
) -> anyhow::Result<Option<StoredEvent>> {
    sqlx::query(
        r#"
SELECT event_id, idempotency_key, schema_version, event_type, aggregate_type,
       aggregate_id, occurred_at_ms, actor, privacy_class, payload_json
FROM provenance_events
WHERE event_id = ?
        "#,
    )
    .bind(event_id)
    .fetch_optional(&mut **tx)
    .await?
    .map(decode_stored_event)
    .transpose()
}

async fn load_event_by_idempotency_key(
    tx: &mut Transaction<'_, Sqlite>,
    idempotency_key: &str,
) -> anyhow::Result<Option<StoredEvent>> {
    sqlx::query(
        r#"
SELECT event_id, idempotency_key, schema_version, event_type, aggregate_type,
       aggregate_id, occurred_at_ms, actor, privacy_class, payload_json
FROM provenance_events
WHERE idempotency_key = ?
        "#,
    )
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await?
    .map(decode_stored_event)
    .transpose()
}

fn decode_stored_event(row: SqliteRow) -> anyhow::Result<StoredEvent> {
    Ok(StoredEvent {
        event_id: row.try_get("event_id")?,
        idempotency_key: row.try_get("idempotency_key")?,
        schema_version: row.try_get("schema_version")?,
        event_type: row.try_get("event_type")?,
        aggregate_type: row.try_get("aggregate_type")?,
        aggregate_id: row.try_get("aggregate_id")?,
        occurred_at_ms: row.try_get("occurred_at_ms")?,
        actor: row.try_get("actor")?,
        privacy_class: row.try_get("privacy_class")?,
        payload_json: row.try_get("payload_json")?,
    })
}

fn ensure_event_matches(
    existing: &StoredEvent,
    event: &ProvenanceEvent,
    payload_json: &str,
    match_occurred_at: bool,
) -> anyhow::Result<()> {
    if existing.idempotency_key != event.idempotency_key
        || existing.schema_version != i64::from(event.schema_version)
        || existing.payload_json != payload_json
        || existing.event_type != event.event_type.as_str()
        || existing.aggregate_type != event.aggregate_type.as_str()
        || existing.aggregate_id != event.aggregate_id
        || existing.actor != event.actor.as_str()
        || existing.privacy_class != event.privacy.as_str()
        || (match_occurred_at && existing.occurred_at_ms != timestamp_millis(event.occurred_at))
    {
        anyhow::bail!(
            "provenance idempotency key or event id already identifies a different event"
        );
    }
    Ok(())
}

pub(super) fn validate_event(event: &ProvenanceEvent) -> anyhow::Result<()> {
    if event.schema_version != crate::decision_provenance::PROVENANCE_EVENT_VERSION {
        anyhow::bail!(
            "unsupported provenance event schema version {}",
            event.schema_version
        );
    }
    let (event_type, aggregate_type, aggregate_id) = match &event.payload {
        ProvenanceEventPayload::PreferenceBoundary(boundary) => (
            ProvenanceEventType::PreferenceBoundaryRecorded,
            EntityType::PreferenceBoundary,
            &boundary.id,
        ),
        ProvenanceEventPayload::BoundaryTransition(transition) => (
            ProvenanceEventType::PreferenceBoundaryTransitioned,
            EntityType::PreferenceBoundary,
            &transition.boundary_id,
        ),
        ProvenanceEventPayload::Crossroad(crossroad) => (
            ProvenanceEventType::CrossroadRecorded,
            EntityType::Crossroad,
            &crossroad.id,
        ),
        ProvenanceEventPayload::CrossroadStatus { id, .. } => (
            ProvenanceEventType::CrossroadStatusChanged,
            EntityType::Crossroad,
            id,
        ),
        ProvenanceEventPayload::Decision(decision) => (
            ProvenanceEventType::DecisionRecorded,
            EntityType::Decision,
            &decision.id,
        ),
        ProvenanceEventPayload::DecisionStatus { id, .. } => (
            ProvenanceEventType::DecisionStatusChanged,
            EntityType::Decision,
            id,
        ),
        ProvenanceEventPayload::Warrant(warrant) => (
            ProvenanceEventType::WarrantRecorded,
            EntityType::Warrant,
            &warrant.id,
        ),
        ProvenanceEventPayload::ChangeSet(change_set) => (
            ProvenanceEventType::ChangeSetLinked,
            EntityType::ChangeSet,
            &change_set.id,
        ),
        ProvenanceEventPayload::Relationship(relationship) => (
            ProvenanceEventType::RelationshipRecorded,
            EntityType::Relationship,
            &relationship.id,
        ),
        ProvenanceEventPayload::Notification(notification) => (
            ProvenanceEventType::NotificationRecorded,
            EntityType::Notification,
            &notification.id,
        ),
    };
    if event.event_type != event_type
        || event.aggregate_type != aggregate_type
        || event.aggregate_id != *aggregate_id
    {
        anyhow::bail!("provenance event metadata does not match its payload");
    }
    match &event.payload {
        ProvenanceEventPayload::PreferenceBoundary(boundary) => {
            validate_boundary_record_actor(event.actor, boundary)?;
        }
        ProvenanceEventPayload::BoundaryTransition(transition)
            if event.actor != transition.actor =>
        {
            anyhow::bail!(
                "preference boundary transition actor must match the provenance event actor"
            );
        }
        ProvenanceEventPayload::BoundaryTransition(transition) => {
            if let Some(replacement) = &transition.replacement {
                validate_boundary_replacement_actor(event.actor, replacement)?;
            }
        }
        ProvenanceEventPayload::Crossroad(crossroad) if event.actor != crossroad.actor => {
            anyhow::bail!("crossroad actor must match the provenance event actor");
        }
        ProvenanceEventPayload::Decision(decision) if event.actor != decision.actor => {
            anyhow::bail!("decision actor must match the provenance event actor");
        }
        _ => {}
    }
    Ok(())
}

fn validate_boundary_record_actor(
    actor: Actor,
    boundary: &PreferenceBoundary,
) -> anyhow::Result<()> {
    if boundary.is_candidate() && !matches!(actor, Actor::Agent | Actor::System) {
        anyhow::bail!("candidate preference boundaries must be recorded by an agent or system");
    }
    if boundary.authority == Authority::User && !matches!(actor, Actor::User | Actor::Collaborative)
    {
        anyhow::bail!("user preference boundaries must be recorded by the user or collaboratively");
    }
    if boundary.authority == Authority::Agent && !matches!(actor, Actor::Agent | Actor::System) {
        anyhow::bail!("agent preference boundaries must be recorded by the agent or system");
    }
    Ok(())
}

fn validate_boundary_replacement_actor(
    actor: Actor,
    replacement: &PreferenceBoundary,
) -> anyhow::Result<()> {
    match actor {
        Actor::Agent if replacement.is_candidate() && replacement.authority == Authority::Agent => {
            Ok(())
        }
        Actor::Agent => {
            anyhow::bail!("agent boundary replacements must remain unconfirmed agent candidates")
        }
        Actor::User | Actor::Collaborative
            if replacement.kind
                == crate::decision_provenance::PreferenceKind::PreferenceBoundary
                && replacement.strength
                    == crate::decision_provenance::PreferenceStrength::Confirmation
                && replacement.authority == Authority::User
                && replacement.lifecycle_status.is_active() =>
        {
            Ok(())
        }
        Actor::User | Actor::Collaborative => {
            anyhow::bail!("user boundary replacements must be active confirmed user boundaries")
        }
        Actor::System => Ok(()),
    }
}

pub(super) async fn materialize_event(
    tx: &mut Transaction<'_, Sqlite>,
    event: &ProvenanceEvent,
) -> anyhow::Result<()> {
    match &event.payload {
        ProvenanceEventPayload::PreferenceBoundary(boundary) => {
            insert_boundary(tx, boundary, &serde_json::to_string(boundary)?).await?;
        }
        ProvenanceEventPayload::BoundaryTransition(transition) => {
            materialize_boundary_transition(tx, event, transition).await?;
        }
        ProvenanceEventPayload::Crossroad(crossroad) => {
            insert_crossroad(tx, crossroad, &serde_json::to_string(crossroad)?).await?;
        }
        ProvenanceEventPayload::CrossroadStatus { id, status } => {
            update_crossroad_status(tx, id, *status, event.occurred_at).await?;
        }
        ProvenanceEventPayload::Decision(decision) => {
            insert_decision(tx, decision, &serde_json::to_string(decision)?).await?;
        }
        ProvenanceEventPayload::DecisionStatus { id, status } => {
            update_decision_status(tx, id, *status, event.occurred_at).await?;
        }
        ProvenanceEventPayload::Warrant(warrant) => {
            insert_warrant(tx, warrant, &serde_json::to_string(warrant)?).await?;
        }
        ProvenanceEventPayload::ChangeSet(change_set) => {
            insert_change_set(tx, change_set, &serde_json::to_string(change_set)?).await?;
        }
        ProvenanceEventPayload::Relationship(relationship) => {
            insert_relationship(tx, relationship, &serde_json::to_string(relationship)?).await?;
        }
        ProvenanceEventPayload::Notification(notification) => {
            insert_notification(tx, notification, &serde_json::to_string(notification)?).await?;
        }
    }
    Ok(())
}

async fn insert_boundary(
    tx: &mut Transaction<'_, Sqlite>,
    boundary: &PreferenceBoundary,
    payload_json: &str,
) -> anyhow::Result<()> {
    ensure_id(&boundary.id, "preference boundary")?;
    let result = sqlx::query(
        r#"
INSERT INTO preference_boundaries (
    id, scope_kind, scope_id, kind, strength, authority, lifecycle_status,
    related_memory_record_id, created_at_ms, updated_at_ms, payload_json
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO NOTHING
        "#,
    )
    .bind(&boundary.id)
    .bind(boundary.scope.kind.as_str())
    .bind(&boundary.scope.id)
    .bind(boundary.kind.as_str())
    .bind(boundary.strength.as_str())
    .bind(boundary.authority.as_str())
    .bind(boundary.lifecycle_status.as_str())
    .bind(boundary.related_memory_record_id.as_deref())
    .bind(timestamp_millis(boundary.timestamps.created_at))
    .bind(timestamp_millis(
        boundary
            .timestamps
            .updated_at
            .unwrap_or(boundary.timestamps.recorded_at),
    ))
    .bind(payload_json)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 0 {
        let existing: String =
            sqlx::query_scalar("SELECT payload_json FROM preference_boundaries WHERE id = ?")
                .bind(&boundary.id)
                .fetch_one(&mut **tx)
                .await?;
        if existing != payload_json {
            anyhow::bail!(
                "preference boundary {} is immutable; record a transition or replacement",
                boundary.id
            );
        }
    }
    Ok(())
}

async fn materialize_boundary_transition(
    tx: &mut Transaction<'_, Sqlite>,
    event: &ProvenanceEvent,
    transition: &BoundaryTransition,
) -> anyhow::Result<()> {
    let current_json: String =
        sqlx::query_scalar("SELECT payload_json FROM preference_boundaries WHERE id = ?")
            .bind(&transition.boundary_id)
            .fetch_optional(&mut **tx)
            .await?
            .context("preference boundary transition refers to an unknown boundary")?;
    let mut current: PreferenceBoundary = serde_json::from_str(&current_json)?;
    validate_boundary_transition(&current, transition)?;
    let updated_at = event.occurred_at;
    current.lifecycle_status = transition.transition.status();
    current.timestamps.updated_at = Some(updated_at);
    if matches!(
        transition.transition,
        BoundaryTransitionKind::Confirm | BoundaryTransitionKind::Activate
    ) && matches!(transition.actor, Actor::User | Actor::Collaborative)
    {
        if current.kind == crate::decision_provenance::PreferenceKind::CandidatePreference {
            current.kind = crate::decision_provenance::PreferenceKind::PreferenceBoundary;
            current.strength = crate::decision_provenance::PreferenceStrength::Confirmation;
        }
        if current.authority == Authority::Agent {
            current.authority = Authority::User;
        }
        current.confidence = None;
        current.timestamps.effective_at = Some(updated_at);
    }
    if matches!(
        transition.transition,
        BoundaryTransitionKind::Supersede
            | BoundaryTransitionKind::Narrow
            | BoundaryTransitionKind::Broaden
    ) {
        current.timestamps.superseded_at = Some(updated_at);
    }
    if let Some(replacement) = &transition.replacement {
        if replacement.id == transition.boundary_id {
            anyhow::bail!("a preference boundary replacement must have a new stable id");
        }
        current.superseded_by = Some(replacement.id.clone());
        let replacement_json = serde_json::to_string(replacement)?;
        insert_boundary(tx, replacement, &replacement_json).await?;
        let replacement_event = ProvenanceEvent {
            schema_version: crate::decision_provenance::PROVENANCE_EVENT_VERSION,
            event_id: format!("{}:replacement", event.event_id),
            idempotency_key: Some(format!("{}:replacement", event.event_id)),
            event_type: ProvenanceEventType::PreferenceBoundaryRecorded,
            aggregate_type: EntityType::PreferenceBoundary,
            aggregate_id: replacement.id.clone(),
            occurred_at: event.occurred_at,
            actor: event.actor,
            privacy: replacement.privacy,
            payload: ProvenanceEventPayload::PreferenceBoundary(replacement.clone()),
        };
        let replacement_payload_json = serde_json::to_string(&replacement_event.payload)?;
        insert_event_row(tx, &replacement_event, &replacement_payload_json).await?;
        if let Some(relation) = transition.transition.relationship() {
            let relationship = ProvenanceRelationship {
                id: new_id("relationship"),
                from_type: EntityType::PreferenceBoundary,
                from_id: transition.boundary_id.clone(),
                relation,
                to_type: EntityType::PreferenceBoundary,
                to_id: replacement.id.clone(),
                evidence: RelationshipEvidence::Explicit,
                summary: Some("boundary lifecycle transition".to_string()),
                source_refs: transition.source.clone().into_iter().collect(),
                created_at: updated_at,
                privacy: replacement.privacy,
            };
            let relationship_json = serde_json::to_string(&relationship)?;
            insert_relationship(tx, &relationship, &relationship_json).await?;
        }
    }
    let updated_json = serde_json::to_string(&current)?;
    sqlx::query(
        "UPDATE preference_boundaries SET lifecycle_status = ?, related_memory_record_id = ?, updated_at_ms = ?, payload_json = ? WHERE id = ?",
    )
    .bind(current.lifecycle_status.as_str())
    .bind(current.related_memory_record_id.as_deref())
    .bind(timestamp_millis(updated_at))
    .bind(updated_json)
    .bind(&transition.boundary_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_crossroad(
    tx: &mut Transaction<'_, Sqlite>,
    crossroad: &Crossroad,
    payload_json: &str,
) -> anyhow::Result<()> {
    ensure_id(&crossroad.id, "crossroad")?;
    let result = sqlx::query(
        r#"
INSERT INTO crossroads (
    id, request_ref, task_ref, project_ref, session_id, status, actor,
    authority_required, linked_scratchpad_wait_id, created_at_ms, updated_at_ms, payload_json
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO NOTHING
        "#,
    )
    .bind(&crossroad.id)
    .bind(crossroad.request_ref.as_deref())
    .bind(crossroad.task_ref.as_deref())
    .bind(crossroad.project_ref.as_deref())
    .bind(crossroad.session_id.as_deref())
    .bind(crossroad.status.as_str())
    .bind(crossroad.actor.as_str())
    .bind(crossroad.authority_required.map(Authority::as_str))
    .bind(crossroad.linked_scratchpad_wait_id.as_deref())
    .bind(timestamp_millis(crossroad.timestamps.created_at))
    .bind(timestamp_millis(
        crossroad
            .timestamps
            .updated_at
            .unwrap_or(crossroad.timestamps.recorded_at),
    ))
    .bind(payload_json)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 0 {
        ensure_same_payload(tx, "crossroads", &crossroad.id, payload_json).await?;
    }
    Ok(())
}

async fn insert_decision(
    tx: &mut Transaction<'_, Sqlite>,
    decision: &Decision,
    payload_json: &str,
) -> anyhow::Result<()> {
    ensure_id(&decision.id, "decision")?;
    let result = sqlx::query(
        r#"
INSERT INTO decision_records (
    id, crossroad_id, request_ref, task_ref, project_ref, repository,
    source_session_id, source_turn_id, status, actor, approval_state,
    recorded_at_ms, updated_at_ms, payload_json
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO NOTHING
        "#,
    )
    .bind(&decision.id)
    .bind(decision.parent_crossroad_id.as_deref())
    .bind(decision.request_ref.as_deref())
    .bind(decision.task_ref.as_deref())
    .bind(decision.project_ref.as_deref())
    .bind(decision.repository.as_deref())
    .bind(decision.source_session_id.as_deref())
    .bind(decision.source_turn_id.as_deref())
    .bind(decision.status.as_str())
    .bind(decision.actor.as_str())
    .bind(decision.approval_state.as_str())
    .bind(timestamp_millis(decision.timestamps.recorded_at))
    .bind(timestamp_millis(
        decision
            .timestamps
            .updated_at
            .unwrap_or(decision.timestamps.recorded_at),
    ))
    .bind(payload_json)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 0 {
        ensure_same_payload(tx, "decision_records", &decision.id, payload_json).await?;
        return Ok(());
    }
    for boundary_id in &decision.related_preference_boundary_ids {
        let relationship = ProvenanceRelationship {
            id: new_id("relationship"),
            from_type: EntityType::Decision,
            from_id: decision.id.clone(),
            relation: RelationshipKind::InfluencedBy,
            to_type: EntityType::PreferenceBoundary,
            to_id: boundary_id.clone(),
            evidence: RelationshipEvidence::Explicit,
            summary: Some("decision referenced this preference boundary".to_string()),
            source_refs: Vec::new(),
            created_at: decision.timestamps.recorded_at,
            privacy: decision.privacy,
        };
        let relationship_json = serde_json::to_string(&relationship)?;
        insert_relationship(tx, &relationship, &relationship_json).await?;
    }
    if let Some(crossroad_id) = decision.parent_crossroad_id.as_deref() {
        let relationship = ProvenanceRelationship {
            id: new_id("relationship"),
            from_type: EntityType::Decision,
            from_id: decision.id.clone(),
            relation: RelationshipKind::DerivedFrom,
            to_type: EntityType::Crossroad,
            to_id: crossroad_id.to_string(),
            evidence: RelationshipEvidence::Explicit,
            summary: Some("decision selected a path from this crossroad".to_string()),
            source_refs: Vec::new(),
            created_at: decision.timestamps.recorded_at,
            privacy: decision.privacy,
        };
        let relationship_json = serde_json::to_string(&relationship)?;
        insert_relationship(tx, &relationship, &relationship_json).await?;
    }
    Ok(())
}

async fn insert_warrant(
    tx: &mut Transaction<'_, Sqlite>,
    warrant: &Warrant,
    payload_json: &str,
) -> anyhow::Result<()> {
    ensure_id(&warrant.id, "warrant")?;
    let result = sqlx::query(
        "INSERT INTO decision_warrants (id, decision_id, created_at_ms, payload_json) VALUES (?, ?, ?, ?) ON CONFLICT(id) DO NOTHING",
    )
    .bind(&warrant.id)
    .bind(&warrant.decision_id)
    .bind(timestamp_millis(warrant.timestamps.created_at))
    .bind(payload_json)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 0 {
        ensure_same_payload(tx, "decision_warrants", &warrant.id, payload_json).await?;
    }
    Ok(())
}

async fn insert_change_set(
    tx: &mut Transaction<'_, Sqlite>,
    change_set: &ChangeSet,
    payload_json: &str,
) -> anyhow::Result<()> {
    ensure_id(&change_set.id, "change set")?;
    let result = sqlx::query(
        r#"
INSERT INTO decision_change_sets (
    id, decision_id, session_id, scratchpad_id, commit_sha,
    git_intent_note_ref, pull_request, issue, created_at_ms, payload_json
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO NOTHING
        "#,
    )
    .bind(&change_set.id)
    .bind(change_set.decision_id.as_deref())
    .bind(change_set.session_id.as_deref())
    .bind(change_set.scratchpad_id.as_deref())
    .bind(change_set.commit_sha.as_deref())
    .bind(change_set.git_intent_note_ref.as_deref())
    .bind(change_set.pull_request.as_deref())
    .bind(change_set.issue.as_deref())
    .bind(timestamp_millis(change_set.timestamps.created_at))
    .bind(payload_json)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 0 {
        ensure_same_payload(tx, "decision_change_sets", &change_set.id, payload_json).await?;
        return Ok(());
    }
    if let Some(decision_id) = change_set.decision_id.as_deref() {
        let relationship = ProvenanceRelationship {
            id: new_id("relationship"),
            from_type: EntityType::Decision,
            from_id: decision_id.to_string(),
            relation: RelationshipKind::ImplementedBy,
            to_type: EntityType::ChangeSet,
            to_id: change_set.id.clone(),
            evidence: RelationshipEvidence::Explicit,
            summary: Some("change set links the decision to implementation artifacts".to_string()),
            source_refs: change_set.source_refs.clone(),
            created_at: change_set.timestamps.created_at,
            privacy: change_set.privacy,
        };
        let relationship_json = serde_json::to_string(&relationship)?;
        insert_relationship(tx, &relationship, &relationship_json).await?;
    }
    Ok(())
}

async fn insert_relationship(
    tx: &mut Transaction<'_, Sqlite>,
    relationship: &ProvenanceRelationship,
    payload_json: &str,
) -> anyhow::Result<()> {
    ensure_id(&relationship.id, "relationship")?;
    let result = sqlx::query(
        r#"
INSERT INTO provenance_relationships (
    id, from_type, from_id, relation, to_type, to_id, evidence,
    created_at_ms, payload_json
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(from_type, from_id, relation, to_type, to_id) DO NOTHING
        "#,
    )
    .bind(&relationship.id)
    .bind(relationship.from_type.as_str())
    .bind(&relationship.from_id)
    .bind(relationship.relation.as_str())
    .bind(relationship.to_type.as_str())
    .bind(&relationship.to_id)
    .bind(relationship.evidence.as_str())
    .bind(timestamp_millis(relationship.created_at))
    .bind(payload_json)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 0 {
        let existing = sqlx::query(
            r#"
SELECT id, payload_json
FROM provenance_relationships
WHERE id = ?
   OR (from_type = ? AND from_id = ? AND relation = ? AND to_type = ? AND to_id = ?)
LIMIT 1
            "#,
        )
        .bind(&relationship.id)
        .bind(relationship.from_type.as_str())
        .bind(&relationship.from_id)
        .bind(relationship.relation.as_str())
        .bind(relationship.to_type.as_str())
        .bind(&relationship.to_id)
        .fetch_optional(&mut **tx)
        .await?
        .context("relationship insert was ignored but no existing relationship was found")?;
        let existing_payload: String = existing.try_get("payload_json")?;
        if existing_payload != payload_json {
            anyhow::bail!(
                "relationship {} is immutable and already has different content",
                relationship.id
            );
        }
    }
    Ok(())
}

async fn insert_notification(
    tx: &mut Transaction<'_, Sqlite>,
    notification: &ProvenanceNotification,
    payload_json: &str,
) -> anyhow::Result<()> {
    ensure_id(&notification.id, "notification")?;
    let result = sqlx::query(
        r#"
INSERT INTO provenance_notifications (
    id, category, preference_boundary_id, crossroad_id, decision_id,
    authority_required, created_at_ms, payload_json
) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO NOTHING
        "#,
    )
    .bind(&notification.id)
    .bind(notification.category.as_str())
    .bind(notification.preference_boundary_id.as_deref())
    .bind(notification.crossroad_id.as_deref())
    .bind(notification.decision_id.as_deref())
    .bind(notification.authority_required.map(Authority::as_str))
    .bind(timestamp_millis(notification.created_at))
    .bind(payload_json)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 0 {
        ensure_same_payload(
            tx,
            "provenance_notifications",
            &notification.id,
            payload_json,
        )
        .await?;
    }
    Ok(())
}

async fn update_crossroad_status(
    tx: &mut Transaction<'_, Sqlite>,
    id: &str,
    status: CrossroadStatus,
    occurred_at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let current_json: String =
        sqlx::query_scalar("SELECT payload_json FROM crossroads WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .context("crossroad status change refers to an unknown crossroad")?;
    let mut crossroad: Crossroad = serde_json::from_str(&current_json)?;
    crossroad.status = status;
    crossroad.timestamps.updated_at = Some(occurred_at);
    let updated_json = serde_json::to_string(&crossroad)?;
    sqlx::query(
        "UPDATE crossroads SET status = ?, updated_at_ms = ?, payload_json = ? WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(timestamp_millis(
        crossroad.timestamps.updated_at.unwrap_or(occurred_at),
    ))
    .bind(updated_json)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn update_decision_status(
    tx: &mut Transaction<'_, Sqlite>,
    id: &str,
    status: DecisionStatus,
    occurred_at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let current_json: String =
        sqlx::query_scalar("SELECT payload_json FROM decision_records WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .context("decision status change refers to an unknown decision")?;
    let mut decision: Decision = serde_json::from_str(&current_json)?;
    decision.status = status;
    decision.timestamps.updated_at = Some(occurred_at);
    if status == DecisionStatus::Superseded {
        decision.timestamps.superseded_at = decision.timestamps.updated_at;
    }
    let updated_json = serde_json::to_string(&decision)?;
    sqlx::query(
        "UPDATE decision_records SET status = ?, updated_at_ms = ?, payload_json = ? WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(timestamp_millis(
        decision.timestamps.updated_at.unwrap_or(occurred_at),
    ))
    .bind(updated_json)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn ensure_same_payload(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    id: &str,
    expected: &str,
) -> anyhow::Result<()> {
    let sql = format!("SELECT payload_json FROM {table} WHERE id = ?");
    let existing: String = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
    if existing != expected {
        anyhow::bail!("record {id} is immutable and already has different content");
    }
    Ok(())
}

pub(super) async fn get_json_record<T: DeserializeOwned>(
    pool: &sqlx::SqlitePool,
    sql: &'static str,
    id: &str,
) -> anyhow::Result<Option<T>> {
    let payload = sqlx::query_scalar::<_, String>(sql)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    payload
        .map(|payload| serde_json::from_str(&payload).context("decode provenance record"))
        .transpose()
}

pub(super) fn decode_rows<T: DeserializeOwned>(rows: Vec<SqliteRow>) -> anyhow::Result<Vec<T>> {
    rows.into_iter()
        .map(|row| {
            let payload: String = row.try_get("payload_json")?;
            serde_json::from_str(&payload).context("decode provenance record")
        })
        .collect()
}

pub(super) fn ensure_id(id: &str, entity: &str) -> anyhow::Result<()> {
    if id.trim().is_empty() {
        anyhow::bail!("{entity} id must not be empty");
    }
    Ok(())
}

pub(super) fn query_limit(limit: usize) -> i64 {
    limit.clamp(1, crate::decision_provenance::MAX_QUERY_RESULTS) as i64
}

pub(super) fn timestamp_millis(timestamp: DateTime<Utc>) -> i64 {
    timestamp.timestamp_millis()
}

pub(super) fn from_timestamp_millis(value: i64) -> anyhow::Result<DateTime<Utc>> {
    Utc.timestamp_millis_opt(value)
        .single()
        .context("invalid provenance timestamp")
}

pub(super) fn parse_actor(value: &str) -> anyhow::Result<Actor> {
    match value {
        "user" => Ok(Actor::User),
        "agent" => Ok(Actor::Agent),
        "system" => Ok(Actor::System),
        "collaborative" => Ok(Actor::Collaborative),
        other => anyhow::bail!("unknown provenance actor {other:?}"),
    }
}

pub(super) fn parse_privacy(
    value: &str,
) -> anyhow::Result<crate::decision_provenance::PrivacyClass> {
    match value {
        "public" => Ok(crate::decision_provenance::PrivacyClass::Public),
        "private" => Ok(crate::decision_provenance::PrivacyClass::Private),
        "sensitive" => Ok(crate::decision_provenance::PrivacyClass::Sensitive),
        other => anyhow::bail!("unknown provenance privacy class {other:?}"),
    }
}
