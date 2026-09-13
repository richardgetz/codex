use super::*;

impl StateRuntime {
    pub async fn enqueue_thread_inbound_message(
        &self,
        target_thread_id: ThreadId,
        source_thread_id: Option<ThreadId>,
        payload_json: String,
    ) -> anyhow::Result<String> {
        let message_id = uuid::Uuid::now_v7().to_string();
        self.enqueue_thread_inbound_message_with_id(
            message_id.clone(),
            target_thread_id,
            source_thread_id,
            payload_json,
        )
        .await?;
        Ok(message_id)
    }

    /// Persist a durable inbound message under a caller-chosen identity.
    ///
    /// Handoff fallbacks use a deterministic identity so retries after a sealed admission are
    /// idempotent. The existing row is left untouched when the same fallback is persisted again.
    pub async fn enqueue_thread_inbound_message_with_id(
        &self,
        message_id: String,
        target_thread_id: ThreadId,
        source_thread_id: Option<ThreadId>,
        payload_json: String,
    ) -> anyhow::Result<bool> {
        let created_at = Utc::now();
        let result = sqlx::query(
            r#"
INSERT INTO thread_inbound_messages (
    id,
    target_thread_id,
    source_thread_id,
    payload_json,
    created_at_ms,
    delivered_at_ms
) VALUES (?, ?, ?, ?, ?, NULL)
ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(message_id)
        .bind(target_thread_id.to_string())
        .bind(source_thread_id.map(|thread_id| thread_id.to_string()))
        .bind(payload_json)
        .bind(datetime_to_epoch_millis(created_at))
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn claim_pending_thread_inbound_messages(
        &self,
        target_thread_id: ThreadId,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::ThreadInboundMessage>> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            r#"
SELECT
    id,
    target_thread_id,
    source_thread_id,
    payload_json,
    created_at_ms,
    delivered_at_ms
FROM thread_inbound_messages
WHERE target_thread_id = ?
  AND delivered_at_ms IS NULL
ORDER BY created_at_ms ASC, id ASC
LIMIT ?
            "#,
        )
        .bind(target_thread_id.to_string())
        .bind(i64::from(limit))
        .fetch_all(&mut *tx)
        .await?;
        let messages: Vec<crate::ThreadInboundMessage> = rows
            .into_iter()
            .map(|row| {
                crate::model::ThreadInboundMessageRow::try_from_row(&row)
                    .and_then(crate::ThreadInboundMessage::try_from)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        if messages.is_empty() {
            tx.commit().await?;
            return Ok(messages);
        }

        let delivered_at = datetime_to_epoch_millis(Utc::now());
        for message in &messages {
            sqlx::query(
                r#"
UPDATE thread_inbound_messages
SET delivered_at_ms = ?
WHERE id = ?
  AND delivered_at_ms IS NULL
                "#,
            )
            .bind(delivered_at)
            .bind(message.id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(messages)
    }

    /// Return a claimed inbound message to the pending queue when admission fails before it can
    /// be applied. The update is idempotent and affects only an already-delivered row.
    pub async fn unclaim_thread_inbound_message(&self, message_id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query(
            r#"
UPDATE thread_inbound_messages
SET delivered_at_ms = NULL
WHERE id = ?
  AND delivered_at_ms IS NOT NULL
            "#,
        )
        .bind(message_id)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::StateRuntime;
    use super::test_support::test_thread_metadata;
    use super::test_support::unique_temp_dir;
    use codex_protocol::ThreadId;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn inbound_messages_are_claimed_once_per_target_thread() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
            "test-provider".to_string(),
        )
        .await
        .expect("initialize runtime");
        let source_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000101").expect("source");
        let target_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000102").expect("target");
        let other_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000103").expect("other");
        for thread_id in [source_thread_id, target_thread_id, other_thread_id] {
            runtime
                .upsert_thread(&test_thread_metadata(
                    codex_home.as_path(),
                    thread_id,
                    codex_home.clone(),
                ))
                .await
                .expect("insert thread metadata");
        }

        let first_payload = r#"{"items":[{"type":"text","text":"first"}]}"#.to_string();
        let second_payload = r#"{"items":[{"type":"text","text":"second"}]}"#.to_string();
        runtime
            .enqueue_thread_inbound_message(
                target_thread_id,
                /*source_thread_id*/ Some(source_thread_id),
                first_payload.clone(),
            )
            .await
            .expect("enqueue first");
        runtime
            .enqueue_thread_inbound_message(
                other_thread_id,
                /*source_thread_id*/ Some(source_thread_id),
                "{}".into(),
            )
            .await
            .expect("enqueue other");
        runtime
            .enqueue_thread_inbound_message(
                target_thread_id,
                /*source_thread_id*/ Some(source_thread_id),
                second_payload.clone(),
            )
            .await
            .expect("enqueue second");

        let claimed = runtime
            .claim_pending_thread_inbound_messages(target_thread_id, /*limit*/ 10)
            .await
            .expect("claim target");
        assert_eq!(
            claimed
                .iter()
                .map(|message| message.payload_json.as_str())
                .collect::<Vec<_>>(),
            vec![first_payload.as_str(), second_payload.as_str()]
        );
        assert_eq!(
            runtime
                .claim_pending_thread_inbound_messages(target_thread_id, /*limit*/ 10)
                .await
                .expect("claim target again"),
            Vec::new()
        );
        assert_eq!(
            runtime
                .claim_pending_thread_inbound_messages(other_thread_id, /*limit*/ 10)
                .await
                .expect("claim other")
                .len(),
            1
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn unclaim_returns_message_to_pending_queue() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
            "test-provider".to_string(),
        )
        .await
        .expect("initialize runtime");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000104").expect("thread");
        runtime
            .upsert_thread(&test_thread_metadata(
                codex_home.as_path(),
                thread_id,
                codex_home.clone(),
            ))
            .await
            .expect("insert thread metadata");
        let message_id = runtime
            .enqueue_thread_inbound_message(thread_id, None, "[]".to_string())
            .await
            .expect("enqueue message");
        let claimed = runtime
            .claim_pending_thread_inbound_messages(thread_id, /*limit*/ 1)
            .await
            .expect("claim message");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, message_id);

        assert!(runtime
            .unclaim_thread_inbound_message(&message_id)
            .await
            .expect("unclaim message"));
        let reclaimed = runtime
            .claim_pending_thread_inbound_messages(thread_id, /*limit*/ 1)
            .await
            .expect("reclaim message");
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].id, message_id);
        assert!(!runtime
            .unclaim_thread_inbound_message(&message_id)
            .await
            .expect("unclaim delivered message"));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn explicit_inbound_message_identity_is_idempotent() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
            "test-provider".to_string(),
        )
        .await
        .expect("initialize runtime");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000105").expect("thread");
        runtime
            .upsert_thread(&test_thread_metadata(
                codex_home.as_path(),
                thread_id,
                codex_home.clone(),
            ))
            .await
            .expect("insert thread metadata");
        let message_id = "handoff-message-105".to_string();
        assert!(runtime
            .enqueue_thread_inbound_message_with_id(
                message_id.clone(),
                thread_id,
                None,
                r#"{"type":"interAgentCommunication"}"#.to_string(),
            )
            .await
            .expect("insert explicit message"));
        assert!(!runtime
            .enqueue_thread_inbound_message_with_id(
                message_id.clone(),
                thread_id,
                None,
                "different payload".to_string(),
            )
            .await
            .expect("ignore duplicate explicit message"));
        let claimed = runtime
            .claim_pending_thread_inbound_messages(thread_id, /*limit*/ 1)
            .await
            .expect("claim explicit message");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, message_id);
        assert_eq!(
            claimed[0].payload_json,
            r#"{"type":"interAgentCommunication"}"#
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

}
