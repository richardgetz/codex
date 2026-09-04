//! Versioned read-only projection materialization for decision provenance.

use super::StateRuntime;
use super::decision_provenance_storage::decode_rows;
use super::decision_provenance_storage::from_timestamp_millis;
use crate::decision_provenance::ChangeSet;
use crate::decision_provenance::Crossroad;
use crate::decision_provenance::Decision;
use crate::decision_provenance::PreferenceBoundary;
use crate::decision_provenance::ProvenanceNotification;
use crate::decision_provenance::ProvenanceProjection;
use crate::decision_provenance::ProvenanceRelationship;
use crate::decision_provenance::Warrant;
use crate::decision_provenance::build_projection;
use crate::decision_provenance::now;
use crate::decision_provenance::write_projection_atomically;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use std::path::PathBuf;

const MAX_PROJECTION_RECORDS: usize = 10_000;
const PROJECTION_FETCH_LIMIT: i64 = (MAX_PROJECTION_RECORDS as i64) + 1;

impl StateRuntime {
    /// Read the projection, repairing it from materialized state when it is missing, stale, or
    /// left behind by an interrupted atomic replacement.
    pub async fn read_provenance_projection(&self) -> anyhow::Result<ProvenanceProjection> {
        let path = self.provenance_projection_path();
        if let Ok(bytes) = tokio::fs::read(&path).await
            && let Ok(projection) = serde_json::from_slice::<ProvenanceProjection>(&bytes)
            && projection.schema_version
                == crate::decision_provenance::PROVENANCE_PROJECTION_VERSION
        {
            let latest_event = sqlx::query(
                "SELECT event_id, recorded_at_ms FROM provenance_events ORDER BY recorded_at_ms DESC, event_id DESC LIMIT 1",
            )
            .fetch_optional(self.pool.as_ref())
            .await?;
            let projection_is_current = match latest_event {
                Some(row) => {
                    let event_id: String = row.try_get("event_id")?;
                    let recorded_at_ms: i64 = row.try_get("recorded_at_ms")?;
                    projection.source_event_id.as_deref() == Some(event_id.as_str())
                        && projection
                            .source_event_recorded_at
                            .is_some_and(|timestamp| timestamp.timestamp_millis() == recorded_at_ms)
                }
                None => {
                    projection.source_event_id.is_none()
                        && projection.source_event_recorded_at.is_none()
                }
            };
            if projection_is_current {
                return Ok(projection);
            }
        }
        self.rebuild_provenance_projection().await?;
        let bytes = tokio::fs::read(&path).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Rebuild and atomically replace the Inbound projection while holding a SQLite write lock.
    pub async fn rebuild_provenance_projection(&self) -> anyhow::Result<PathBuf> {
        let mut tx = self.pool.begin().await?;
        // This write turns the deferred transaction into the single writer for the duration of
        // the snapshot. A concurrent event writer waits, then refreshes after its own commit.
        sqlx::query(
            "INSERT INTO provenance_projection_meta(key, value) VALUES ('writer_lock', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(now().to_rfc3339())
        .execute(&mut *tx)
        .await?;

        let latest_event = sqlx::query(
            "SELECT event_id, recorded_at_ms FROM provenance_events ORDER BY recorded_at_ms DESC, event_id DESC LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| {
            let event_id = row.try_get::<String, _>("event_id")?;
            let recorded_at_ms = row.try_get::<i64, _>("recorded_at_ms")?;
            Ok::<_, anyhow::Error>((event_id, recorded_at_ms))
        })
        .transpose()?;

        let (decisions, decisions_truncated) = decode_bounded_rows::<Decision>(
            sqlx::query(
                "SELECT payload_json FROM decision_records ORDER BY updated_at_ms DESC, id DESC LIMIT ?",
            )
            .bind(PROJECTION_FETCH_LIMIT)
            .fetch_all(&mut *tx)
            .await?,
        )?;
        let (crossroads, crossroads_truncated) = decode_bounded_rows::<Crossroad>(
            sqlx::query(
                "SELECT payload_json FROM crossroads ORDER BY updated_at_ms DESC, id DESC LIMIT ?",
            )
            .bind(PROJECTION_FETCH_LIMIT)
            .fetch_all(&mut *tx)
            .await?,
        )?;
        let (preference_boundaries, boundaries_truncated) =
            decode_bounded_rows::<PreferenceBoundary>(
            sqlx::query(
                "SELECT payload_json FROM preference_boundaries ORDER BY updated_at_ms DESC, id DESC LIMIT ?",
            )
            .bind(PROJECTION_FETCH_LIMIT)
            .fetch_all(&mut *tx)
            .await?,
        )?;
        let (warrants, warrants_truncated) = decode_bounded_rows::<Warrant>(
            sqlx::query(
                "SELECT payload_json FROM decision_warrants ORDER BY created_at_ms DESC, id DESC LIMIT ?",
            )
            .bind(PROJECTION_FETCH_LIMIT)
            .fetch_all(&mut *tx)
            .await?,
        )?;
        let (change_sets, change_sets_truncated) = decode_bounded_rows::<ChangeSet>(
            sqlx::query(
                "SELECT payload_json FROM decision_change_sets ORDER BY created_at_ms DESC, id DESC LIMIT ?",
            )
            .bind(PROJECTION_FETCH_LIMIT)
            .fetch_all(&mut *tx)
            .await?,
        )?;
        let (relationships, relationships_truncated) =
            decode_bounded_rows::<ProvenanceRelationship>(
            sqlx::query(
                "SELECT payload_json FROM provenance_relationships ORDER BY created_at_ms DESC, id DESC LIMIT ?",
            )
            .bind(PROJECTION_FETCH_LIMIT)
            .fetch_all(&mut *tx)
            .await?,
        )?;
        let (notifications, notifications_truncated) =
            decode_bounded_rows::<ProvenanceNotification>(
            sqlx::query(
                "SELECT payload_json FROM provenance_notifications ORDER BY created_at_ms DESC, id DESC LIMIT ?",
            )
            .bind(PROJECTION_FETCH_LIMIT)
            .fetch_all(&mut *tx)
            .await?,
        )?;
        let mut projection = build_projection(
            decisions,
            crossroads,
            preference_boundaries,
            warrants,
            change_sets,
            relationships,
            notifications,
        );
        projection.source_event_id = latest_event.as_ref().map(|(event_id, _)| event_id.clone());
        projection.source_event_recorded_at = latest_event
            .as_ref()
            .map(|(_, recorded_at_ms)| from_timestamp_millis(*recorded_at_ms))
            .transpose()?;
        projection.truncated = decisions_truncated
            || crossroads_truncated
            || boundaries_truncated
            || warrants_truncated
            || change_sets_truncated
            || relationships_truncated
            || notifications_truncated;
        let path = self.provenance_projection_path();
        sqlx::query(
            "INSERT INTO provenance_projection_meta(key, value) VALUES ('last_generated_at', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(projection.generated_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        // Keep the SQLite writer lock until the projection is published so concurrent rebuilds
        // cannot publish an older snapshot after a newer one. The projection only contains rows
        // visible in this transaction; canonical event rows are committed before rebuild starts.
        write_projection_atomically(&path, &projection).await?;
        tx.commit().await?;
        Ok(path)
    }
}

fn decode_bounded_rows<T: serde::de::DeserializeOwned>(
    mut rows: Vec<SqliteRow>,
) -> anyhow::Result<(Vec<T>, bool)> {
    let truncated = rows.len() > MAX_PROJECTION_RECORDS;
    if truncated {
        rows.truncate(MAX_PROJECTION_RECORDS);
    }
    Ok((decode_rows(rows)?, truncated))
}
