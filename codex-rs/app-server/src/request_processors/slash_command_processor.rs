use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SlashCommandCapabilities;
use codex_app_server_protocol::SlashCommandExecuteParams;
use codex_app_server_protocol::SlashCommandExecuteResponse;
use codex_app_server_protocol::SlashCommandListParams;
use codex_app_server_protocol::SlashCommandListResponse;
use codex_app_server_protocol::SlashCommandOutput;
use codex_app_server_protocol::SlashCommandReloadResult;
use codex_app_server_protocol::SlashCommandResultKind;
use codex_app_server_protocol::SlashCommandResultNotification;
use codex_app_server_protocol::SlashCommandResultPayload;
use codex_app_server_protocol::SlashCommandSpec;
use codex_app_server_protocol::ThreadActivityContinueParams;
use codex_app_server_protocol::ThreadActivityPauseParams;
use codex_app_server_transport::APP_SERVER_DAEMON_MANAGED_ENV;
use codex_app_server_transport::APP_SERVER_DAEMON_RELOAD_ENV;

use super::AccountRequestProcessor;
use super::ConnectionRequestId;
use super::OutgoingMessageSender;
use super::TurnRequestProcessor;
use super::invalid_request;
use crate::server_lifecycle::NEW_WORK_REJECTED_MESSAGE;
use crate::server_lifecycle::ServerLifecycle;

const MAX_OUTPUT_CHARS: usize = 200_000;
const RELOAD_HANDOFF_DELAY: Duration = Duration::from_millis(250);

type ReloadLauncher = dyn Fn(std::path::PathBuf, ReloadOperation) -> std::io::Result<tokio::process::Child>
    + Send
    + Sync
    + 'static;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReloadOperation {
    Apply,
    Recover,
}

#[derive(Clone)]
struct ReloadNotificationTarget {
    outgoing: Arc<OutgoingMessageSender>,
    request_id: ConnectionRequestId,
    thread_id: String,
}

#[derive(Clone)]
pub(crate) struct SlashCommandRequestProcessor {
    account_processor: AccountRequestProcessor,
    outgoing: Arc<OutgoingMessageSender>,
    reload_scheduled: Arc<AtomicBool>,
    server_lifecycle: Arc<ServerLifecycle>,
    turn_processor: TurnRequestProcessor,
}

impl SlashCommandRequestProcessor {
    pub(crate) fn new(
        account_processor: AccountRequestProcessor,
        turn_processor: TurnRequestProcessor,
        outgoing: Arc<OutgoingMessageSender>,
        server_lifecycle: Arc<ServerLifecycle>,
    ) -> Self {
        Self {
            account_processor,
            outgoing,
            reload_scheduled: Arc::new(AtomicBool::new(false)),
            server_lifecycle,
            turn_processor,
        }
    }

    pub(crate) fn list(&self, _params: SlashCommandListParams) -> SlashCommandListResponse {
        let reload_available = daemon_reload_available();
        SlashCommandListResponse {
            commands: command_specs(reload_available),
            capabilities: SlashCommandCapabilities {
                status: true,
                spend: true,
                reload: reload_available,
            },
        }
    }

    pub(crate) async fn execute(
        &self,
        request_id: &ConnectionRequestId,
        params: SlashCommandExecuteParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let command = normalize_command(&params.command)?;
        let thread_id = params.thread_id.clone();
        if matches!(
            command.as_str(),
            "status" | "spend" | "usage" | "pause" | "continue"
        ) && !params.args.trim().is_empty()
        {
            let error = invalid_request(format!(
                "`/{command}` does not accept inline arguments in the app-server bridge"
            ));
            let notification = error_result(&command, &error);
            self.send_result_notification(request_id, &thread_id, &notification)
                .await;
            return Err(error);
        }
        if command == "continue"
            && self
                .server_lifecycle
                .rejects_new_work("thread/activity/continue")
        {
            let error = invalid_request(NEW_WORK_REJECTED_MESSAGE);
            let notification = error_result(&command, &error);
            self.send_result_notification(request_id, &thread_id, &notification)
                .await;
            return Err(error);
        }
        let result = match command.as_str() {
            "status" => match self.status_result().await {
                Ok(result) => result,
                Err(error) => {
                    let notification = error_result(&command, &error);
                    self.send_result_notification(request_id, &thread_id, &notification)
                        .await;
                    return Err(error);
                }
            },
            "spend" | "usage" => match self.spend_result(&command).await {
                Ok(result) => result,
                Err(error) => {
                    let notification = error_result(&command, &error);
                    self.send_result_notification(request_id, &thread_id, &notification)
                        .await;
                    return Err(error);
                }
            },
            "pause" | "continue" => {
                match self.activity_result(&command, request_id, &thread_id).await {
                    Ok(result) => result,
                    Err(error) => {
                        let notification = error_result(&command, &error);
                        self.send_result_notification(request_id, &thread_id, &notification)
                            .await;
                        return Err(error);
                    }
                }
            }
            "reload" => {
                self.reload_result(&params.args, request_id, &thread_id)
                    .await
            }
            _ => SlashCommandExecuteResponse {
                command: command.clone(),
                ok: false,
                result_kind: SlashCommandResultKind::Error,
                output: SlashCommandOutput {
                    format: "markdown".to_string(),
                    text: format!("Unknown slash command `/{command}`."),
                },
                reload: None,
            },
        };

        self.send_result_notification(request_id, &thread_id, &result)
            .await;
        Ok(Some(result.into()))
    }

    async fn activity_result(
        &self,
        command: &str,
        request_id: &ConnectionRequestId,
        thread_id: &str,
    ) -> Result<SlashCommandExecuteResponse, JSONRPCErrorError> {
        match command {
            "pause" => {
                self.turn_processor
                    .thread_activity_pause(
                        request_id,
                        ThreadActivityPauseParams {
                            thread_id: thread_id.to_string(),
                        },
                    )
                    .await
            }
            "continue" => {
                self.turn_processor
                    .thread_activity_continue(
                        request_id,
                        ThreadActivityContinueParams {
                            thread_id: thread_id.to_string(),
                        },
                    )
                    .await
            }
            _ => unreachable!("activity_result called for unsupported command {command}"),
        }?;
        Ok(SlashCommandExecuteResponse {
            command: command.to_string(),
            ok: true,
            result_kind: SlashCommandResultKind::Text,
            output: SlashCommandOutput {
                format: "markdown".to_string(),
                text: format!("`/{command}` completed successfully."),
            },
            reload: None,
        })
    }

    async fn send_result_notification(
        &self,
        request_id: &ConnectionRequestId,
        thread_id: &str,
        result: &SlashCommandExecuteResponse,
    ) {
        self.outgoing
            .send_server_notification_to_connections(
                std::slice::from_ref(&request_id.connection_id),
                ServerNotification::SlashCommandResult(SlashCommandResultNotification {
                    thread_id: thread_id.to_string(),
                    command: result.command.clone(),
                    request_id: request_id.request_id.clone(),
                    result: result_payload(result),
                }),
            )
            .await;
    }

    async fn reload_result(
        &self,
        args: &str,
        request_id: &ConnectionRequestId,
        thread_id: &str,
    ) -> SlashCommandExecuteResponse {
        if !daemon_reload_available() {
            return unavailable_reload_result(
                "Reload requires an app-server process launched by the managed local daemon with an explicitly configured local Codex launcher.",
            );
        }
        match args.trim().to_ascii_lowercase().as_str() {
            "status" => return self.reload_status_result().await,
            "recover" => {}
            "" => {}
            operation => {
                return failed_reload_result(format!(
                    "Unsupported /reload operation `{operation}`; use `/reload`, `/reload status`, or `/reload recover`."
                ));
            }
        }
        if !reserve_reload(&self.reload_scheduled) {
            return unavailable_reload_result(
                "A Codex reload is already scheduled for this app-server session.",
            );
        }
        let executable = match resolve_current_executable() {
            Ok(executable) => executable,
            Err(reason) => {
                self.reload_scheduled.store(false, Ordering::Release);
                return unavailable_reload_result(&reason);
            }
        };

        let operation = if args.trim().eq_ignore_ascii_case("recover") {
            ReloadOperation::Recover
        } else {
            ReloadOperation::Apply
        };
        let reload_target = ReloadNotificationTarget {
            outgoing: Arc::clone(&self.outgoing),
            request_id: request_id.clone(),
            thread_id: thread_id.to_string(),
        };
        let _reload_task = schedule_reload(
            Arc::clone(&self.reload_scheduled),
            executable,
            operation,
            Arc::new(spawn_reload_daemon),
            Some(reload_target),
        );

        SlashCommandExecuteResponse {
            command: "reload".to_string(),
            ok: true,
            result_kind: SlashCommandResultKind::Reload,
            output: SlashCommandOutput {
                format: "markdown".to_string(),
                text: format!(
                    "`/reload {}` was accepted by the managed daemon. The operation is not complete yet; use `/reload status` to observe the durable handoff receipt. The daemon pauses active turns, replaces the configured launcher, and recovers exact paused turns.",
                    if matches!(operation, ReloadOperation::Recover) {
                        "recover"
                    } else {
                        "apply"
                    }
                ),
            },
            reload: Some(SlashCommandReloadResult {
                eligible: true,
                state: "accepted".to_string(),
                reason: Some("The daemon owns pause, replacement, exact-turn recovery, and unresolved-failure receipts; completion is reported by `/reload status`.".to_string()),
            }),
        }
    }

    async fn reload_status_result(&self) -> SlashCommandExecuteResponse {
        let executable = match resolve_current_executable() {
            Ok(executable) => executable,
            Err(reason) => return failed_reload_result(reason),
        };
        let output = tokio::process::Command::new(executable)
            .args(reload_status_command_args())
            .stdin(Stdio::null())
            .output()
            .await;
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                return failed_reload_result(format!(
                    "The daemon handoff status could not be read: {error}."
                ));
            }
        };
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return failed_reload_result(if detail.is_empty() {
                format!(
                    "The daemon handoff status command exited with {}.",
                    output.status
                )
            } else {
                format!("The daemon handoff status could not be read: {detail}")
            });
        }
        let payload: serde_json::Value = match serde_json::from_slice(&output.stdout) {
            Ok(payload) => payload,
            Err(error) => {
                return failed_reload_result(format!(
                    "The daemon returned invalid handoff status JSON: {error}."
                ));
            }
        };
        let status = payload
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let handoff_id = payload.get("handoffId").and_then(serde_json::Value::as_str);
        let detail = payload
            .get("error")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let (ok, state, text) = match status {
            "applied" => (
                true,
                "completed",
                "The managed daemon completed the Codex handoff and recovered exact paused turns."
                    .to_string(),
            ),
            "inProgress" => (
                true,
                "in_progress",
                "The managed daemon is still reconciling the Codex handoff; retry `/reload status` shortly."
                    .to_string(),
            ),
            "needsAttention" => (
                false,
                "failed",
                detail.clone().unwrap_or_else(|| {
                    "The managed daemon needs explicit handoff recovery before another replacement."
                        .to_string()
                }),
            ),
            _ => (
                false,
                "unavailable",
                "The managed daemon has no recognized handoff status yet.".to_string(),
            ),
        };
        let operation = handoff_id
            .map(|id| format!(" Handoff `{id}`."))
            .unwrap_or_default();
        SlashCommandExecuteResponse {
            command: "reload".to_string(),
            ok,
            result_kind: SlashCommandResultKind::Reload,
            output: SlashCommandOutput {
                format: "markdown".to_string(),
                text: bounded_output(format!("{text}{operation}")),
            },
            reload: Some(SlashCommandReloadResult {
                eligible: true,
                state: state.to_string(),
                reason: detail,
            }),
        }
    }

    async fn status_result(&self) -> Result<SlashCommandExecuteResponse, JSONRPCErrorError> {
        let usage = self
            .account_processor
            .get_account_token_usage(None)
            .await?
            .ok_or_else(|| invalid_request("account usage response was unavailable"))?;
        let rate_limits = self
            .account_processor
            .get_account_rate_limits(None)
            .await?
            .ok_or_else(|| invalid_request("account rate-limit response was unavailable"))?;
        let usage_json = response_payload_json(usage)?;
        let rate_limits_json = response_payload_json(rate_limits)?;
        let text = bounded_output(format!(
            "## Status\n\n### Token usage\n```json\n{}\n```\n\n### Rate limits\n```json\n{}\n```",
            serde_json::to_string_pretty(&usage_json).map_err(internal_error)?,
            serde_json::to_string_pretty(&rate_limits_json).map_err(internal_error)?,
        ));
        Ok(SlashCommandExecuteResponse {
            command: "status".to_string(),
            ok: true,
            result_kind: SlashCommandResultKind::Status,
            output: SlashCommandOutput {
                format: "markdown".to_string(),
                text,
            },
            reload: None,
        })
    }

    async fn spend_result(
        &self,
        command: &str,
    ) -> Result<SlashCommandExecuteResponse, JSONRPCErrorError> {
        let usage = self
            .account_processor
            .get_account_token_usage(None)
            .await?
            .ok_or_else(|| invalid_request("account usage response was unavailable"))?;
        let usage_json = response_payload_json(usage)?;
        let text = bounded_output(format!(
            "## Spend\n\nThe configured account's token usage and daily trend are below. Estimated USD pricing is omitted when the provider does not return it.\n\n```json\n{}\n```",
            serde_json::to_string_pretty(&usage_json).map_err(internal_error)?,
        ));
        Ok(SlashCommandExecuteResponse {
            command: command.to_string(),
            ok: true,
            result_kind: SlashCommandResultKind::Spend,
            output: SlashCommandOutput {
                format: "markdown".to_string(),
                text,
            },
            reload: None,
        })
    }
}

fn normalize_command(command: &str) -> Result<String, JSONRPCErrorError> {
    let normalized = command.trim().trim_start_matches('/').to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(invalid_request("slash command must not be empty"));
    }
    Ok(normalized)
}

fn unavailable_reload_result(reason: &str) -> SlashCommandExecuteResponse {
    let reload = SlashCommandReloadResult {
        eligible: false,
        state: "unavailable".to_string(),
        reason: Some(reason.to_string()),
    };
    SlashCommandExecuteResponse {
        command: "reload".to_string(),
        ok: false,
        result_kind: SlashCommandResultKind::Reload,
        output: SlashCommandOutput {
            format: "markdown".to_string(),
            text: format!("`/reload` is unavailable for this app-server session. {reason}"),
        },
        reload: Some(reload),
    }
}

fn failed_reload_result(reason: impl Into<String>) -> SlashCommandExecuteResponse {
    let reason = reason.into();
    SlashCommandExecuteResponse {
        command: "reload".to_string(),
        ok: false,
        result_kind: SlashCommandResultKind::Reload,
        output: SlashCommandOutput {
            format: "markdown".to_string(),
            text: format!("`/reload` failed: {reason}"),
        },
        reload: Some(SlashCommandReloadResult {
            eligible: true,
            state: "failed".to_string(),
            reason: Some(reason),
        }),
    }
}

fn daemon_reload_available() -> bool {
    env_is_one(APP_SERVER_DAEMON_MANAGED_ENV) && env_is_one(APP_SERVER_DAEMON_RELOAD_ENV)
}

fn env_is_one(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| value == "1")
}

fn resolve_current_executable() -> Result<std::path::PathBuf, String> {
    match std::env::current_exe() {
        Ok(executable) if executable.is_file() => Ok(executable),
        Ok(executable) => Err(format!(
            "The current Codex launcher is not a file: {}.",
            executable.display()
        )),
        Err(error) => Err(format!(
            "The current Codex launcher could not be resolved: {error}."
        )),
    }
}

fn spawn_reload_daemon(
    executable: std::path::PathBuf,
    operation: ReloadOperation,
) -> std::io::Result<tokio::process::Child> {
    tokio::process::Command::new(executable)
        .args(reload_command_args(operation))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn schedule_reload(
    reload_scheduled: Arc<AtomicBool>,
    executable: std::path::PathBuf,
    operation: ReloadOperation,
    launcher: Arc<ReloadLauncher>,
    notification_target: Option<ReloadNotificationTarget>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(RELOAD_HANDOFF_DELAY).await;
        let child = match launcher(executable, operation) {
            Ok(child) => child,
            Err(error) => {
                report_reload_failure(
                    &reload_scheduled,
                    notification_target,
                    format!("failed to start managed Codex reload: {error}"),
                )
                .await;
                return;
            }
        };
        let output = match child.wait_with_output().await {
            Ok(output) => output,
            Err(error) => {
                report_reload_failure(
                    &reload_scheduled,
                    notification_target,
                    format!("managed Codex reload process failed: {error}"),
                )
                .await;
                return;
            }
        };
        let status = output
            .status
            .success()
            .then(|| serde_json::from_slice::<serde_json::Value>(&output.stdout).ok())
            .flatten()
            .and_then(|value| {
                value
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            });
        match status.as_deref() {
            Some("applied") => reload_scheduled.store(false, Ordering::Release),
            Some("inProgress") => {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let detail = if detail.is_empty() {
                    "the daemon is still reconciling the handoff".to_string()
                } else {
                    detail
                };
                report_reload_progress(&reload_scheduled, notification_target, detail).await;
            }
            _ => {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let reason = if !detail.is_empty() {
                    detail
                } else if let Some(status) = status {
                    format!("daemon returned handoff status `{status}`")
                } else if output.status.success() {
                    "daemon returned no applied handoff status".to_string()
                } else {
                    format!("daemon exited with {}", output.status)
                };
                report_reload_failure(&reload_scheduled, notification_target, reason).await;
            }
        }
    })
}

async fn report_reload_progress(
    reload_scheduled: &AtomicBool,
    notification_target: Option<ReloadNotificationTarget>,
    detail: String,
) {
    reload_scheduled.store(false, Ordering::Release);
    let Some(target) = notification_target else {
        return;
    };
    let result = SlashCommandExecuteResponse {
        command: "reload".to_string(),
        ok: true,
        result_kind: SlashCommandResultKind::Reload,
        output: SlashCommandOutput {
            format: "markdown".to_string(),
            text: bounded_output(format!(
                "`/reload` remains in progress: {detail}. Use `/reload status` or `/reload recover` after the daemon reconnects."
            )),
        },
        reload: Some(SlashCommandReloadResult {
            eligible: true,
            state: "in_progress".to_string(),
            reason: Some(detail),
        }),
    };
    target
        .outgoing
        .send_server_notification_to_connections(
            std::slice::from_ref(&target.request_id.connection_id),
            ServerNotification::SlashCommandResult(SlashCommandResultNotification {
                thread_id: target.thread_id,
                command: result.command.clone(),
                request_id: target.request_id.request_id.clone(),
                result: result_payload(&result),
            }),
        )
        .await;
}

async fn report_reload_failure(
    reload_scheduled: &AtomicBool,
    notification_target: Option<ReloadNotificationTarget>,
    reason: String,
) {
    reload_scheduled.store(false, Ordering::Release);
    tracing::error!(reason = %reason, "managed Codex reload did not complete");
    let Some(target) = notification_target else {
        return;
    };
    let result = failed_reload_result(reason);
    target
        .outgoing
        .send_server_notification_to_connections(
            std::slice::from_ref(&target.request_id.connection_id),
            ServerNotification::SlashCommandResult(SlashCommandResultNotification {
                thread_id: target.thread_id,
                command: result.command.clone(),
                request_id: target.request_id.request_id.clone(),
                result: result_payload(&result),
            }),
        )
        .await;
}

fn reserve_reload(reload_scheduled: &AtomicBool) -> bool {
    reload_scheduled
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

fn reload_command_args(operation: ReloadOperation) -> [&'static str; 3] {
    [
        "app-server",
        "daemon",
        match operation {
            ReloadOperation::Apply => "apply",
            ReloadOperation::Recover => "recover",
        },
    ]
}

fn reload_status_command_args() -> [&'static str; 3] {
    ["app-server", "daemon", "apply-status"]
}

fn response_payload_json(
    payload: ClientResponsePayload,
) -> Result<serde_json::Value, JSONRPCErrorError> {
    payload
        .to_jsonrpc_parts(RequestId::Integer(0))
        .map(|(_, value)| value)
        .map_err(internal_error)
}

fn bounded_output(mut text: String) -> String {
    if text.chars().count() > MAX_OUTPUT_CHARS {
        let end = text
            .char_indices()
            .nth(MAX_OUTPUT_CHARS)
            .map_or(text.len(), |(index, _)| index);
        text.truncate(end);
        text.push_str("\n\n_Output truncated by the host._");
    }
    text
}

fn result_payload(result: &SlashCommandExecuteResponse) -> SlashCommandResultPayload {
    SlashCommandResultPayload {
        command: result.command.clone(),
        ok: result.ok,
        result_kind: result.result_kind.clone(),
        output: result.output.clone(),
        reload: result.reload.clone(),
    }
}

fn error_result(command: &str, error: &JSONRPCErrorError) -> SlashCommandExecuteResponse {
    SlashCommandExecuteResponse {
        command: command.to_string(),
        ok: false,
        result_kind: SlashCommandResultKind::Error,
        output: SlashCommandOutput {
            format: "markdown".to_string(),
            text: format!("`/{command}` failed: {}", error.message),
        },
        reload: None,
    }
}

fn internal_error(error: impl std::fmt::Display) -> JSONRPCErrorError {
    crate::error_code::internal_error(error.to_string())
}

fn command_specs(reload_available: bool) -> Vec<SlashCommandSpec> {
    let mut commands: Vec<SlashCommandSpec> = [
        (
            "status",
            "show current session configuration and token usage",
            false,
        ),
        ("spend", "show daily token usage and trends", false),
        ("reload", "reload the latest installed Codex safely", true),
        ("model", "switch model", true),
        ("permissions", "change permissions", true),
        ("plan", "enter plan mode", false),
        ("compact", "compact the conversation", true),
        ("recap", "summarize the conversation", false),
        ("new", "start a new conversation", true),
        ("resume", "resume a conversation", true),
        ("fork", "fork the conversation", true),
        ("archive", "archive the conversation", false),
        ("delete", "delete the conversation", false),
        ("review", "review changes", true),
        ("rename", "rename the conversation", true),
        ("team", "configure the model team", true),
        ("goal", "configure the current goal", true),
        ("eta", "show task estimates", true),
        ("pause", "pause activity", false),
        ("continue", "continue activity", false),
        ("skills", "list skills", false),
        ("apps", "list apps", false),
        ("plugins", "list plugins", false),
        ("mcp", "list MCP servers", true),
        ("account", "switch account", true),
        ("logout", "log out", false),
        ("pwd", "show the working directory", false),
        ("cd", "change the working directory", true),
        ("diff", "show the current diff", false),
        ("export", "export the conversation", true),
        ("raw", "toggle raw output", true),
        ("clear", "clear the conversation", false),
        ("stop", "stop the current turn", false),
        ("quit", "quit Codex", false),
    ]
    .into_iter()
    .map(
        |(name, description, supports_inline_args)| SlashCommandSpec {
            name: name.to_string(),
            aliases: Vec::new(),
            description: description.to_string(),
            supports_inline_args,
            available: matches!(name, "status" | "spend" | "pause" | "continue")
                || (name == "reload" && reload_available),
            unavailable_reason: (match name {
                "status" | "spend" | "pause" | "continue" => None,
                "reload" if reload_available => None,
                "reload" => Some(
                    "Reload requires an app-server process launched by the managed local daemon with an explicitly configured local Codex launcher.".to_string(),
                ),
                _ => Some("This command is currently TUI-only.".to_string()),
            }),
        },
    )
    .collect();

    if let Some(spend) = commands.iter_mut().find(|command| command.name == "spend") {
        spend.aliases.push("usage".to_string());
    }

    commands
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use tokio::time::timeout;

    use super::MAX_OUTPUT_CHARS;
    use super::ReloadLauncher;
    use super::ReloadOperation;
    use super::bounded_output;
    use super::command_specs;
    use super::reload_command_args;
    use super::reserve_reload;
    use super::schedule_reload;

    #[test]
    fn reload_uses_daemon_apply_command() {
        assert_eq!(
            reload_command_args(ReloadOperation::Apply),
            ["app-server", "daemon", "apply"]
        );
        assert_eq!(
            reload_command_args(ReloadOperation::Recover),
            ["app-server", "daemon", "recover"]
        );
    }

    #[test]
    fn reload_catalog_requires_explicit_launcher_marker() {
        let unavailable = command_specs(false)
            .into_iter()
            .find(|command| command.name == "reload")
            .expect("reload command");
        assert!(!unavailable.available);
        assert!(
            unavailable
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("managed local daemon"))
        );

        let available = command_specs(true)
            .into_iter()
            .find(|command| command.name == "reload")
            .expect("reload command");
        assert!(available.available);
        assert_eq!(available.unavailable_reason, None);
    }

    #[test]
    fn usage_is_catalogued_as_spend_alias() {
        let commands = command_specs(true);
        let spend = commands
            .iter()
            .find(|command| command.name == "spend")
            .expect("spend command");

        assert_eq!(spend.aliases, vec!["usage"]);
        assert!(commands.iter().all(|command| command.name != "usage"));
    }

    #[test]
    fn bounded_output_truncates_at_a_character_boundary() {
        let output = bounded_output("é".repeat(MAX_OUTPUT_CHARS + 1));
        assert!(output.ends_with("_Output truncated by the host._"));
        assert!(output.starts_with(&"é".repeat(MAX_OUTPUT_CHARS)));
    }

    #[tokio::test]
    async fn scheduled_reload_invokes_injected_launcher_once() {
        let reload_scheduled = Arc::new(AtomicBool::new(false));
        assert!(reserve_reload(&reload_scheduled));
        assert!(!reserve_reload(&reload_scheduled));
        let launches = Arc::new(AtomicUsize::new(0));
        let launched_path = Arc::new(std::sync::Mutex::new(None));
        let launcher: Arc<ReloadLauncher> = {
            let launches = Arc::clone(&launches);
            let launched_path = Arc::clone(&launched_path);
            Arc::new(move |executable, _operation| {
                launches.fetch_add(1, Ordering::AcqRel);
                *launched_path.lock().expect("path lock") = Some(executable);
                status_child("applied")
            })
        };
        schedule_reload(
            Arc::clone(&reload_scheduled),
            std::path::PathBuf::from("/tmp/codex-test-launcher"),
            ReloadOperation::Apply,
            launcher,
            None,
        )
        .await
        .expect("fake launcher task should complete");
        timeout(Duration::from_secs(1), async {
            while launches.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fake launcher should be invoked");
        assert_eq!(launches.load(Ordering::Acquire), 1);
        assert_eq!(
            launched_path.lock().expect("path lock").as_deref(),
            Some(std::path::Path::new("/tmp/codex-test-launcher"))
        );
        assert!(!reload_scheduled.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn failed_injected_launcher_rearms_reload() {
        let reload_scheduled = Arc::new(AtomicBool::new(true));
        let launcher: Arc<ReloadLauncher> =
            Arc::new(|_, _| Err(std::io::Error::other("fixture launch failure")));
        schedule_reload(
            Arc::clone(&reload_scheduled),
            std::path::PathBuf::from("/tmp/codex-test-launcher"),
            ReloadOperation::Apply,
            launcher,
            None,
        )
        .await
        .expect("failed launcher task should complete");
        timeout(Duration::from_secs(1), async {
            while reload_scheduled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed launcher should rearm reload");
    }

    #[tokio::test]
    async fn non_applied_daemon_status_rearms_reload() {
        let reload_scheduled = Arc::new(AtomicBool::new(true));
        let launcher: Arc<ReloadLauncher> = Arc::new(|_, _| status_child("needsAttention"));
        schedule_reload(
            Arc::clone(&reload_scheduled),
            std::path::PathBuf::from("/tmp/codex-test-launcher"),
            ReloadOperation::Apply,
            launcher,
            None,
        )
        .await
        .expect("fixture daemon task should complete");
        timeout(Duration::from_secs(1), async {
            while reload_scheduled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("non-applied daemon status should rearm reload");
    }

    #[tokio::test]
    async fn in_progress_daemon_status_rearms_reload_without_failure() {
        let reload_scheduled = Arc::new(AtomicBool::new(true));
        let launcher: Arc<ReloadLauncher> = Arc::new(|_, _| status_child("inProgress"));
        schedule_reload(
            Arc::clone(&reload_scheduled),
            std::path::PathBuf::from("/tmp/codex-test-launcher"),
            ReloadOperation::Apply,
            launcher,
            None,
        )
        .await
        .expect("fixture daemon task should complete");
        timeout(Duration::from_secs(1), async {
            while reload_scheduled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("in-progress daemon status should rearm reload");
    }

    fn status_child(status: &str) -> std::io::Result<tokio::process::Child> {
        #[cfg(unix)]
        {
            tokio::process::Command::new("sh")
                .args(["-c", &format!("printf '{{\"status\":\"{status}\"}}'")])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        }
        #[cfg(windows)]
        {
            tokio::process::Command::new("cmd")
                .args(["/C", &format!("echo {{\"status\":\"{status}\"}}")])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        }
    }
}
