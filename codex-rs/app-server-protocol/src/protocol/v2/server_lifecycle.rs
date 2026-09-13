use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Process-local lifecycle phase for a shared app-server daemon.
///
/// `Draining` means the server has begun a graceful restart and will reject
/// new work admission while existing assistant turns drain. `Forced` means a
/// second forceable shutdown signal arrived; the server may terminate before
/// all work completes. Neither phase is a durable thread checkpoint.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ServerLifecyclePhase {
    Ready,
    Draining,
    Forced,
}

/// Request the current process-local app-server lifecycle state.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ServerLifecycleReadParams {}

/// Current process-local app-server lifecycle state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ServerLifecycleReadResponse {
    /// UUID generated for this app-server process. It changes after restart.
    pub daemon_instance_id: String,
    pub phase: ServerLifecyclePhase,
    /// UUID shared by lifecycle updates for one graceful drain attempt.
    /// It is absent while the process is ready.
    pub transition_id: Option<String>,
    /// Number of assistant turns currently reported as running by this process.
    #[ts(type = "number")]
    pub running_assistant_turns: u32,
}

/// Emitted when the process lifecycle phase or draining turn count changes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ServerLifecycleUpdatedNotification {
    /// UUID generated for this app-server process. It changes after restart.
    pub daemon_instance_id: String,
    pub phase: ServerLifecyclePhase,
    /// UUID shared by lifecycle updates for one graceful drain attempt.
    /// It is absent while the process is ready.
    pub transition_id: Option<String>,
    /// Number of assistant turns currently reported as running by this process.
    #[ts(type = "number")]
    pub running_assistant_turns: u32,
}
