use super::*;

impl StateRuntime {
    pub async fn get_active_thread_control(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<crate::ThreadControlRecord>> {
        Ok(self
            .get_thread_control(thread_id)
            .await?
            .filter(|control| control.released_at.is_none()))
    }

    pub async fn get_thread_control(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<crate::ThreadControlRecord>> {
        let row = sqlx::query(
            r#"
SELECT
    thread_id,
    mode,
    reason,
    release_channel,
    watch_interval_seconds,
    released_at,
    updated_at
FROM thread_controls
WHERE thread_id = ?
            "#,
        )
        .bind(thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut control = crate::model::ThreadControlRow::try_from_row(&row)
            .and_then(crate::ThreadControlRecord::try_from)?;
        control.target_thread_ids = self.list_thread_control_targets(thread_id).await?;
        Ok(Some(control))
    }

    pub async fn upsert_thread_control(
        &self,
        control: &crate::ThreadControlRecord,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
INSERT INTO thread_controls (
    thread_id,
    mode,
    reason,
    release_channel,
    watch_interval_seconds,
    released_at,
    updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id) DO UPDATE SET
    mode = excluded.mode,
    reason = excluded.reason,
    release_channel = excluded.release_channel,
    watch_interval_seconds = excluded.watch_interval_seconds,
    released_at = excluded.released_at,
    updated_at = excluded.updated_at
            "#,
        )
        .bind(control.thread_id.to_string())
        .bind(control.mode.as_str())
        .bind(control.reason.as_str())
        .bind(control.release_channel.as_deref())
        .bind(control.watch_interval_seconds.map(i64::from))
        .bind(control.released_at.map(datetime_to_epoch_seconds))
        .bind(datetime_to_epoch_seconds(control.updated_at))
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM thread_control_targets WHERE thread_id = ?")
            .bind(control.thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        for target_thread_id in &control.target_thread_ids {
            sqlx::query(
                r#"
INSERT INTO thread_control_targets (
    thread_id,
    target_thread_id
) VALUES (?, ?)
                "#,
            )
            .bind(control.thread_id.to_string())
            .bind(target_thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn release_thread_control(
        &self,
        thread_id: ThreadId,
        released_at: DateTime<Utc>,
    ) -> anyhow::Result<Option<crate::ThreadControlRecord>> {
        let result = sqlx::query(
            "UPDATE thread_controls SET released_at = ?, updated_at = ? WHERE thread_id = ? AND released_at IS NULL",
        )
        .bind(datetime_to_epoch_seconds(released_at))
        .bind(datetime_to_epoch_seconds(released_at))
        .bind(thread_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        if result.rows_affected() == 0 {
            return self.get_thread_control(thread_id).await;
        }
        self.get_thread_control(thread_id).await
    }

    pub async fn list_active_thread_controls_targeting(
        &self,
        target_thread_id: ThreadId,
    ) -> anyhow::Result<Vec<crate::ThreadControlRecord>> {
        let rows = sqlx::query(
            r#"
SELECT
    thread_controls.thread_id,
    thread_controls.mode,
    thread_controls.reason,
    thread_controls.release_channel,
    thread_controls.watch_interval_seconds,
    thread_controls.released_at,
    thread_controls.updated_at
FROM thread_control_targets
JOIN thread_controls ON thread_controls.thread_id = thread_control_targets.thread_id
WHERE thread_control_targets.target_thread_id = ?
  AND thread_controls.released_at IS NULL
ORDER BY thread_controls.updated_at DESC
            "#,
        )
        .bind(target_thread_id.to_string())
        .fetch_all(self.pool.as_ref())
        .await?;

        let mut controls = Vec::with_capacity(rows.len());
        for row in rows {
            let mut control = crate::model::ThreadControlRow::try_from_row(&row)
                .and_then(crate::ThreadControlRecord::try_from)?;
            control.target_thread_ids = self.list_thread_control_targets(control.thread_id).await?;
            controls.push(control);
        }
        Ok(controls)
    }

    async fn list_thread_control_targets(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Vec<ThreadId>> {
        let rows = sqlx::query(
            r#"
SELECT target_thread_id
FROM thread_control_targets
WHERE thread_id = ?
ORDER BY target_thread_id ASC
            "#,
        )
        .bind(thread_id.to_string())
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.into_iter()
            .map(|row| {
                let target_thread_id: String = row.try_get("target_thread_id")?;
                ThreadId::from_string(&target_thread_id).map_err(Into::into)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::StateRuntime;
    use super::test_support::unique_temp_dir;
    use chrono::TimeZone;
    use chrono::Utc;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn thread_control_round_trips_targets_and_release_state() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000011").expect("thread id");
        let target_thread_ids = vec![
            ThreadId::from_string("00000000-0000-0000-0000-000000000012").expect("target a"),
            ThreadId::from_string("00000000-0000-0000-0000-000000000013").expect("target b"),
        ];
        let updated_at = Utc
            .timestamp_opt(1_700_000_123, 0)
            .single()
            .expect("updated_at");
        runtime
            .upsert_thread_control(&crate::ThreadControlRecord {
                thread_id,
                mode: crate::ThreadControlMode::Router,
                reason: "Keep supervising spawned sessions".to_string(),
                release_channel: Some("imessage".to_string()),
                watch_interval_seconds: Some(45),
                released_at: None,
                updated_at,
                target_thread_ids: target_thread_ids.clone(),
            })
            .await
            .expect("insert control");

        assert_eq!(
            runtime
                .get_active_thread_control(thread_id)
                .await
                .expect("load active control"),
            Some(crate::ThreadControlRecord {
                thread_id,
                mode: crate::ThreadControlMode::Router,
                reason: "Keep supervising spawned sessions".to_string(),
                release_channel: Some("imessage".to_string()),
                watch_interval_seconds: Some(45),
                released_at: None,
                updated_at,
                target_thread_ids: target_thread_ids.clone(),
            })
        );

        assert_eq!(
            runtime
                .get_thread_control(thread_id)
                .await
                .expect("load control"),
            Some(crate::ThreadControlRecord {
                thread_id,
                mode: crate::ThreadControlMode::Router,
                reason: "Keep supervising spawned sessions".to_string(),
                release_channel: Some("imessage".to_string()),
                watch_interval_seconds: Some(45),
                released_at: None,
                updated_at,
                target_thread_ids: target_thread_ids.clone(),
            })
        );

        assert_eq!(
            runtime
                .list_active_thread_controls_targeting(target_thread_ids[0])
                .await
                .expect("load controls targeting first target"),
            vec![crate::ThreadControlRecord {
                thread_id,
                mode: crate::ThreadControlMode::Router,
                reason: "Keep supervising spawned sessions".to_string(),
                release_channel: Some("imessage".to_string()),
                watch_interval_seconds: Some(45),
                released_at: None,
                updated_at,
                target_thread_ids: target_thread_ids.clone(),
            }]
        );

        let released_at = Utc
            .timestamp_opt(1_700_000_456, 0)
            .single()
            .expect("released_at");
        let released = runtime
            .release_thread_control(thread_id, released_at)
            .await
            .expect("release control")
            .expect("released control");
        assert_eq!(released.released_at, Some(released_at));
        assert_eq!(
            runtime
                .get_active_thread_control(thread_id)
                .await
                .expect("load released active control"),
            None
        );
        assert_eq!(
            runtime
                .list_active_thread_controls_targeting(target_thread_ids[0])
                .await
                .expect("released control should not be active"),
            Vec::<crate::ThreadControlRecord>::new()
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }
}
