//! SQLite-backed decision provenance operations.

#[path = "decision_provenance_history.rs"]
mod decision_provenance_history;
#[path = "decision_provenance_projection_runtime.rs"]
mod decision_provenance_projection_runtime;
#[path = "decision_provenance_storage.rs"]
mod decision_provenance_storage;

use self::decision_provenance_storage::decode_rows;
use self::decision_provenance_storage::ensure_id;
use self::decision_provenance_storage::event_for;
use self::decision_provenance_storage::get_json_record;
use self::decision_provenance_storage::insert_event_row;
use self::decision_provenance_storage::materialize_event;
use self::decision_provenance_storage::query_limit;
use self::decision_provenance_storage::validate_boundary_transition;
use self::decision_provenance_storage::validate_event;
use super::StateRuntime;
use crate::decision_provenance::Actor;
use crate::decision_provenance::AppendProvenanceEventResult;
use crate::decision_provenance::BoundaryTransition;
use crate::decision_provenance::ChangeSet;
use crate::decision_provenance::Crossroad;
use crate::decision_provenance::CrossroadFilter;
use crate::decision_provenance::CrossroadStatus;
use crate::decision_provenance::Decision;
use crate::decision_provenance::DecisionFilter;
use crate::decision_provenance::DecisionStatus;
use crate::decision_provenance::EntityType;
use crate::decision_provenance::PreferenceBoundary;
use crate::decision_provenance::PreferenceBoundaryFilter;
use crate::decision_provenance::PreferenceBoundaryPreflight;
use crate::decision_provenance::PreferenceKind;
use crate::decision_provenance::PreferenceStrength;
use crate::decision_provenance::ProvenanceEvent;
use crate::decision_provenance::ProvenanceEventPayload;
use crate::decision_provenance::ProvenanceEventType;
use crate::decision_provenance::ProvenanceNotification;
use crate::decision_provenance::ProvenanceRelationship;
use crate::decision_provenance::ProvenanceWriteOptions;
use crate::decision_provenance::ScopeRef;
use crate::decision_provenance::Warrant;
use crate::decision_provenance::sanitize_event;
use anyhow::Context;
use sqlx::QueryBuilder;
use sqlx::Sqlite;
use std::path::PathBuf;

const MIN_PREFIX_QUERY_LIMIT: usize = 2;

impl StateRuntime {
    /// Return the versioned, read-only projection path intended for local consumers such as
    /// Inbound. The projection is derived from this runtime's configured state home.
    pub fn provenance_projection_path(&self) -> PathBuf {
        self.sqlite
            .home()
            .join(crate::decision_provenance::PROVENANCE_PROJECTION_DIRECTORY)
            .join(crate::decision_provenance::PROVENANCE_PROJECTION_FILENAME)
    }

    /// Append an immutable provenance event and update its indexed materialized record.
    ///
    /// Event insertion and materialization share one SQLite transaction. A repeated event id or
    /// idempotency key is accepted only when its immutable payload is identical.
    pub async fn append_provenance_event(
        &self,
        mut event: ProvenanceEvent,
    ) -> anyhow::Result<AppendProvenanceEventResult> {
        ensure_id(&event.event_id, "event")?;
        ensure_id(&event.aggregate_id, "aggregate")?;
        sanitize_event(&mut event);
        validate_event(&event)?;
        let payload_json = serde_json::to_string(&event.payload)?;
        let mut tx = self.pool.begin().await?;
        let insert_outcome = insert_event_row(&mut tx, &event, &payload_json).await?;
        if !insert_outcome.inserted {
            tx.rollback().await?;
            return Ok(AppendProvenanceEventResult {
                event_id: insert_outcome.event_id,
                inserted: false,
                projection_path: Some(self.provenance_projection_path()),
            });
        }

        materialize_event(&mut tx, &event).await?;
        tx.commit().await?;

        let projection_path = match self.rebuild_provenance_projection().await {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::warn!("failed to refresh decision provenance projection: {err:#}");
                None
            }
        };
        Ok(AppendProvenanceEventResult {
            event_id: event.event_id,
            inserted: true,
            projection_path,
        })
    }

    /// Record a preference boundary or a candidate preference without copying the memory record.
    pub async fn record_preference_boundary(
        &self,
        boundary: PreferenceBoundary,
        options: ProvenanceWriteOptions,
    ) -> anyhow::Result<AppendProvenanceEventResult> {
        let event = event_for(
            ProvenanceEventType::PreferenceBoundaryRecorded,
            EntityType::PreferenceBoundary,
            boundary.id.clone(),
            boundary.privacy,
            options,
            ProvenanceEventPayload::PreferenceBoundary(boundary),
        );
        self.append_provenance_event(event).await
    }

    /// Append a boundary lifecycle event. A replacement is a new canonical boundary record linked
    /// from the old record; the old boundary is never overwritten or deleted.
    pub async fn transition_preference_boundary(
        &self,
        mut transition: BoundaryTransition,
        options: ProvenanceWriteOptions,
    ) -> anyhow::Result<AppendProvenanceEventResult> {
        if transition.actor != options.actor {
            anyhow::bail!(
                "preference boundary transition actor must match the provenance write actor"
            );
        }
        ensure_id(&transition.boundary_id, "preference boundary")?;
        let current = self
            .get_preference_boundary(&transition.boundary_id)
            .await?
            .context("preference boundary transition refers to an unknown boundary")?;
        validate_boundary_transition(&current, &transition)?;
        if matches!(
            transition.transition,
            crate::decision_provenance::BoundaryTransitionKind::Narrow
                | crate::decision_provenance::BoundaryTransitionKind::Broaden
                | crate::decision_provenance::BoundaryTransitionKind::Supersede
        ) && transition.replacement.is_none()
        {
            anyhow::bail!(
                "{} boundary transitions require a replacement boundary with a new stable id",
                transition.transition.as_str()
            );
        }
        if let Some(replacement) = transition.replacement.as_mut() {
            match transition.actor {
                Actor::System => {}
                Actor::Agent => {
                    replacement.kind = PreferenceKind::CandidatePreference;
                    replacement.strength = PreferenceStrength::Soft;
                    replacement.authority = crate::decision_provenance::Authority::Agent;
                    replacement.lifecycle_status =
                        crate::decision_provenance::LifecycleStatus::Candidate;
                    replacement.confidence =
                        replacement.confidence.map(|confidence| confidence.min(100));
                    replacement.timestamps.effective_at = None;
                }
                Actor::User | Actor::Collaborative => {
                    replacement.kind = PreferenceKind::PreferenceBoundary;
                    replacement.strength = PreferenceStrength::Confirmation;
                    replacement.authority = crate::decision_provenance::Authority::User;
                    replacement.lifecycle_status = if replacement.lifecycle_status.is_active() {
                        replacement.lifecycle_status
                    } else {
                        crate::decision_provenance::LifecycleStatus::Active
                    };
                    replacement.confidence = None;
                    replacement.timestamps.effective_at = replacement
                        .timestamps
                        .effective_at
                        .or(Some(replacement.timestamps.created_at));
                }
            }
        }
        let event = event_for(
            ProvenanceEventType::PreferenceBoundaryTransitioned,
            EntityType::PreferenceBoundary,
            transition.boundary_id.clone(),
            transition.replacement.as_ref().map_or(
                crate::decision_provenance::PrivacyClass::Private,
                |boundary| boundary.privacy,
            ),
            options,
            ProvenanceEventPayload::BoundaryTransition(transition),
        );
        self.append_provenance_event(event).await
    }

    /// Record a crossroad. It is intentionally explicit so low-risk choices do not create
    /// interruptive provenance records automatically.
    pub async fn record_crossroad(
        &self,
        crossroad: Crossroad,
        options: ProvenanceWriteOptions,
    ) -> anyhow::Result<AppendProvenanceEventResult> {
        let event = event_for(
            ProvenanceEventType::CrossroadRecorded,
            EntityType::Crossroad,
            crossroad.id.clone(),
            crossroad.privacy,
            options,
            ProvenanceEventPayload::Crossroad(crossroad),
        );
        self.append_provenance_event(event).await
    }

    /// Record a selected path. Related boundary IDs are indexed as `influenced_by` links without
    /// copying their statements into the decision record.
    pub async fn record_decision(
        &self,
        decision: Decision,
        options: ProvenanceWriteOptions,
    ) -> anyhow::Result<AppendProvenanceEventResult> {
        let event = event_for(
            ProvenanceEventType::DecisionRecorded,
            EntityType::Decision,
            decision.id.clone(),
            decision.privacy,
            options,
            ProvenanceEventPayload::Decision(decision),
        );
        self.append_provenance_event(event).await
    }

    pub async fn record_warrant(
        &self,
        warrant: Warrant,
        options: ProvenanceWriteOptions,
    ) -> anyhow::Result<AppendProvenanceEventResult> {
        let event = event_for(
            ProvenanceEventType::WarrantRecorded,
            EntityType::Warrant,
            warrant.id.clone(),
            warrant.privacy,
            options,
            ProvenanceEventPayload::Warrant(warrant),
        );
        self.append_provenance_event(event).await
    }

    pub async fn link_change_set(
        &self,
        change_set: ChangeSet,
        options: ProvenanceWriteOptions,
    ) -> anyhow::Result<AppendProvenanceEventResult> {
        let event = event_for(
            ProvenanceEventType::ChangeSetLinked,
            EntityType::ChangeSet,
            change_set.id.clone(),
            change_set.privacy,
            options,
            ProvenanceEventPayload::ChangeSet(change_set),
        );
        self.append_provenance_event(event).await
    }

    pub async fn record_relationship(
        &self,
        relationship: ProvenanceRelationship,
        options: ProvenanceWriteOptions,
    ) -> anyhow::Result<AppendProvenanceEventResult> {
        let event = event_for(
            ProvenanceEventType::RelationshipRecorded,
            EntityType::Relationship,
            relationship.id.clone(),
            relationship.privacy,
            options,
            ProvenanceEventPayload::Relationship(relationship),
        );
        self.append_provenance_event(event).await
    }

    pub async fn record_notification(
        &self,
        notification: ProvenanceNotification,
        options: ProvenanceWriteOptions,
    ) -> anyhow::Result<AppendProvenanceEventResult> {
        let event = event_for(
            ProvenanceEventType::NotificationRecorded,
            EntityType::Notification,
            notification.id.clone(),
            notification.privacy,
            options,
            ProvenanceEventPayload::Notification(notification),
        );
        self.append_provenance_event(event).await
    }

    pub async fn transition_crossroad(
        &self,
        id: &str,
        status: CrossroadStatus,
        options: ProvenanceWriteOptions,
    ) -> anyhow::Result<AppendProvenanceEventResult> {
        ensure_id(id, "crossroad")?;
        let event = event_for(
            ProvenanceEventType::CrossroadStatusChanged,
            EntityType::Crossroad,
            id.to_string(),
            crate::decision_provenance::PrivacyClass::Private,
            options,
            ProvenanceEventPayload::CrossroadStatus {
                id: id.to_string(),
                status,
            },
        );
        self.append_provenance_event(event).await
    }

    pub async fn transition_decision(
        &self,
        id: &str,
        status: DecisionStatus,
        options: ProvenanceWriteOptions,
    ) -> anyhow::Result<AppendProvenanceEventResult> {
        ensure_id(id, "decision")?;
        let event = event_for(
            ProvenanceEventType::DecisionStatusChanged,
            EntityType::Decision,
            id.to_string(),
            crate::decision_provenance::PrivacyClass::Private,
            options,
            ProvenanceEventPayload::DecisionStatus {
                id: id.to_string(),
                status,
            },
        );
        self.append_provenance_event(event).await
    }

    pub async fn get_decision(&self, id: &str) -> anyhow::Result<Option<Decision>> {
        get_json_record(
            &self.pool,
            "SELECT payload_json FROM decision_records WHERE id = ?",
            id,
        )
        .await
    }

    pub async fn get_crossroad(&self, id: &str) -> anyhow::Result<Option<Crossroad>> {
        get_json_record(
            &self.pool,
            "SELECT payload_json FROM crossroads WHERE id = ?",
            id,
        )
        .await
    }

    pub async fn crossroads_with_id_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Crossroad>> {
        let limit = prefix_query_limit(prefix, limit)?;
        let rows = sqlx::query(
            "SELECT payload_json FROM crossroads WHERE substr(id, 1, length(?)) = ? COLLATE BINARY ORDER BY updated_at_ms DESC, id DESC LIMIT ?",
        )
        .bind(prefix)
        .bind(prefix)
        .bind(limit)
        .fetch_all(self.pool.as_ref())
        .await?;
        decode_rows(rows)
    }

    pub async fn get_preference_boundary(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<PreferenceBoundary>> {
        get_json_record(
            &self.pool,
            "SELECT payload_json FROM preference_boundaries WHERE id = ?",
            id,
        )
        .await
    }

    pub async fn get_preference_boundary_for_memory_record(
        &self,
        memory_record_id: &str,
    ) -> anyhow::Result<Option<PreferenceBoundary>> {
        let payload = sqlx::query_scalar::<_, String>(
            "SELECT payload_json FROM preference_boundaries WHERE related_memory_record_id = ? ORDER BY updated_at_ms DESC, id DESC LIMIT 1",
        )
        .bind(memory_record_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        payload
            .map(|payload| serde_json::from_str(&payload).context("decode provenance record"))
            .transpose()
    }

    pub async fn list_decisions(&self, filter: DecisionFilter) -> anyhow::Result<Vec<Decision>> {
        let limit = query_limit(filter.limit);
        let mut query =
            QueryBuilder::<Sqlite>::new("SELECT payload_json FROM decision_records WHERE 1 = 1");
        if let Some(status) = filter.status {
            query.push(" AND status = ").push_bind(status.as_str());
        }
        if let Some(actor) = filter.actor {
            query.push(" AND actor = ").push_bind(actor.as_str());
        }
        if let Some(session_id) = filter.session_id {
            query
                .push(" AND source_session_id = ")
                .push_bind(session_id);
        }
        if let Some(repository) = filter.repository {
            query.push(" AND repository = ").push_bind(repository);
        }
        if let Some(project_ref) = filter.project_ref {
            query.push(" AND project_ref = ").push_bind(project_ref);
        }
        if let Some(text) = filter.text {
            query
                .push(" AND payload_json LIKE ")
                .push_bind(format!("%{text}%"));
        }
        query
            .push(" ORDER BY updated_at_ms DESC, id DESC LIMIT ")
            .push_bind(limit);
        decode_rows(query.build().fetch_all(self.pool.as_ref()).await?)
    }

    pub async fn decisions_with_id_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Decision>> {
        let limit = prefix_query_limit(prefix, limit)?;
        let rows = sqlx::query(
            "SELECT payload_json FROM decision_records WHERE substr(id, 1, length(?)) = ? COLLATE BINARY ORDER BY updated_at_ms DESC, id DESC LIMIT ?",
        )
        .bind(prefix)
        .bind(prefix)
        .bind(limit)
        .fetch_all(self.pool.as_ref())
        .await?;
        decode_rows(rows)
    }

    pub async fn list_crossroads(&self, filter: CrossroadFilter) -> anyhow::Result<Vec<Crossroad>> {
        let limit = query_limit(filter.limit);
        let mut query =
            QueryBuilder::<Sqlite>::new("SELECT payload_json FROM crossroads WHERE 1 = 1");
        if let Some(status) = filter.status {
            query.push(" AND status = ").push_bind(status.as_str());
        }
        if let Some(session_id) = filter.session_id {
            query.push(" AND session_id = ").push_bind(session_id);
        }
        if let Some(project_ref) = filter.project_ref {
            query.push(" AND project_ref = ").push_bind(project_ref);
        }
        query
            .push(" ORDER BY updated_at_ms DESC, id DESC LIMIT ")
            .push_bind(limit);
        decode_rows(query.build().fetch_all(self.pool.as_ref()).await?)
    }

    pub async fn list_open_crossroads(&self, limit: usize) -> anyhow::Result<Vec<Crossroad>> {
        let rows = sqlx::query(
            "SELECT payload_json FROM crossroads WHERE status IN ('open', 'reopened') ORDER BY updated_at_ms DESC, id DESC LIMIT ?",
        )
        .bind(query_limit(limit))
        .fetch_all(self.pool.as_ref())
        .await?;
        decode_rows(rows)
    }

    pub async fn list_preference_boundaries(
        &self,
        filter: PreferenceBoundaryFilter,
    ) -> anyhow::Result<Vec<PreferenceBoundary>> {
        let limit = query_limit(filter.limit);
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT payload_json FROM preference_boundaries WHERE 1 = 1",
        );
        if let Some(scope) = filter.scope {
            query
                .push(" AND scope_kind = ")
                .push_bind(scope.kind.as_str())
                .push(" AND scope_id = ")
                .push_bind(scope.id);
        }
        if let Some(status) = filter.lifecycle_status {
            query
                .push(" AND lifecycle_status = ")
                .push_bind(status.as_str());
        }
        if let Some(text) = filter.text {
            query
                .push(" AND payload_json LIKE ")
                .push_bind(format!("%{text}%"));
        }
        query
            .push(" ORDER BY updated_at_ms DESC, id DESC LIMIT ")
            .push_bind(limit);
        decode_rows(query.build().fetch_all(self.pool.as_ref()).await?)
    }

    pub async fn active_preference_boundaries(
        &self,
        scope: ScopeRef,
    ) -> anyhow::Result<Vec<PreferenceBoundary>> {
        let rows = sqlx::query(
            "SELECT payload_json FROM preference_boundaries WHERE scope_kind = ? AND scope_id = ? AND lifecycle_status IN ('active', 'confirmed') ORDER BY updated_at_ms DESC, id DESC LIMIT ?",
        )
        .bind(scope.kind.as_str())
        .bind(scope.id)
        .bind(crate::decision_provenance::MAX_QUERY_RESULTS as i64)
        .fetch_all(self.pool.as_ref())
        .await?;
        decode_rows(rows)
    }

    /// Read active boundaries and unconfirmed candidates that apply to a turn scope.
    ///
    /// Global boundaries are included for every scoped preflight. Candidates are returned
    /// separately so callers can consider them without treating an agent inference as durable
    /// user authority. This query is intentionally read-only; conflict handling and any user
    /// confirmation remain explicit lifecycle operations.
    pub async fn preflight_preference_boundaries(
        &self,
        scope: ScopeRef,
    ) -> anyhow::Result<PreferenceBoundaryPreflight> {
        self.preflight_preference_boundaries_for_scopes(scope.clone(), &[scope])
            .await
    }

    /// Read active boundaries and unconfirmed candidates across global and explicitly selected
    /// scopes. The caller supplies the display scope separately so a multi-scope preflight still
    /// reports the task or request it belongs to without copying any request text.
    pub async fn preflight_preference_boundaries_for_scopes(
        &self,
        display_scope: ScopeRef,
        scopes: &[ScopeRef],
    ) -> anyhow::Result<PreferenceBoundaryPreflight> {
        let mut query =
            QueryBuilder::<Sqlite>::new("SELECT payload_json FROM preference_boundaries WHERE ");
        query.push("(scope_kind = 'global'");
        for scope in scopes
            .iter()
            .filter(|scope| !matches!(scope.kind, crate::decision_provenance::Scope::Global))
        {
            query
                .push(" OR (scope_kind = ")
                .push_bind(scope.kind.as_str())
                .push(" AND scope_id = ")
                .push_bind(scope.id.clone())
                .push(")");
        }
        query.push(") AND lifecycle_status IN ('active', 'confirmed', 'candidate')");
        query
            .push(" ORDER BY updated_at_ms DESC, id DESC LIMIT ")
            .push_bind(crate::decision_provenance::MAX_QUERY_RESULTS as i64 + 1);
        let boundaries: Vec<PreferenceBoundary> =
            decode_rows(query.build().fetch_all(self.pool.as_ref()).await?)?;
        if boundaries.len() > crate::decision_provenance::MAX_QUERY_RESULTS {
            anyhow::bail!(
                "preference preflight exceeds the {}-record limit",
                crate::decision_provenance::MAX_QUERY_RESULTS
            );
        }
        let mut active = Vec::new();
        let mut candidates = Vec::new();
        for boundary in boundaries {
            if boundary.is_candidate() {
                candidates.push(boundary);
            } else if boundary.lifecycle_status.is_active() {
                active.push(boundary);
            }
        }
        Ok(PreferenceBoundaryPreflight {
            scope: display_scope,
            active,
            candidates,
        })
    }

    pub async fn decisions_influenced_by(
        &self,
        boundary_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Decision>> {
        let limit = query_limit(limit);
        let rows = sqlx::query(
            r#"
SELECT decisions.payload_json
FROM decision_records AS decisions
JOIN provenance_relationships AS links
  ON links.from_type = 'decision'
 AND links.from_id = decisions.id
 AND links.relation = 'influenced_by'
WHERE links.to_type = 'preference_boundary'
  AND links.to_id = ?
ORDER BY decisions.recorded_at_ms DESC, decisions.id DESC
LIMIT ?
            "#,
        )
        .bind(boundary_id)
        .bind(limit)
        .fetch_all(self.pool.as_ref())
        .await?;
        decode_rows(rows)
    }

    pub async fn decision_sessions(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let Some(decision) = self.get_decision(id).await? else {
            return Ok(Vec::new());
        };
        let mut sessions = decision.source_session_id.into_iter().collect::<Vec<_>>();
        if let Some(crossroad_id) = decision.parent_crossroad_id.as_deref()
            && let Some(crossroad) = self.get_crossroad(crossroad_id).await?
            && let Some(session_id) = crossroad.session_id
            && !sessions.iter().any(|existing| existing == &session_id)
        {
            sessions.push(session_id);
        }
        for change_set in self.decision_artifacts(id).await? {
            if let Some(session_id) = change_set.session_id
                && !sessions.iter().any(|existing| existing == &session_id)
            {
                sessions.push(session_id);
            }
        }
        Ok(sessions)
    }

    pub async fn decision_artifacts(&self, id: &str) -> anyhow::Result<Vec<ChangeSet>> {
        let change_set_ids = self
            .get_decision(id)
            .await?
            .map(|decision| decision.change_set_ids)
            .unwrap_or_default();
        if change_set_ids.len() > crate::decision_provenance::MAX_QUERY_RESULTS {
            anyhow::bail!(
                "change-set traversal for decision {id} exceeds the {}-record limit",
                crate::decision_provenance::MAX_QUERY_RESULTS
            );
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT payload_json FROM decision_change_sets WHERE decision_id = ",
        );
        query.push_bind(id);
        if !change_set_ids.is_empty() {
            query.push(" OR id IN (");
            let mut separated = query.separated(", ");
            for change_set_id in change_set_ids
                .iter()
                .take(crate::decision_provenance::MAX_QUERY_RESULTS)
            {
                separated.push_bind(change_set_id);
            }
            separated.push_unseparated(")");
        }
        query
            .push(" ORDER BY created_at_ms, id LIMIT ")
            .push_bind(crate::decision_provenance::MAX_QUERY_RESULTS as i64 + 1);
        let rows = query.build().fetch_all(self.pool.as_ref()).await?;
        if rows.len() > crate::decision_provenance::MAX_QUERY_RESULTS {
            anyhow::bail!(
                "change-set traversal for decision {id} exceeds the {}-record limit",
                crate::decision_provenance::MAX_QUERY_RESULTS
            );
        }
        decode_rows(rows)
    }

    pub async fn list_provenance_notifications(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<ProvenanceNotification>> {
        let rows = sqlx::query(
            "SELECT payload_json FROM provenance_notifications ORDER BY created_at_ms DESC, id DESC LIMIT ?",
        )
        .bind(query_limit(limit))
        .fetch_all(self.pool.as_ref())
        .await?;
        decode_rows(rows)
    }
}

fn prefix_query_limit(prefix: &str, limit: usize) -> anyhow::Result<i64> {
    if prefix.is_empty() {
        anyhow::bail!("provenance ID prefix cannot be empty");
    }
    if limit < MIN_PREFIX_QUERY_LIMIT {
        anyhow::bail!("provenance ID prefix query limit must be at least {MIN_PREFIX_QUERY_LIMIT}");
    }
    i64::try_from(limit.min(crate::decision_provenance::MAX_QUERY_RESULTS))
        .context("provenance ID prefix query limit is too large")
}

#[cfg(test)]
#[path = "decision_provenance_tests.rs"]
mod tests;
