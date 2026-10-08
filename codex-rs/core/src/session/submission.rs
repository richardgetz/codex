//! Core-owned queue metadata; forwarding transfers the residency guard with the operation.

use codex_extension_api::ExtensionDataInit;
use crate::agent::control::HandoffAdmissionGuard;
use crate::realtime_conversation::RealtimeHandoffAdmission;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::W3cTraceContext;
use std::sync::Arc;
use tokio::sync::OwnedRwLockWriteGuard;
use tokio::sync::OwnedRwLockReadGuard;
use tokio::sync::OwnedSemaphorePermit;

pub(crate) const REALTIME_RESERVED_SUBMISSION_CAPACITY: usize = 1;

/// Carries realtime-only turn options without adding them to the public request shape.
#[derive(Debug)]
pub(crate) struct RealtimeHandoffInput {
    pub(crate) admission: Arc<RealtimeHandoffAdmission>,
    pub(crate) transient_reasoning_effort: Option<ReasoningEffort>,
}

/// Bounds ordinary queue admission and orders it around lifecycle operations.
#[derive(Debug)]
pub(crate) struct OrdinarySubmissionPermit {
    pub(crate) slot: OwnedSemaphorePermit,
    pub(crate) gate_read: Option<OwnedRwLockReadGuard<()>>,
    pub(crate) gate_write: Option<OwnedRwLockWriteGuard<()>>,
}

#[derive(Debug)]
#[expect(dead_code, reason = "Turn ancestry is retained in Debug diagnostics.")]
pub(crate) struct Submission {
    pub id: String,
    pub op: Op,
    /// Client message identity associated with a submitted user operation.
    pub client_user_message_id: Option<String>,
    /// Accepted with settings, never separately from the request that supplied them.
    pub turn_extension_init: Option<ExtensionDataInit>,
    /// Optional W3C trace carrier propagated across async submission handoffs.
    pub trace: Option<W3cTraceContext>,
    pub parent_turn_id: Option<String>,
    pub root_turn_id: Option<String>,
    /// Keeps a V2 recipient resident until this submission is handled or dropped.
    pub residency_guard: Option<OwnedRwLockReadGuard<()>>,
    /// Carries an already admitted operation through the session queue and handler.
    pub handoff_admission: Option<HandoffAdmissionGuard>,
    /// Carries the originating realtime session gate and its transient turn effort.
    pub realtime_handoff_input: Option<RealtimeHandoffInput>,
    /// Holds an ordinary queue slot until dequeue. Ordinary submissions also retain a shared
    /// gate guard so lifecycle submissions cannot overtake them; a lifecycle submission retains
    /// the exclusive gate through its drain. Realtime handoffs omit this permit and use the one
    /// reserved channel slot.
    pub ordinary_slot_permit: Option<OrdinarySubmissionPermit>,
}
