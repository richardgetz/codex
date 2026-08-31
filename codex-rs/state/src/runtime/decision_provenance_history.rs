//! Historical and causal traversal for decision provenance.

use super::StateRuntime;
use super::decision_provenance_storage::decode_rows;
use super::decision_provenance_storage::from_timestamp_millis;
use super::decision_provenance_storage::get_json_record;
use super::decision_provenance_storage::parse_actor;
use super::decision_provenance_storage::parse_privacy;
use super::decision_provenance_storage::timestamp_millis;
use crate::decision_provenance::Actor;
use crate::decision_provenance::Authority;
use crate::decision_provenance::ChangeSet;
use crate::decision_provenance::Crossroad;
use crate::decision_provenance::Decision;
use crate::decision_provenance::DecisionStatus;
use crate::decision_provenance::DecisionWhy;
use crate::decision_provenance::EntityType;
use crate::decision_provenance::EventSummary;
use crate::decision_provenance::PreferenceBoundary;
use crate::decision_provenance::PreferenceKind;
use crate::decision_provenance::ProvenanceEventPayload;
use crate::decision_provenance::ProvenanceRelationship;
use crate::decision_provenance::Warrant;
use anyhow::Context;
use chrono::DateTime;
use chrono::Utc;
use sqlx::Row;
use std::collections::HashSet;

impl StateRuntime {
    pub async fn decision_history(&self, id: &str) -> anyhow::Result<Vec<EventSummary>> {
        self.event_history_until(EntityType::Decision, id, None)
            .await
    }

    pub async fn boundary_history(&self, id: &str) -> anyhow::Result<Vec<EventSummary>> {
        self.event_history_until(EntityType::PreferenceBoundary, id, None)
            .await
    }

    async fn event_history_until(
        &self,
        entity_type: EntityType,
        id: &str,
        at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Vec<EventSummary>> {
        let rows = sqlx::query(
            r#"
SELECT event_id, idempotency_key, event_type, aggregate_type, aggregate_id,
       occurred_at_ms, actor, privacy_class
FROM provenance_events
WHERE aggregate_type = ? AND aggregate_id = ?
  AND (? IS NULL OR occurred_at_ms <= ?)
ORDER BY occurred_at_ms ASC, event_id ASC
LIMIT ?
            "#,
        )
        .bind(entity_type.as_str())
        .bind(id)
        .bind(at.map(timestamp_millis))
        .bind(at.map(timestamp_millis))
        .bind(crate::decision_provenance::MAX_QUERY_RESULTS as i64 + 1)
        .fetch_all(self.pool.as_ref())
        .await?;
        if rows.len() > crate::decision_provenance::MAX_QUERY_RESULTS {
            anyhow::bail!(
                "provenance history for {}:{id} exceeds the {}-event traversal limit",
                entity_type.as_str(),
                crate::decision_provenance::MAX_QUERY_RESULTS
            );
        }
        rows.into_iter()
            .map(|row| {
                Ok(EventSummary {
                    event_id: row.try_get("event_id")?,
                    idempotency_key: row.try_get("idempotency_key")?,
                    event_type: row.try_get("event_type")?,
                    aggregate_type: row.try_get("aggregate_type")?,
                    aggregate_id: row.try_get("aggregate_id")?,
                    occurred_at: from_timestamp_millis(row.try_get("occurred_at_ms")?)?,
                    actor: parse_actor(row.try_get::<String, _>("actor")?.as_str())?,
                    privacy: parse_privacy(row.try_get::<String, _>("privacy_class")?.as_str())?,
                })
            })
            .collect()
    }

    pub async fn relationships_for(
        &self,
        entity_type: EntityType,
        id: &str,
    ) -> anyhow::Result<Vec<ProvenanceRelationship>> {
        self.relationships_for_until(entity_type, id, None).await
    }

    async fn relationships_for_until(
        &self,
        entity_type: EntityType,
        id: &str,
        at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Vec<ProvenanceRelationship>> {
        let rows = sqlx::query(
            r#"
SELECT payload_json
FROM provenance_relationships
WHERE ((from_type = ? AND from_id = ?) OR (to_type = ? AND to_id = ?))
  AND (? IS NULL OR created_at_ms <= ?)
ORDER BY created_at_ms ASC, id ASC
LIMIT ?
            "#,
        )
        .bind(entity_type.as_str())
        .bind(id)
        .bind(entity_type.as_str())
        .bind(id)
        .bind(at.map(timestamp_millis))
        .bind(at.map(timestamp_millis))
        .bind(crate::decision_provenance::MAX_QUERY_RESULTS as i64 + 1)
        .fetch_all(self.pool.as_ref())
        .await?;
        if rows.len() > crate::decision_provenance::MAX_QUERY_RESULTS {
            anyhow::bail!(
                "provenance relationships for {}:{id} exceed the {}-relationship traversal limit",
                entity_type.as_str(),
                crate::decision_provenance::MAX_QUERY_RESULTS
            );
        }
        decode_rows(rows)
    }

    pub async fn decision_why(&self, id: &str) -> anyhow::Result<Option<DecisionWhy>> {
        let Some(decision) = self.get_decision(id).await? else {
            return Ok(None);
        };
        let crossroad = match decision.parent_crossroad_id.as_deref() {
            Some(crossroad_id) => self.get_crossroad(crossroad_id).await?,
            None => None,
        };
        let boundaries = self.decision_boundaries(&decision, None).await?;
        let warrant = match decision.warrant_id.as_deref() {
            Some(warrant_id) => {
                get_json_record(
                    &self.pool,
                    "SELECT payload_json FROM decision_warrants WHERE id = ?",
                    warrant_id,
                )
                .await?
            }
            None => None,
        };
        let change_sets = self.decision_artifacts(id).await?;
        let relationships = self.decision_why_relationships(&decision, None).await?;
        let history = self.decision_history(id).await?;
        Ok(Some(DecisionWhy {
            decision,
            crossroad,
            boundaries,
            warrant,
            change_sets,
            relationships,
            history,
        }))
    }

    /// Reconstruct the decision-relevant view that was visible at a point in time from the
    /// append-only event stream. This never rewrites the current materialized records.
    pub async fn decision_why_at(
        &self,
        id: &str,
        at: DateTime<Utc>,
    ) -> anyhow::Result<Option<DecisionWhy>> {
        let Some(decision) = self.decision_at(id, at).await? else {
            return Ok(None);
        };
        let crossroad = match decision.parent_crossroad_id.as_deref() {
            Some(crossroad_id) => self.crossroad_at(crossroad_id, at).await?,
            None => None,
        };
        let boundaries = self.decision_boundaries(&decision, Some(at)).await?;
        let warrant = match decision.warrant_id.as_deref() {
            Some(warrant_id) => self.warrant_at(warrant_id, at).await?,
            None => None,
        };
        let change_sets = self.decision_artifacts_until(id, at).await?;
        let relationships = self.decision_why_relationships(&decision, Some(at)).await?;
        let history = self
            .event_history_until(EntityType::Decision, id, Some(at))
            .await?;
        Ok(Some(DecisionWhy {
            decision,
            crossroad,
            boundaries,
            warrant,
            change_sets,
            relationships,
            history,
        }))
    }

    async fn decision_why_relationships(
        &self,
        decision: &Decision,
        at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Vec<ProvenanceRelationship>> {
        let mut relationships = self
            .relationships_for_until(EntityType::Decision, &decision.id, at)
            .await?;
        if relationships.len() > crate::decision_provenance::MAX_QUERY_RESULTS {
            anyhow::bail!(
                "causal traversal for decision {} exceeds the {}-relationship limit",
                decision.id,
                crate::decision_provenance::MAX_QUERY_RESULTS
            );
        }
        if decision.related_preference_boundary_ids.len()
            > crate::decision_provenance::MAX_QUERY_RESULTS
        {
            anyhow::bail!(
                "causal traversal for decision {} exceeds the {}-boundary limit",
                decision.id,
                crate::decision_provenance::MAX_QUERY_RESULTS
            );
        }
        for boundary_id in &decision.related_preference_boundary_ids {
            let boundary_relationships = self
                .relationships_for_until(EntityType::PreferenceBoundary, boundary_id, at)
                .await?;
            for relationship in boundary_relationships {
                if relationship.from_type != EntityType::PreferenceBoundary
                    || relationship.from_id != *boundary_id
                    || relationship.to_type != EntityType::PreferenceBoundary
                {
                    continue;
                }
                if !relationships
                    .iter()
                    .any(|existing| existing.id == relationship.id)
                {
                    if relationships.len() >= crate::decision_provenance::MAX_QUERY_RESULTS {
                        anyhow::bail!(
                            "causal traversal for decision {} exceeds the {}-relationship limit",
                            decision.id,
                            crate::decision_provenance::MAX_QUERY_RESULTS
                        );
                    }
                    relationships.push(relationship);
                }
            }
        }
        if relationships.len() > crate::decision_provenance::MAX_QUERY_RESULTS {
            anyhow::bail!(
                "causal traversal for decision {} exceeds the {}-relationship limit",
                decision.id,
                crate::decision_provenance::MAX_QUERY_RESULTS
            );
        }
        Ok(relationships)
    }

    async fn decision_boundaries(
        &self,
        decision: &Decision,
        at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Vec<PreferenceBoundary>> {
        let mut boundaries = Vec::new();
        let mut seen = HashSet::new();
        if decision.related_preference_boundary_ids.len()
            > crate::decision_provenance::MAX_QUERY_RESULTS
        {
            anyhow::bail!(
                "boundary traversal for decision {} exceeds the {}-boundary limit",
                decision.id,
                crate::decision_provenance::MAX_QUERY_RESULTS
            );
        }
        for boundary_id in &decision.related_preference_boundary_ids {
            let Some(mut boundary) = (match at {
                Some(at) => self.boundary_at(boundary_id, at).await?,
                None => self.get_preference_boundary(boundary_id).await?,
            }) else {
                continue;
            };
            loop {
                if !seen.insert(boundary.id.clone()) {
                    break;
                }
                if boundaries.len() >= crate::decision_provenance::MAX_QUERY_RESULTS {
                    anyhow::bail!(
                        "boundary traversal for decision {} exceeds the {}-boundary limit",
                        decision.id,
                        crate::decision_provenance::MAX_QUERY_RESULTS
                    );
                }
                let superseded_by = boundary.superseded_by.clone();
                boundaries.push(boundary);
                let Some(superseded_by) = superseded_by else {
                    break;
                };
                boundary = match at {
                    Some(at) => match self.boundary_at(&superseded_by, at).await? {
                        Some(boundary) => boundary,
                        None => break,
                    },
                    None => match self.get_preference_boundary(&superseded_by).await? {
                        Some(boundary) => boundary,
                        None => break,
                    },
                };
            }
        }
        Ok(boundaries)
    }

    async fn decision_at(&self, id: &str, at: DateTime<Utc>) -> anyhow::Result<Option<Decision>> {
        let mut decision = None;
        for (occurred_at, payload) in self
            .provenance_payloads_until(EntityType::Decision, id, Some(at))
            .await?
        {
            match payload {
                ProvenanceEventPayload::Decision(value) => decision = Some(value),
                ProvenanceEventPayload::DecisionStatus {
                    id: status_id,
                    status,
                } if status_id == id => {
                    if let Some(value) = decision.as_mut() {
                        value.status = status;
                        value.timestamps.updated_at = Some(occurred_at);
                        if status == DecisionStatus::Superseded {
                            value.timestamps.superseded_at = Some(occurred_at);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(decision)
    }

    async fn crossroad_at(&self, id: &str, at: DateTime<Utc>) -> anyhow::Result<Option<Crossroad>> {
        let mut crossroad = None;
        for (occurred_at, payload) in self
            .provenance_payloads_until(EntityType::Crossroad, id, Some(at))
            .await?
        {
            match payload {
                ProvenanceEventPayload::Crossroad(value) => crossroad = Some(value),
                ProvenanceEventPayload::CrossroadStatus {
                    id: status_id,
                    status,
                } if status_id == id => {
                    if let Some(value) = crossroad.as_mut() {
                        value.status = status;
                        value.timestamps.updated_at = Some(occurred_at);
                    }
                }
                _ => {}
            }
        }
        Ok(crossroad)
    }

    async fn boundary_at(
        &self,
        id: &str,
        at: DateTime<Utc>,
    ) -> anyhow::Result<Option<PreferenceBoundary>> {
        let mut boundary = None;
        for (occurred_at, payload) in self
            .provenance_payloads_until(EntityType::PreferenceBoundary, id, Some(at))
            .await?
        {
            match payload {
                ProvenanceEventPayload::PreferenceBoundary(value) => boundary = Some(value),
                ProvenanceEventPayload::BoundaryTransition(transition)
                    if transition.boundary_id == id =>
                {
                    if let Some(value) = boundary.as_mut() {
                        value.lifecycle_status = transition.transition.status();
                        value.timestamps.updated_at = Some(occurred_at);
                        if matches!(
                            transition.transition,
                            crate::decision_provenance::BoundaryTransitionKind::Confirm
                                | crate::decision_provenance::BoundaryTransitionKind::Activate
                        ) && matches!(transition.actor, Actor::User | Actor::Collaborative)
                        {
                            if value.kind == PreferenceKind::CandidatePreference {
                                value.kind = PreferenceKind::PreferenceBoundary;
                                value.strength =
                                    crate::decision_provenance::PreferenceStrength::Confirmation;
                            }
                            if value.authority == Authority::Agent {
                                value.authority = Authority::User;
                            }
                            value.confidence = None;
                            value.timestamps.effective_at = Some(occurred_at);
                        }
                        if matches!(
                            transition.transition,
                            crate::decision_provenance::BoundaryTransitionKind::Narrow
                                | crate::decision_provenance::BoundaryTransitionKind::Broaden
                                | crate::decision_provenance::BoundaryTransitionKind::Supersede
                        ) {
                            value.timestamps.superseded_at = Some(occurred_at);
                        }
                        if let Some(replacement) = transition.replacement {
                            value.superseded_by = Some(replacement.id);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(boundary)
    }

    async fn warrant_at(&self, id: &str, at: DateTime<Utc>) -> anyhow::Result<Option<Warrant>> {
        let mut warrant = None;
        for (_, payload) in self
            .provenance_payloads_until(EntityType::Warrant, id, Some(at))
            .await?
        {
            if let ProvenanceEventPayload::Warrant(value) = payload {
                warrant = Some(value);
            }
        }
        Ok(warrant)
    }

    async fn provenance_payloads_until(
        &self,
        entity_type: EntityType,
        id: &str,
        at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Vec<(DateTime<Utc>, ProvenanceEventPayload)>> {
        let rows = sqlx::query(
            r#"
SELECT occurred_at_ms, payload_json
FROM provenance_events
WHERE aggregate_type = ? AND aggregate_id = ?
  AND (? IS NULL OR occurred_at_ms <= ?)
ORDER BY occurred_at_ms ASC, event_id ASC
LIMIT ?
            "#,
        )
        .bind(entity_type.as_str())
        .bind(id)
        .bind(at.map(timestamp_millis))
        .bind(at.map(timestamp_millis))
        .bind(crate::decision_provenance::MAX_QUERY_RESULTS as i64 + 1)
        .fetch_all(self.pool.as_ref())
        .await?;
        if rows.len() > crate::decision_provenance::MAX_QUERY_RESULTS {
            anyhow::bail!(
                "provenance replay for {}:{id} exceeds the {}-event traversal limit",
                entity_type.as_str(),
                crate::decision_provenance::MAX_QUERY_RESULTS
            );
        }
        rows.into_iter()
            .map(|row| {
                let occurred_at = from_timestamp_millis(row.try_get("occurred_at_ms")?)?;
                let payload =
                    serde_json::from_str(row.try_get::<String, _>("payload_json")?.as_str())
                        .context("decode provenance event payload")?;
                Ok((occurred_at, payload))
            })
            .collect()
    }

    async fn decision_artifacts_until(
        &self,
        id: &str,
        at: DateTime<Utc>,
    ) -> anyhow::Result<Vec<ChangeSet>> {
        let change_set_ids = self
            .decision_at(id, at)
            .await?
            .map(|decision| decision.change_set_ids)
            .unwrap_or_default();
        if change_set_ids.len() > crate::decision_provenance::MAX_QUERY_RESULTS {
            anyhow::bail!(
                "change-set traversal for decision {id} exceeds the {}-record limit",
                crate::decision_provenance::MAX_QUERY_RESULTS
            );
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT payload_json FROM decision_change_sets WHERE created_at_ms <= ",
        );
        query
            .push_bind(timestamp_millis(at))
            .push(" AND (decision_id = ")
            .push_bind(id);
        if !change_set_ids.is_empty() {
            query.push(" OR id IN (");
            let mut separated = query.separated(", ");
            for change_set_id in &change_set_ids {
                separated.push_bind(change_set_id);
            }
            separated.push_unseparated(")");
        }
        query
            .push(") ORDER BY created_at_ms, id LIMIT ")
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
}
