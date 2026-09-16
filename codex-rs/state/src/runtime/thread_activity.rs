use super::*;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::Row;

fn pause_from_row(
    row: &sqlx::sqlite::SqliteRow,
    root_thread_id: ThreadId,
) -> anyhow::Result<ThreadActivityPause> {
    let generation = row.try_get("generation")?;
    let state = match row.try_get::<String, _>("state")?.as_str() {
        "pausing" => ThreadActivityPauseState::Pausing,
        "paused" => ThreadActivityPauseState::Paused,
        "resuming" => ThreadActivityPauseState::Resuming,
        value => anyhow::bail!("unknown thread activity pause state {value:?}"),
    };
    Ok(ThreadActivityPause {
        root_thread_id,
        generation,
        state,
    })
}

/// Durable user intent for a root Team activity tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadActivityPauseState {
    Pausing,
    Paused,
    Resuming,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadActivityPause {
    pub root_thread_id: ThreadId,
    pub generation: i64,
    pub state: ThreadActivityPauseState,
}

/// One thread captured as unfinished work at a durable Team pause boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadActivityPauseSnapshot {
    pub thread_id: ThreadId,
    pub parent_thread_id: Option<ThreadId>,
}

impl StateRuntime {
    /// Persist a new pause generation before the process-local activity gate is changed.
    pub async fn pause_thread_activity(
        &self,
        root_thread_id: ThreadId,
    ) -> anyhow::Result<ThreadActivityPause> {
        let updated_at_ms = Utc::now().timestamp_millis();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
INSERT INTO thread_activity_pauses (root_thread_id, generation, state, updated_at_ms)
VALUES (?, 1, 'pausing', ?)
ON CONFLICT(root_thread_id) DO UPDATE SET
    generation = thread_activity_pauses.generation + 1,
    state = 'pausing',
    snapshot_captured = 0,
    updated_at_ms = excluded.updated_at_ms
RETURNING generation, state
            "#,
        )
        .bind(root_thread_id.to_string())
        .bind(updated_at_ms)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM thread_activity_pause_snapshots WHERE root_thread_id = ?")
            .bind(root_thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        pause_from_row(&row, root_thread_id)
    }

    /// Persist the pre-pause unfinished Team snapshot before exposing the pause as ready.
    ///
    /// An empty slice is meaningful: it records that the pause had no active work. A missing
    /// snapshot flag on an older row remains distinguishable and is handled conservatively by
    /// recovery so historical interrupted workers are never resurrected by accident.
    pub async fn record_thread_activity_pause_snapshot(
        &self,
        root_thread_id: ThreadId,
        generation: i64,
        snapshots: &[ThreadActivityPauseSnapshot],
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE thread_activity_pauses SET snapshot_captured = 1, updated_at_ms = ? WHERE root_thread_id = ? AND generation = ? AND state = 'pausing'",
        )
        .bind(Utc::now().timestamp_millis())
        .bind(root_thread_id.to_string())
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Ok(false);
        }
        sqlx::query(
            "DELETE FROM thread_activity_pause_snapshots WHERE root_thread_id = ? AND generation = ?",
        )
        .bind(root_thread_id.to_string())
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        for snapshot in snapshots {
            sqlx::query(
                "INSERT INTO thread_activity_pause_snapshots (root_thread_id, generation, thread_id, parent_thread_id) VALUES (?, ?, ?, ?)",
            )
            .bind(root_thread_id.to_string())
            .bind(generation)
            .bind(snapshot.thread_id.to_string())
            .bind(snapshot.parent_thread_id.map(|thread_id| thread_id.to_string()))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Read the current pause's captured unfinished Team snapshot.
    ///
    /// `None` means the marker predates snapshot persistence (or the capture failed), while
    /// `Some(empty)` means a new pause explicitly captured no active workers.
    pub async fn get_thread_activity_pause_snapshot(
        &self,
        root_thread_id: ThreadId,
    ) -> anyhow::Result<Option<Vec<ThreadActivityPauseSnapshot>>> {
        let Some((generation, snapshot_captured)) = sqlx::query_as::<_, (i64, i64)>(
            "SELECT generation, snapshot_captured FROM thread_activity_pauses WHERE root_thread_id = ?",
        )
        .bind(root_thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?
        else {
            return Ok(None);
        };
        if snapshot_captured == 0 {
            return Ok(None);
        }
        let rows = sqlx::query(
            "SELECT thread_id, parent_thread_id FROM thread_activity_pause_snapshots WHERE root_thread_id = ? AND generation = ? ORDER BY thread_id",
        )
        .bind(root_thread_id.to_string())
        .bind(generation)
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ThreadActivityPauseSnapshot {
                    thread_id: ThreadId::from_string(&row.try_get::<String, _>("thread_id")?)?,
                    parent_thread_id: row
                        .try_get::<Option<String>, _>("parent_thread_id")?
                        .map(|value| ThreadId::from_string(&value))
                        .transpose()?,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .map(Some)
    }

    pub async fn begin_thread_activity_resume(
        &self,
        root_thread_id: ThreadId,
    ) -> anyhow::Result<Option<ThreadActivityPause>> {
        let updated_at_ms = Utc::now().timestamp_millis();
        let row = sqlx::query(
            "UPDATE thread_activity_pauses SET state = 'resuming', updated_at_ms = ? WHERE root_thread_id = ? AND state = 'paused' RETURNING generation, state",
        )
        .bind(updated_at_ms)
        .bind(root_thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(|row| pause_from_row(&row, root_thread_id))
            .transpose()
    }

    pub async fn complete_thread_activity_pause(
        &self,
        root_thread_id: ThreadId,
        generation: i64,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE thread_activity_pauses SET state = 'paused', updated_at_ms = ? WHERE root_thread_id = ? AND generation = ? AND state = 'pausing'",
        )
        .bind(Utc::now().timestamp_millis())
        .bind(root_thread_id.to_string())
        .bind(generation)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Re-arm a continue that was interrupted while the root runtime was being replaced.
    pub async fn recover_thread_activity_pause(
        &self,
        root_thread_id: ThreadId,
    ) -> anyhow::Result<Option<ThreadActivityPause>> {
        sqlx::query(
            "UPDATE thread_activity_pauses SET state = 'paused', updated_at_ms = ? WHERE root_thread_id = ? AND state IN ('pausing', 'resuming')",
        )
        .bind(Utc::now().timestamp_millis())
        .bind(root_thread_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        self.get_thread_activity_pause(root_thread_id).await
    }

    /// Leave a failed continue attempt durably paused for a later explicit retry.
    pub async fn retain_thread_activity_pause(
        &self,
        root_thread_id: ThreadId,
        generation: i64,
    ) -> anyhow::Result<bool> {
        let updated_at_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE thread_activity_pauses SET state = 'paused', updated_at_ms = ? WHERE root_thread_id = ? AND generation = ?",
        )
        .bind(updated_at_ms)
        .bind(root_thread_id.to_string())
        .bind(generation)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Clear a pause only after the corresponding continue operation was acknowledged by Core.
    pub async fn complete_thread_activity_resume(
        &self,
        root_thread_id: ThreadId,
        generation: i64,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "DELETE FROM thread_activity_pauses WHERE root_thread_id = ? AND generation = ? AND state = 'resuming'",
        )
        .bind(root_thread_id.to_string())
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Ok(false);
        }
        sqlx::query(
            "DELETE FROM thread_activity_pause_snapshots WHERE root_thread_id = ? AND generation = ?",
        )
        .bind(root_thread_id.to_string())
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn get_thread_activity_pause(
        &self,
        root_thread_id: ThreadId,
    ) -> anyhow::Result<Option<ThreadActivityPause>> {
        let row = sqlx::query(
            "SELECT generation, state FROM thread_activity_pauses WHERE root_thread_id = ?",
        )
        .bind(root_thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(|row| pause_from_row(&row, root_thread_id))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::StateRuntime;
    use super::ThreadActivityPauseState;
    use super::ThreadActivityPauseSnapshot;
    use crate::SqliteConfig;
    use crate::runtime::test_support::unique_temp_dir;
    use codex_protocol::ThreadId;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn pause_generation_survives_failed_continue_and_retries() {
        let home = unique_temp_dir();
        let runtime = StateRuntime::init(
            SqliteConfig::new_for_testing(home.as_path().abs()),
            "test-provider".to_string(),
        )
        .await
        .expect("initialize runtime");
        let root = ThreadId::from_string("00000000-0000-0000-0000-000000000011").expect("root id");

        let first = runtime.pause_thread_activity(root).await.expect("pause");
        assert_eq!(first.state, ThreadActivityPauseState::Pausing);
        assert!(
            runtime
                .begin_thread_activity_resume(root)
                .await
                .expect("continue while pausing")
                .is_none()
        );
        assert!(
            runtime
                .complete_thread_activity_pause(root, first.generation)
                .await
                .expect("apply pause")
        );
        let paused = runtime
            .get_thread_activity_pause(root)
            .await
            .expect("read paused marker");
        assert_eq!(
            paused.map(|marker| marker.state),
            Some(ThreadActivityPauseState::Paused)
        );
        let claimed = runtime
            .begin_thread_activity_resume(root)
            .await
            .expect("begin resume")
            .expect("marker");
        assert_eq!(claimed.generation, first.generation);
        assert_eq!(claimed.state, ThreadActivityPauseState::Resuming);
        assert!(
            runtime
                .retain_thread_activity_pause(root, first.generation)
                .await
                .expect("retain pause")
        );

        let retry = runtime
            .begin_thread_activity_resume(root)
            .await
            .expect("retry resume")
            .expect("marker");
        assert_eq!(retry, claimed);
        assert!(
            runtime
                .complete_thread_activity_resume(root, retry.generation)
                .await
                .expect("complete resume")
        );
        assert!(
            runtime
                .get_thread_activity_pause(root)
                .await
                .expect("read marker")
                .is_none()
        );
    }

    #[tokio::test]
    async fn pause_snapshot_round_trips_and_new_generation_clears_previous_capture() {
        let home = unique_temp_dir();
        let runtime = StateRuntime::init(
            SqliteConfig::new_for_testing(home.as_path().abs()),
            "test-provider".to_string(),
        )
        .await
        .expect("initialize runtime");
        let root = ThreadId::from_string("00000000-0000-0000-0000-000000000021").expect("root");
        let child = ThreadId::from_string("00000000-0000-0000-0000-000000000022").expect("child");

        let first = runtime.pause_thread_activity(root).await.expect("pause");
        let snapshots = vec![ThreadActivityPauseSnapshot {
            thread_id: child,
            parent_thread_id: Some(root),
        }];
        assert!(
            runtime
                .record_thread_activity_pause_snapshot(root, first.generation, &snapshots)
                .await
                .expect("record snapshot")
        );
        assert_eq!(
            runtime
                .get_thread_activity_pause_snapshot(root)
                .await
                .expect("read snapshot"),
            Some(snapshots)
        );

        let second = runtime.pause_thread_activity(root).await.expect("next pause");
        assert_eq!(second.generation, first.generation + 1);
        assert_eq!(
            runtime
                .get_thread_activity_pause_snapshot(root)
                .await
                .expect("read cleared snapshot"),
            None
        );
    }
}
