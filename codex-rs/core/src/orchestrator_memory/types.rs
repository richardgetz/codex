use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

pub(super) const EXPLICIT_CONFIDENCE: f32 = 0.95;
pub(super) const REPEATED_STEERING_CONFIDENCE: f32 = 0.65;
pub(super) const ASSISTANT_ACKNOWLEDGED_CONFIDENCE: f32 = 0.85;
pub(super) const MODEL_CLASSIFIED_CONFIDENCE: f32 = 0.8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MemorySignal {
    Explicit,
    RepeatedSteering,
    AssistantAcknowledged,
    ModelClassified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MemoryBucket {
    DurablePreference,
    PersonalContext,
    RelationalAttunement,
    OperatorPlaybook,
    OngoingThreads,
    FollowupState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MemoryOperation {
    Upsert,
    Forget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct MemoryEvent {
    pub observed_at: DateTime<Utc>,
    pub thread_id: String,
    pub turn_id: String,
    pub bucket: MemoryBucket,
    pub operation: MemoryOperation,
    pub signal: MemorySignal,
    pub key: String,
    pub candidate: String,
    pub source_excerpt: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct CandidateMemoryItem {
    pub bucket: MemoryBucket,
    pub operation: MemoryOperation,
    pub signal: MemorySignal,
    pub key: String,
    pub candidate: String,
    pub source_excerpt: String,
    pub confidence: f32,
}

#[derive(Debug, Clone)]
pub(super) struct AggregatedMemoryItem {
    pub bucket: MemoryBucket,
    pub candidate: String,
    pub observations: usize,
    pub direct_observations: usize,
    pub last_seen: DateTime<Utc>,
    pub confidence_sum: f32,
}

#[derive(Debug, Clone, Default)]
pub(super) struct AggregatedMemorySnapshot {
    pub preferences: Vec<AggregatedMemoryItem>,
    pub personal_context: Vec<AggregatedMemoryItem>,
    pub relational_attunement: Vec<AggregatedMemoryItem>,
    pub operator_playbook: Vec<AggregatedMemoryItem>,
    pub ongoing_threads: Vec<AggregatedMemoryItem>,
    pub followups: Vec<AggregatedMemoryItem>,
}

impl MemorySignal {
    pub fn is_direct(self) -> bool {
        matches!(
            self,
            MemorySignal::Explicit
                | MemorySignal::AssistantAcknowledged
                | MemorySignal::ModelClassified
        )
    }
}
