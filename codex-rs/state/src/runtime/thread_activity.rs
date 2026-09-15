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
    updated_at_ms = excluded.updated_at_ms
RETURNING generation, state
            "#,
        )
        .bind(root_thread_id.to_string())
        .bind(updated_at_ms)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        pause_from_row(&row, root_thread_id)
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
        let result = sqlx::query(
            "DELETE FROM thread_activity_pauses WHERE root_thread_id = ? AND generation = ? AND state = 'resuming'",
        )
        .bind(root_thread_id.to_string())
        .bind(generation)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
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
}
