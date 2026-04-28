use super::*;

impl StateRuntime {
    pub async fn enqueue_thread_inbound_message(
        &self,
        target_thread_id: ThreadId,
        source_thread_id: Option<ThreadId>,
        payload_json: String,
    ) -> anyhow::Result<String> {
        let message_id = uuid::Uuid::now_v7().to_string();
        let created_at = Utc::now();
        sqlx::query(
            r#"
INSERT INTO thread_inbound_messages (
    id,
    target_thread_id,
    source_thread_id,
    payload_json,
    created_at_ms,
    delivered_at_ms
) VALUES (?, ?, ?, ?, ?, NULL)
            "#,
        )
        .bind(message_id.as_str())
        .bind(target_thread_id.to_string())
        .bind(source_thread_id.map(|thread_id| thread_id.to_string()))
        .bind(payload_json)
        .bind(datetime_to_epoch_millis(created_at))
        .execute(self.pool.as_ref())
        .await?;
        Ok(message_id)
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
}

#[cfg(test)]
mod tests {
    use super::StateRuntime;
    use super::test_support::test_thread_metadata;
    use super::test_support::unique_temp_dir;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn inbound_messages_are_claimed_once_per_target_thread() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
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
}
