use chrono::DateTime;
use chrono::Utc;
use codex_protocol::config_types::UserPreferencesMemoryBucket;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MemoryScopeKind {
    Global,
    Repo,
    Project,
    Task,
    Person,
    Process,
    Skill,
    Command,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct MemoryScope {
    #[serde(rename = "type")]
    pub kind: MemoryScopeKind,
    pub id: String,
    pub evidence: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct MemoryEvent {
    pub observed_at: DateTime<Utc>,
    pub thread_id: String,
    pub turn_id: String,
    pub bucket: MemoryBucket,
    #[serde(default)]
    pub scope: MemoryScope,
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
    pub scope: MemoryScope,
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
    pub scope: MemoryScope,
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

impl Default for MemoryScope {
    fn default() -> Self {
        Self::global()
    }
}

impl MemoryScope {
    pub fn new(
        kind: MemoryScopeKind,
        id: impl Into<String>,
        evidence: impl Into<String>,
        confidence: f32,
    ) -> Self {
        Self {
            kind,
            id: id.into(),
            evidence: evidence.into(),
            confidence,
        }
        .normalized()
    }

    pub fn global() -> Self {
        Self {
            kind: MemoryScopeKind::Global,
            id: "global".to_string(),
            evidence: "applies globally".to_string(),
            confidence: 1.0,
        }
    }

    pub fn process(id: impl Into<String>, evidence: impl Into<String>) -> Self {
        Self {
            kind: MemoryScopeKind::Process,
            id: id.into(),
            evidence: evidence.into(),
            confidence: 0.8,
        }
    }

    pub fn normalized(mut self) -> Self {
        self.id = self.id.trim().to_ascii_lowercase();
        self.evidence = self.evidence.trim().to_string();
        if self.kind == MemoryScopeKind::Global || self.id.is_empty() {
            return Self::global();
        }
        self.confidence = self.confidence.clamp(/*min*/ 0.0, /*max*/ 1.0);
        self
    }

    pub fn label(&self) -> Option<String> {
        if self.kind == MemoryScopeKind::Global {
            return None;
        }
        Some(format!("{}:{}", self.kind.as_str(), self.id))
    }

    pub fn identity(&self) -> (MemoryScopeKind, String) {
        if self.kind == MemoryScopeKind::Global {
            (MemoryScopeKind::Global, "global".to_string())
        } else {
            (self.kind, self.id.clone())
        }
    }
}

impl MemoryScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryScopeKind::Global => "global",
            MemoryScopeKind::Repo => "repo",
            MemoryScopeKind::Project => "project",
            MemoryScopeKind::Task => "task",
            MemoryScopeKind::Person => "person",
            MemoryScopeKind::Process => "process",
            MemoryScopeKind::Skill => "skill",
            MemoryScopeKind::Command => "command",
            MemoryScopeKind::Tool => "tool",
        }
    }
}

impl MemoryBucket {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryBucket::DurablePreference => "durable_preference",
            MemoryBucket::PersonalContext => "personal_context",
            MemoryBucket::RelationalAttunement => "relational_attunement",
            MemoryBucket::OperatorPlaybook => "operator_playbook",
            MemoryBucket::OngoingThreads => "ongoing_threads",
            MemoryBucket::FollowupState => "followup_state",
        }
    }

    pub fn all() -> &'static [MemoryBucket] {
        &[
            MemoryBucket::DurablePreference,
            MemoryBucket::PersonalContext,
            MemoryBucket::RelationalAttunement,
            MemoryBucket::OperatorPlaybook,
            MemoryBucket::OngoingThreads,
            MemoryBucket::FollowupState,
        ]
    }
}

impl From<MemoryBucket> for UserPreferencesMemoryBucket {
    fn from(value: MemoryBucket) -> Self {
        match value {
            MemoryBucket::DurablePreference => Self::DurablePreference,
            MemoryBucket::PersonalContext => Self::PersonalContext,
            MemoryBucket::RelationalAttunement => Self::RelationalAttunement,
            MemoryBucket::OperatorPlaybook => Self::OperatorPlaybook,
            MemoryBucket::OngoingThreads => Self::OngoingThreads,
            MemoryBucket::FollowupState => Self::FollowupState,
        }
    }
}
