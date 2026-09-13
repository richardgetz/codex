use crate::JsonSchema;
use crate::TS;
use codex_experimental_api_macros::ExperimentalApi;
use serde::Deserialize;
use serde::Serialize;

/// Lifecycle state for a task tracked by the ETA store.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadEtaStatus {
    Pending,
    Active,
    Blocked,
    Completed,
    Cancelled,
    Unknown,
}

/// Accuracy classification against a task's immutable original estimate.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadEtaAccuracy {
    Early,
    Within,
    Late,
    Unknown,
}

/// Mutation operation accepted by `thread/eta/update` and the model-facing ETA tool.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadEtaAction {
    Create,
    Start,
    Revise,
    Block,
    Complete,
    Cancel,
}

/// One bounded estimate revision retained for a task.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadEtaRevision {
    pub lower_seconds: Option<i64>,
    pub upper_seconds: Option<i64>,
    pub reason: Option<String>,
    pub updated_at: i64,
    pub actor_thread_id: String,
}

/// A task in the root session's active tree or bounded history page.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadEtaTask {
    pub task_id: String,
    pub root_thread_id: String,
    pub owner_thread_id: String,
    pub parent_task_id: Option<String>,
    pub depends_on_task_ids: Vec<String>,
    pub title: String,
    pub status: ThreadEtaStatus,
    pub current_lower_seconds: Option<i64>,
    pub current_upper_seconds: Option<i64>,
    pub original_lower_seconds: Option<i64>,
    pub original_upper_seconds: Option<i64>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub terminal_at: Option<i64>,
    pub actual_elapsed_seconds: Option<i64>,
    pub updated_at: i64,
    pub is_stale: bool,
    pub accuracy: ThreadEtaAccuracy,
    pub revisions: Vec<ThreadEtaRevision>,
}

/// Aggregate remaining work for the selected root session.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadEtaOverall {
    pub finish_at: Option<i64>,
    pub remaining_lower_seconds: Option<i64>,
    pub remaining_upper_seconds: Option<i64>,
    pub unknown_reason: Option<String>,
}

/// Read-only root-scoped ETA projection returned by `thread/eta/read`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadEtaSnapshot {
    pub root_thread_id: String,
    pub generated_at: i64,
    pub sequence: i64,
    pub active: Vec<ThreadEtaTask>,
    pub history: Vec<ThreadEtaTask>,
    pub next_cursor: Option<String>,
    pub overall: ThreadEtaOverall,
}

/// Parameters for the paginated, read-only ETA projection.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadEtaReadParams {
    pub thread_id: String,
    /// Opaque history cursor returned by a previous call.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Optional history page size, bounded by the server to 100 entries.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadEtaReadResponse {
    pub snapshot: ThreadEtaSnapshot,
}

/// One requested ETA mutation. Fields omitted for an action are left unchanged;
/// `create` requires a title and may omit an estimate to record an explicit unknown.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadEtaUpdateOperation {
    pub action: ThreadEtaAction,
    #[ts(optional = nullable)]
    pub task_id: Option<String>,
    #[ts(optional = nullable)]
    pub title: Option<String>,
    #[ts(optional = nullable)]
    pub parent_task_id: Option<String>,
    #[ts(optional = nullable)]
    pub depends_on_task_ids: Option<Vec<String>>,
    #[ts(optional = nullable)]
    pub estimate_lower_seconds: Option<i64>,
    #[ts(optional = nullable)]
    pub estimate_upper_seconds: Option<i64>,
    #[ts(optional = nullable)]
    pub reason: Option<String>,
}

/// Parameters for mutating tasks owned by the selected root session.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadEtaUpdateParams {
    pub thread_id: String,
    pub operations: Vec<ThreadEtaUpdateOperation>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadEtaUpdateResponse {
    pub root_thread_id: String,
    pub generated_at: i64,
    pub sequence: i64,
    pub changed_tasks: Vec<ThreadEtaTask>,
    pub overall: ThreadEtaOverall,
}

/// Event-driven update emitted after a task mutation. Clients should refresh the
/// read projection only when this sequence is newer than their local snapshot.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadEtaUpdatedNotification {
    pub root_thread_id: String,
    pub generated_at: i64,
    pub sequence: i64,
    pub changed_tasks: Vec<ThreadEtaTask>,
    pub overall: ThreadEtaOverall,
}
