use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;

mod agent_job;
mod backfill_state;
mod graph;
mod log;
mod memories;
mod thread_metadata;

pub use agent_job::AgentJob;
pub use agent_job::AgentJobCreateParams;
pub use agent_job::AgentJobItem;
pub use agent_job::AgentJobItemCreateParams;
pub use agent_job::AgentJobItemStatus;
pub use agent_job::AgentJobProgress;
pub use agent_job::AgentJobStatus;
pub use backfill_state::BackfillState;
pub use backfill_state::BackfillStatus;
pub use graph::DirectionalThreadSpawnEdgeStatus;
pub use log::LogEntry;
pub use log::LogQuery;
pub use log::LogRow;
pub use memories::Phase2InputSelection;
pub use memories::Phase2JobClaimOutcome;
pub use memories::Stage1JobClaim;
pub use memories::Stage1JobClaimOutcome;
pub use memories::Stage1Output;
pub use memories::Stage1OutputRef;
pub use memories::Stage1StartupClaimParams;
pub use thread_metadata::Anchor;
pub use thread_metadata::BackfillStats;
pub use thread_metadata::ExtractionOutcome;
pub use thread_metadata::SortDirection;
pub use thread_metadata::SortKey;
pub use thread_metadata::ThreadMetadata;
pub use thread_metadata::ThreadMetadataBuilder;
pub use thread_metadata::ThreadsPage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadControlMode {
    Continuous,
    Router,
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

pub(crate) use agent_job::AgentJobItemRow;
pub(crate) use agent_job::AgentJobRow;
pub(crate) use memories::Stage1OutputRow;
pub(crate) use memories::stage1_output_ref_from_parts;
pub(crate) use thread_metadata::ThreadRow;
pub(crate) use thread_metadata::anchor_from_item;
pub(crate) use thread_metadata::datetime_to_epoch_millis;
pub(crate) use thread_metadata::datetime_to_epoch_seconds;
pub(crate) use thread_metadata::epoch_millis_to_datetime;
