use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadInboundMessage {
    pub id: String,
    pub target_thread_id: ThreadId,
    pub source_thread_id: Option<ThreadId>,
    pub payload_json: String,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThreadInboundMessageRow {
    pub id: String,
    pub target_thread_id: String,
    pub source_thread_id: Option<String>,
    pub payload_json: String,
    pub created_at_ms: i64,
    pub delivered_at_ms: Option<i64>,
}

impl ThreadInboundMessageRow {
    pub(crate) fn try_from_row(row: &SqliteRow) -> anyhow::Result<Self> {
        Ok(Self {
            id: row.try_get("id")?,
            target_thread_id: row.try_get("target_thread_id")?,
            source_thread_id: row.try_get("source_thread_id")?,
            payload_json: row.try_get("payload_json")?,
            created_at_ms: row.try_get("created_at_ms")?,
            delivered_at_ms: row.try_get("delivered_at_ms")?,
        })
    }
}

impl TryFrom<ThreadInboundMessageRow> for ThreadInboundMessage {
    type Error = anyhow::Error;

    fn try_from(value: ThreadInboundMessageRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id,
            target_thread_id: ThreadId::from_string(&value.target_thread_id)?,
            source_thread_id: value
                .source_thread_id
                .as_deref()
                .map(ThreadId::from_string)
                .transpose()?,
            payload_json: value.payload_json,
            created_at: crate::model::epoch_millis_to_datetime(value.created_at_ms)?,
            delivered_at: value
                .delivered_at_ms
                .map(crate::model::epoch_millis_to_datetime)
                .transpose()?,
        })
    }
}
