use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadControlMode {
    Continuous,
    Router,
}

impl ThreadControlMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Continuous => "continuous",
            Self::Router => "router",
        }
    }
}

impl TryFrom<String> for ThreadControlMode {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "continuous" => Ok(Self::Continuous),
            "router" => Ok(Self::Router),
            _ => Err(anyhow::anyhow!("unknown thread control mode: {value}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadControlRecord {
    pub thread_id: ThreadId,
    pub mode: ThreadControlMode,
    pub reason: String,
    pub release_channel: Option<String>,
    pub watch_interval_seconds: Option<u32>,
    pub released_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub target_thread_ids: Vec<ThreadId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThreadControlRow {
    pub thread_id: String,
    pub mode: String,
    pub reason: String,
    pub release_channel: Option<String>,
    pub watch_interval_seconds: Option<i64>,
    pub released_at: Option<i64>,
    pub updated_at: i64,
}

impl ThreadControlRow {
    pub(crate) fn try_from_row(row: &SqliteRow) -> anyhow::Result<Self> {
        Ok(Self {
            thread_id: row.try_get("thread_id")?,
            mode: row.try_get("mode")?,
            reason: row.try_get("reason")?,
            release_channel: row.try_get("release_channel")?,
            watch_interval_seconds: row.try_get("watch_interval_seconds")?,
            released_at: row.try_get("released_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

impl TryFrom<ThreadControlRow> for ThreadControlRecord {
    type Error = anyhow::Error;

    fn try_from(value: ThreadControlRow) -> Result<Self, Self::Error> {
        let watch_interval_seconds = value
            .watch_interval_seconds
            .map(u32::try_from)
            .transpose()
            .map_err(|err| anyhow::anyhow!("invalid watch interval: {err}"))?;
        Ok(Self {
            thread_id: ThreadId::from_string(&value.thread_id)?,
            mode: ThreadControlMode::try_from(value.mode)?,
            reason: value.reason,
            release_channel: value.release_channel,
            watch_interval_seconds,
            released_at: value
                .released_at
                .map(|epoch| {
                    DateTime::<Utc>::from_timestamp(epoch, 0)
                        .ok_or_else(|| anyhow::anyhow!("invalid released_at timestamp"))
                })
                .transpose()?,
            updated_at: DateTime::<Utc>::from_timestamp(value.updated_at, 0)
                .ok_or_else(|| anyhow::anyhow!("invalid updated_at timestamp"))?,
            target_thread_ids: Vec::new(),
        })
    }
}
