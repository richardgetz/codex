use super::shared::v2_enum_from_core;
use crate::JsonSchema;
use crate::TS;
use codex_experimental_api_macros::ExperimentalApi;
use serde::Deserialize;
use serde::Serialize;

v2_enum_from_core!(
    #[ts(rename_all = "camelCase")]
    pub enum ThreadHandoffBlocker from codex_protocol::turn_input::HandoffBlocker {
        UnsupportedTask,
        ActiveOperation,
        PendingApproval,
        PendingUserInput,
        PendingDynamicToolResponse,
        PendingInput,
        PendingMailbox,
        PendingDispatch,
        UnifiedExecProcess,
        LiveDescendants,
        Persistence,
        VersionMismatch,
        ParentUnavailable,
        RealtimeConversation,
        SuspensionTimeout,
        TaskExitedUnexpectedly,
        TurnFinalization
    }
);

/// Lifecycle state of a process handoff attempt.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadHandoffState {
    Prepared,
    Draining,
    Suspended,
    Restoring,
    Completed,
    NeedsAttention,
}

/// Lifecycle state of one thread in a handoff receipt.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadHandoffNodeState {
    Planned,
    Suspending,
    Suspended,
    NotActive,
    Recovering,
    Restored,
    Paused,
    NeedsAttention,
}

/// Stable thread identity and process-local state captured for recovery.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadHandoffNode {
    pub thread_id: String,
    pub root_thread_id: String,
    pub parent_thread_id: Option<String>,
    pub agent_path: Option<String>,
    pub turn_id: Option<String>,
    pub rollout_path: Option<String>,
    pub was_running: bool,
    pub was_paused: bool,
    pub state: ThreadHandoffNodeState,
    pub blockers: Vec<ThreadHandoffBlocker>,
}

/// Durable receipt for a process handoff.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadHandoffReceipt {
    pub handoff_id: String,
    pub state: ThreadHandoffState,
    pub runtime_version: String,
    /// Creation time as integer Unix seconds.
    #[ts(type = "number")]
    pub created_at: i64,
    pub nodes: Vec<ThreadHandoffNode>,
}

#[derive(
    Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadHandoffPrepareParams {
    /// Omit to prepare every loaded root; a value limits the attempt to one loaded root tree.
    #[ts(optional = nullable)]
    pub root_thread_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadHandoffPrepareResponse {
    pub receipt: ThreadHandoffReceipt,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadHandoffStatusParams {
    pub handoff_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadHandoffStatusResponse {
    pub receipt: ThreadHandoffReceipt,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadHandoffRecoverParams {
    pub handoff_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadHandoffRecoverResponse {
    pub receipt: ThreadHandoffReceipt,
}

#[cfg(test)]
mod tests {
    use super::ThreadHandoffPrepareParams;
    use crate::{ClientRequest, JSONRPCRequest, RequestId};

    #[test]
    fn prepare_request_defaults_when_params_are_omitted() {
        let request = JSONRPCRequest {
            id: RequestId::Integer(1),
            method: "thread/handoff/prepare".to_string(),
            params: None,
            trace: None,
        };

        let parsed = ClientRequest::try_from(request).expect("prepare request should deserialize");
        let ClientRequest::ThreadHandoffPrepare { params, .. } = parsed else {
            panic!("expected handoff prepare request");
        };
        assert_eq!(params, ThreadHandoffPrepareParams::default());
    }
}
