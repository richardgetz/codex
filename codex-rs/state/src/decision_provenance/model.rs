use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
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

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Repo => "repo",
            Self::Project => "project",
            Self::Task => "task",
            Self::Person => "person",
            Self::Process => "process",
            Self::Skill => "skill",
            Self::Command => "command",
            Self::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceReference {
    pub source_type: String,
    pub reference: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub privacy: PrivacyClass,
}

impl SourceReference {
    pub fn new(source_type: impl Into<String>, reference: impl Into<String>) -> Self {
        Self {
            source_type: source_type.into(),
            reference: reference.into(),
            label: None,
            privacy: PrivacyClass::Private,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamps {
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub observed_at: Option<DateTime<Utc>>,
    pub recorded_at: DateTime<Utc>,
    #[serde(default)]
    pub effective_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub superseded_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Timestamps {
    pub fn now() -> Self {
        let now = DateTime::<Utc>::from(SystemTime::now());
        Self {
            created_at: now,
            observed_at: Some(now),
            recorded_at: now,
            effective_at: None,
            superseded_at: None,
            updated_at: Some(now),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceKind {
    HardConstraint,
    PreferenceBoundary,
    SoftPreference,
    CandidatePreference,
}

impl PreferenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HardConstraint => "hard_constraint",
            Self::PreferenceBoundary => "preference_boundary",
            Self::SoftPreference => "soft_preference",
            Self::CandidatePreference => "candidate_preference",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceStrength {
    Hard,
    Confirmation,
    Soft,
}

impl PreferenceStrength {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Confirmation => "confirmation",
            Self::Soft => "soft",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    System,
    Developer,
    Safety,
    Legal,
    Privacy,
    Security,
    Repository,
    Product,
    User,
    Agent,
    Default,
}

impl Authority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
            Self::Safety => "safety",
            Self::Legal => "legal",
            Self::Privacy => "privacy",
            Self::Security => "security",
            Self::Repository => "repository",
            Self::Product => "product",
            Self::User => "user",
            Self::Agent => "agent",
            Self::Default => "default",
        }
    }

    pub fn precedence(self) -> u8 {
        match self {
            Self::System
            | Self::Developer
            | Self::Safety
            | Self::Legal
            | Self::Privacy
            | Self::Security => 7,
            Self::Repository | Self::Product => 6,
            Self::User => 5,
            Self::Agent => 2,
            Self::Default => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    Candidate,
    Active,
    Confirmed,
    Narrowed,
    Broadened,
    Withdrawn,
    Superseded,
    Reopened,
}

impl LifecycleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Confirmed => "confirmed",
            Self::Narrowed => "narrowed",
            Self::Broadened => "broadened",
            Self::Withdrawn => "withdrawn",
            Self::Superseded => "superseded",
            Self::Reopened => "reopened",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Active | Self::Confirmed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyClass {
    Public,
    #[default]
    Private,
    Sensitive,
}

impl PrivacyClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::Sensitive => "sensitive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationCategory {
    Informational,
    ReviewRecommended,
    ApprovalRequired,
    Blocked,
    PreferenceBoundaryCrossed,
    InferredPreference,
    ConflictDetected,
    Superseded,
    Reopened,
    ImplementationFailure,
}

impl NotificationCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Informational => "informational",
            Self::ReviewRecommended => "review_recommended",
            Self::ApprovalRequired => "approval_required",
            Self::Blocked => "blocked",
            Self::PreferenceBoundaryCrossed => "preference_boundary_crossed",
            Self::InferredPreference => "inferred_preference",
            Self::ConflictDetected => "conflict_detected",
            Self::Superseded => "superseded",
            Self::Reopened => "reopened",
            Self::ImplementationFailure => "implementation_failure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceNotification {
    pub id: String,
    pub category: NotificationCategory,
    pub message: String,
    #[serde(default)]
    pub preference_boundary_id: Option<String>,
    #[serde(default)]
    pub crossroad_id: Option<String>,
    #[serde(default)]
    pub decision_id: Option<String>,
    #[serde(default)]
    pub authority_required: Option<Authority>,
    #[serde(default)]
    pub choice: Option<String>,
    pub will_record: bool,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub source_refs: Vec<SourceReference>,
    #[serde(default)]
    pub privacy: PrivacyClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreferenceBoundary {
    pub id: String,
    pub kind: PreferenceKind,
    pub statement: String,
    pub scope: ScopeRef,
    pub strength: PreferenceStrength,
    pub authority: Authority,
    pub source: SourceReference,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub confidence: Option<u8>,
    pub lifecycle_status: LifecycleStatus,
    pub timestamps: Timestamps,
    #[serde(default)]
    pub related_memory_record_id: Option<String>,
    #[serde(default)]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub privacy: PrivacyClass,
}

impl PreferenceBoundary {
    pub fn is_candidate(&self) -> bool {
        self.kind == PreferenceKind::CandidatePreference
            || self.lifecycle_status == LifecycleStatus::Candidate
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreferenceBoundaryPreflight {
    pub scope: ScopeRef,
    pub active: Vec<PreferenceBoundary>,
    pub candidates: Vec<PreferenceBoundary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeRef {
    pub kind: Scope,
    pub id: String,
}

impl ScopeRef {
    pub fn global() -> Self {
        Self {
            kind: Scope::Global,
            id: "global".to_string(),
        }
    }

    pub fn new(kind: Scope, id: impl Into<String>) -> Self {
        let id = id.into().trim().to_string();
        if matches!(kind, Scope::Global) || id.is_empty() {
            return Self::global();
        }
        Self { kind, id }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossroadOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub tradeoffs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossroadStatus {
    Open,
    Resolved,
    Cancelled,
    Reopened,
}

impl CrossroadStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Cancelled => "cancelled",
            Self::Reopened => "reopened",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crossroad {
    pub id: String,
    #[serde(default)]
    pub request_ref: Option<String>,
    #[serde(default)]
    pub task_ref: Option<String>,
    #[serde(default)]
    pub project_ref: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub question: String,
    pub options: Vec<CrossroadOption>,
    #[serde(default)]
    pub recommended_option: Option<String>,
    #[serde(default)]
    pub affected_boundary_ids: Vec<String>,
    #[serde(default)]
    pub constraint_ids: Vec<String>,
    #[serde(default)]
    pub expected_tradeoffs: Vec<String>,
    #[serde(default)]
    pub authority_required: Option<Authority>,
    pub status: CrossroadStatus,
    pub actor: Actor,
    #[serde(default)]
    pub source_refs: Vec<SourceReference>,
    #[serde(default)]
    pub linked_scratchpad_wait_id: Option<String>,
    pub timestamps: Timestamps,
    #[serde(default)]
    pub privacy: PrivacyClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    User,
    Agent,
    System,
    Collaborative,
}

impl Actor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::System => "system",
            Self::Collaborative => "collaborative",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    NotRequired,
    Pending,
    Approved,
    Rejected,
    Acknowledged,
}

impl ApprovalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Acknowledged => "acknowledged",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Proposed,
    Accepted,
    Rejected,
    Superseded,
    Reopened,
}

impl DecisionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Superseded => "superseded",
            Self::Reopened => "reopened",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    #[serde(default)]
    pub parent_crossroad_id: Option<String>,
    pub selected_option: String,
    #[serde(default)]
    pub unselected_options: Vec<String>,
    pub actor: Actor,
    pub approval_state: ApprovalState,
    pub authority_basis: Authority,
    pub summary: String,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub assumptions: Vec<String>,
    #[serde(default)]
    pub tradeoffs: Vec<String>,
    #[serde(default)]
    pub request_ref: Option<String>,
    #[serde(default)]
    pub task_ref: Option<String>,
    #[serde(default)]
    pub project_ref: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub source_session_id: Option<String>,
    #[serde(default)]
    pub source_turn_id: Option<String>,
    #[serde(default)]
    pub related_preference_boundary_ids: Vec<String>,
    #[serde(default)]
    pub related_constraint_ids: Vec<String>,
    #[serde(default)]
    pub warrant_id: Option<String>,
    #[serde(default)]
    pub change_set_ids: Vec<String>,
    pub status: DecisionStatus,
    pub timestamps: Timestamps,
    #[serde(default)]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub reopened_as: Option<String>,
    #[serde(default)]
    pub privacy: PrivacyClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warrant {
    pub id: String,
    pub decision_id: String,
    #[serde(default)]
    pub observations: Vec<String>,
    #[serde(default)]
    pub assumptions: Vec<String>,
    #[serde(default)]
    pub priorities: Vec<String>,
    #[serde(default)]
    pub evidence_refs: Vec<SourceReference>,
    #[serde(default)]
    pub tradeoffs: Vec<String>,
    #[serde(default)]
    pub uncertainty: Option<String>,
    #[serde(default)]
    pub qualifier: Option<String>,
    pub timestamps: Timestamps,
    #[serde(default)]
    pub privacy: PrivacyClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub id: String,
    #[serde(default)]
    pub decision_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub scratchpad_id: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub git_intent_note_ref: Option<String>,
    #[serde(default)]
    pub pull_request: Option<String>,
    #[serde(default)]
    pub issue: Option<String>,
    #[serde(default)]
    pub test_runs: Vec<String>,
    #[serde(default)]
    pub deployment_result: Option<String>,
    #[serde(default)]
    pub later_failure_or_rollback: Option<String>,
    #[serde(default)]
    pub source_refs: Vec<SourceReference>,
    pub timestamps: Timestamps,
    #[serde(default)]
    pub privacy: PrivacyClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    PreferenceBoundary,
    Crossroad,
    Decision,
    Warrant,
    ChangeSet,
    Relationship,
    Notification,
    Constraint,
    Session,
    Scratchpad,
    Commit,
    PullRequest,
    Issue,
    TestRun,
    Outcome,
}

impl EntityType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreferenceBoundary => "preference_boundary",
            Self::Crossroad => "crossroad",
            Self::Decision => "decision",
            Self::Warrant => "warrant",
            Self::ChangeSet => "change_set",
            Self::Relationship => "relationship",
            Self::Notification => "notification",
            Self::Constraint => "constraint",
            Self::Session => "session",
            Self::Scratchpad => "scratchpad",
            Self::Commit => "commit",
            Self::PullRequest => "pull_request",
            Self::Issue => "issue",
            Self::TestRun => "test_run",
            Self::Outcome => "outcome",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipKind {
    DerivedFrom,
    ConstrainedBy,
    InfluencedBy,
    TradeoffAgainst,
    ConfirmedBy,
    ConsideredNotDecisive,
    ConflictsWith,
    Supersedes,
    ReopenedAs,
    ImplementedBy,
    ReviewedBy,
    ValidatedBy,
    FailedIn,
    CausedBy,
}

impl RelationshipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DerivedFrom => "derived_from",
            Self::ConstrainedBy => "constrained_by",
            Self::InfluencedBy => "influenced_by",
            Self::TradeoffAgainst => "tradeoff_against",
            Self::ConfirmedBy => "confirmed_by",
            Self::ConsideredNotDecisive => "considered_not_decisive",
            Self::ConflictsWith => "conflicts_with",
            Self::Supersedes => "supersedes",
            Self::ReopenedAs => "reopened_as",
            Self::ImplementedBy => "implemented_by",
            Self::ReviewedBy => "reviewed_by",
            Self::ValidatedBy => "validated_by",
            Self::FailedIn => "failed_in",
            Self::CausedBy => "caused_by",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipEvidence {
    Explicit,
    Inferred,
    Considered,
}

impl RelationshipEvidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Inferred => "inferred",
            Self::Considered => "considered",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceRelationship {
    pub id: String,
    pub from_type: EntityType,
    pub from_id: String,
    pub relation: RelationshipKind,
    pub to_type: EntityType,
    pub to_id: String,
    pub evidence: RelationshipEvidence,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub source_refs: Vec<SourceReference>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub privacy: PrivacyClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryTransitionKind {
    Confirm,
    Activate,
    Narrow,
    Broaden,
    Withdraw,
    Supersede,
}

impl BoundaryTransitionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirm => "confirm",
            Self::Activate => "activate",
            Self::Narrow => "narrow",
            Self::Broaden => "broaden",
            Self::Withdraw => "withdraw",
            Self::Supersede => "supersede",
        }
    }

    pub fn status(self) -> LifecycleStatus {
        match self {
            Self::Confirm => LifecycleStatus::Confirmed,
            Self::Activate => LifecycleStatus::Active,
            Self::Narrow => LifecycleStatus::Narrowed,
            Self::Broaden => LifecycleStatus::Broadened,
            Self::Withdraw => LifecycleStatus::Withdrawn,
            Self::Supersede => LifecycleStatus::Superseded,
        }
    }

    pub fn relationship(self) -> Option<RelationshipKind> {
        match self {
            Self::Confirm => Some(RelationshipKind::ConfirmedBy),
            Self::Activate => None,
            Self::Narrow => Some(RelationshipKind::Supersedes),
            Self::Broaden => Some(RelationshipKind::Supersedes),
            Self::Withdraw => None,
            Self::Supersede => Some(RelationshipKind::Supersedes),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryTransition {
    pub boundary_id: String,
    pub transition: BoundaryTransitionKind,
    #[serde(default)]
    pub replacement: Option<PreferenceBoundary>,
    pub actor: Actor,
    #[serde(default)]
    pub source: Option<SourceReference>,
}
