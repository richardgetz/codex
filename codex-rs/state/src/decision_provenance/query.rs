//! Query, event, and projection-facing provenance types.

use super::model::*;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionFilter {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub status: Option<DecisionStatus>,
    #[serde(default)]
    pub actor: Option<Actor>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub project_ref: Option<String>,
    #[serde(default)]
    pub limit: usize,
}

impl Default for DecisionFilter {
    fn default() -> Self {
        Self {
            text: None,
            status: None,
            actor: None,
            session_id: None,
            repository: None,
            project_ref: None,
            limit: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossroadFilter {
    #[serde(default)]
    pub status: Option<CrossroadStatus>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub project_ref: Option<String>,
    #[serde(default)]
    pub limit: usize,
}

impl Default for CrossroadFilter {
    fn default() -> Self {
        Self {
            status: None,
            session_id: None,
            project_ref: None,
            limit: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreferenceBoundaryFilter {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub scope: Option<ScopeRef>,
    #[serde(default)]
    pub lifecycle_status: Option<LifecycleStatus>,
    #[serde(default)]
    pub limit: usize,
}

impl Default for PreferenceBoundaryFilter {
    fn default() -> Self {
        Self {
            text: None,
            scope: None,
            lifecycle_status: None,
            limit: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendProvenanceEventResult {
    pub event_id: String,
    pub inserted: bool,
    #[serde(default)]
    pub projection_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceWriteOptions {
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub actor: Actor,
    pub occurred_at: DateTime<Utc>,
}

impl Default for ProvenanceWriteOptions {
    fn default() -> Self {
        Self {
            idempotency_key: None,
            actor: Actor::Agent,
            occurred_at: now(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSummary {
    pub event_id: String,
    pub idempotency_key: Option<String>,
    pub event_type: String,
    pub aggregate_type: String,
    pub aggregate_id: String,
    pub occurred_at: DateTime<Utc>,
    pub actor: Actor,
    pub privacy: PrivacyClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionWhy {
    pub decision: Decision,
    pub crossroad: Option<Crossroad>,
    pub boundaries: Vec<PreferenceBoundary>,
    pub warrant: Option<Warrant>,
    pub change_sets: Vec<ChangeSet>,
    pub relationships: Vec<ProvenanceRelationship>,
    pub history: Vec<EventSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProvenanceIndexes {
    pub decision_id: BTreeMap<String, Vec<String>>,
    pub crossroad_id: BTreeMap<String, Vec<String>>,
    pub preference_boundary_id: BTreeMap<String, Vec<String>>,
    pub session_id: BTreeMap<String, Vec<String>>,
    pub repository: BTreeMap<String, Vec<String>>,
    pub project: BTreeMap<String, Vec<String>>,
    pub commit_sha: BTreeMap<String, Vec<String>>,
    pub pull_request: BTreeMap<String, Vec<String>>,
    pub timestamp: BTreeMap<String, Vec<String>>,
    pub status: BTreeMap<String, Vec<String>>,
    pub actor: BTreeMap<String, Vec<String>>,
    pub scope: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceProjection {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub read_only: bool,
    /// The latest event included in this snapshot. Consumers can compare this watermark with
    /// the canonical event log and retry a refresh when the projection is stale.
    #[serde(default)]
    pub source_event_id: Option<String>,
    #[serde(default)]
    pub source_event_recorded_at: Option<DateTime<Utc>>,
    /// True when one or more projection collections were capped at the configured snapshot
    /// limit. Inbound should treat the projection as a bounded view in that case.
    #[serde(default)]
    pub truncated: bool,
    pub decisions: Vec<Decision>,
    pub crossroads: Vec<Crossroad>,
    pub preference_boundaries: Vec<PreferenceBoundary>,
    pub warrants: Vec<Warrant>,
    pub change_sets: Vec<ChangeSet>,
    pub relationships: Vec<ProvenanceRelationship>,
    pub notifications: Vec<ProvenanceNotification>,
    pub indexes: ProvenanceIndexes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceEvent {
    #[serde(default = "default_event_schema_version")]
    pub schema_version: u32,
    pub event_id: String,
    pub idempotency_key: Option<String>,
    pub event_type: ProvenanceEventType,
    pub aggregate_type: EntityType,
    pub aggregate_id: String,
    pub occurred_at: DateTime<Utc>,
    pub actor: Actor,
    pub privacy: PrivacyClass,
    pub payload: ProvenanceEventPayload,
}

fn default_event_schema_version() -> u32 {
    super::PROVENANCE_EVENT_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ProvenanceEventPayload {
    PreferenceBoundary(PreferenceBoundary),
    BoundaryTransition(BoundaryTransition),
    Crossroad(Crossroad),
    CrossroadStatus { id: String, status: CrossroadStatus },
    Decision(Decision),
    DecisionStatus { id: String, status: DecisionStatus },
    Warrant(Warrant),
    ChangeSet(ChangeSet),
    Relationship(ProvenanceRelationship),
    Notification(ProvenanceNotification),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceEventType {
    PreferenceBoundaryRecorded,
    PreferenceBoundaryTransitioned,
    CrossroadRecorded,
    CrossroadStatusChanged,
    DecisionRecorded,
    DecisionStatusChanged,
    WarrantRecorded,
    ChangeSetLinked,
    RelationshipRecorded,
    NotificationRecorded,
}

impl ProvenanceEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreferenceBoundaryRecorded => "preference_boundary_recorded",
            Self::PreferenceBoundaryTransitioned => "preference_boundary_transitioned",
            Self::CrossroadRecorded => "crossroad_recorded",
            Self::CrossroadStatusChanged => "crossroad_status_changed",
            Self::DecisionRecorded => "decision_recorded",
            Self::DecisionStatusChanged => "decision_status_changed",
            Self::WarrantRecorded => "warrant_recorded",
            Self::ChangeSetLinked => "change_set_linked",
            Self::RelationshipRecorded => "relationship_recorded",
            Self::NotificationRecorded => "notification_recorded",
        }
    }
}

pub fn now() -> DateTime<Utc> {
    DateTime::<Utc>::from(SystemTime::now())
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::now_v7())
}
