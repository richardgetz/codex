//! Local decision provenance and crossroads.
//!
//! This module records decision-relevant summaries and relationships. Rollouts
//! remain the transcript, scratchpads remain operational state, user-preference
//! memory remains reusable guidance, and Git intent notes remain commit intent.

mod model;
mod projection;
mod query;
mod sanitize;

pub use model::Actor;
pub use model::ApprovalState;
pub use model::Authority;
pub use model::BoundaryTransition;
pub use model::BoundaryTransitionKind;
pub use model::ChangeSet;
pub use model::Crossroad;
pub use model::CrossroadOption;
pub use model::CrossroadStatus;
pub use model::Decision;
pub use model::DecisionStatus;
pub use model::EntityType;
pub use model::LifecycleStatus;
pub use model::NotificationCategory;
pub use model::PreferenceBoundary;
pub use model::PreferenceBoundaryPreflight;
pub use model::PreferenceKind;
pub use model::PreferenceStrength;
pub use model::PrivacyClass;
pub use model::ProvenanceNotification;
pub use model::ProvenanceRelationship;
pub use model::RelationshipEvidence;
pub use model::RelationshipKind;
pub use model::Scope;
pub use model::ScopeRef;
pub use model::SourceReference;
pub use model::Timestamps;
pub use model::Warrant;
pub(crate) use projection::build_projection;
pub(crate) use projection::write_projection_atomically;
pub use query::AppendProvenanceEventResult;
pub use query::CrossroadFilter;
pub use query::DecisionFilter;
pub use query::DecisionWhy;
pub use query::EventSummary;
pub use query::PreferenceBoundaryFilter;
pub use query::ProvenanceEvent;
pub use query::ProvenanceEventPayload;
pub use query::ProvenanceEventType;
pub use query::ProvenanceIndexes;
pub use query::ProvenanceProjection;
pub use query::ProvenanceWriteOptions;
pub use query::new_id;
pub use query::now;
pub(crate) use sanitize::sanitize_event;

pub const PROVENANCE_PROJECTION_VERSION: u32 = 1;
pub const PROVENANCE_EVENT_VERSION: u32 = 1;
pub const PROVENANCE_PROJECTION_DIRECTORY: &str = "decision-provenance";
pub const PROVENANCE_PROJECTION_FILENAME: &str = "projection-v1.json";
pub const MAX_QUERY_RESULTS: usize = 100;
