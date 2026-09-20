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
use codex_app_server_transport::APP_SERVER_DAEMON_RELOAD_ENV;

use super::AccountRequestProcessor;
use super::ConnectionRequestId;
use super::OutgoingMessageSender;
use super::invalid_request;

const MAX_OUTPUT_CHARS: usize = 20_000;
const RELOAD_HANDOFF_DELAY: Duration = Duration::from_millis(250);

type ReloadLauncher = dyn Fn(std::path::PathBuf) -> std::io::Result<()> + Send + Sync + 'static;

#[derive(Clone)]
pub(crate) struct SlashCommandRequestProcessor {
    account_processor: AccountRequestProcessor,
    outgoing: Arc<OutgoingMessageSender>,
    reload_scheduled: Arc<AtomicBool>,
}

impl SlashCommandRequestProcessor {
    pub(crate) fn new(
        account_processor: AccountRequestProcessor,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> Self {
        Self {
            account_processor,
            outgoing,
            reload_scheduled: Arc::new(AtomicBool::new(false)),
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
        let result = match command.as_str() {
            "status" => self.status_result().await?,
            "spend" | "usage" => self.spend_result().await?,
            "reload" => self.reload_result().await,
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

        self.outgoing
            .send_server_notification(ServerNotification::SlashCommandResult(
                SlashCommandResultNotification {
                    thread_id: params.thread_id,
                    command,
                    request_id: request_id.request_id.to_string(),
                    result: result_payload(&result),
                },
            ))
            .await;
        Ok(Some(result.into()))
    }

    async fn reload_result(&self) -> SlashCommandExecuteResponse {
        if !daemon_reload_available() {
            return unavailable_reload_result(
                "Reload requires an app-server process launched by the managed local daemon with an explicitly configured local Codex launcher.",
            );
        }
        if !reserve_reload(&self.reload_scheduled) {
            return unavailable_reload_result(
                "A Codex reload is already scheduled for this app-server session.",
            );
        }
        let executable = match std::env::current_exe() {
            Ok(executable) if executable.is_file() => executable,
            Ok(executable) => {
                self.reload_scheduled.store(false, Ordering::Release);
                return unavailable_reload_result(&format!(
                    "The current Codex launcher is not a file: {}.",
                    executable.display()
                ));
            }
            Err(error) => {
                self.reload_scheduled.store(false, Ordering::Release);
                return unavailable_reload_result(&format!(
                    "The current Codex launcher could not be resolved: {error}."
                ));
            }
        };

        schedule_reload(
            Arc::clone(&self.reload_scheduled),
            executable,
            Arc::new(spawn_reload_daemon),
        );

        SlashCommandExecuteResponse {
            command: "reload".to_string(),
            ok: true,
            result_kind: SlashCommandResultKind::Reload,
            output: SlashCommandOutput {
                format: "markdown".to_string(),
                text: "`/reload` scheduled a managed daemon handoff. The app-server will pause, replace the running Codex, and recover the exact paused turns.".to_string(),
            },
            reload: Some(SlashCommandReloadResult {
                eligible: true,
                state: "scheduled".to_string(),
                reason: Some("The daemon owns pause, replacement, exact-turn recovery, and unresolved-failure receipts.".to_string()),
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

    async fn spend_result(&self) -> Result<SlashCommandExecuteResponse, JSONRPCErrorError> {
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
            command: "spend".to_string(),
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

fn daemon_reload_available() -> bool {
    std::env::var_os(APP_SERVER_DAEMON_RELOAD_ENV).is_some_and(|value| value == "1")
}

fn spawn_reload_daemon(executable: std::path::PathBuf) -> std::io::Result<()> {
    std::process::Command::new(executable)
        .args(reload_command_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

fn schedule_reload(
    reload_scheduled: Arc<AtomicBool>,
    executable: std::path::PathBuf,
    launcher: Arc<ReloadLauncher>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(RELOAD_HANDOFF_DELAY).await;
        if let Err(error) = launcher(executable) {
            reload_scheduled.store(false, Ordering::Release);
            tracing::error!(%error, "failed to schedule managed Codex reload");
        }
    });
}

fn reserve_reload(reload_scheduled: &AtomicBool) -> bool {
    reload_scheduled
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

fn reload_command_args() -> [&'static str; 3] {
    ["app-server", "daemon", "apply"]
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
        text.truncate(MAX_OUTPUT_CHARS);
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

fn internal_error(error: impl std::fmt::Display) -> JSONRPCErrorError {
    crate::error_code::internal_error(error.to_string())
}

fn command_specs(reload_available: bool) -> Vec<SlashCommandSpec> {
    [
        (
            "status",
            "show current session configuration and token usage",
            false,
        ),
        ("spend", "show daily token usage and trends", true),
        ("usage", "show token usage", true),
        ("reload", "reload the latest installed Codex safely", false),
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
            available: matches!(name, "status" | "spend" | "usage")
                || (name == "reload" && reload_available),
            unavailable_reason: (match name {
                "status" | "spend" | "usage" => None,
                "reload" if reload_available => None,
                "reload" => Some(
                    "Reload requires an app-server process launched by the managed local daemon with an explicitly configured local Codex launcher.".to_string(),
                ),
                _ => Some("This command is currently TUI-only.".to_string()),
            }),
        },
    )
    .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use tokio::time::timeout;

    use super::ReloadLauncher;
    use super::command_specs;
    use super::reload_command_args;
    use super::reserve_reload;
    use super::schedule_reload;

    #[test]
    fn reload_uses_daemon_apply_command() {
        assert_eq!(reload_command_args(), ["app-server", "daemon", "apply"]);
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
            Arc::new(move |executable| {
                launches.fetch_add(1, Ordering::AcqRel);
                *launched_path.lock().expect("path lock") = Some(executable);
                Ok(())
            })
        };
        schedule_reload(
            Arc::clone(&reload_scheduled),
            std::path::PathBuf::from("/tmp/codex-test-launcher"),
            launcher,
        );
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
        assert!(reload_scheduled.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn failed_injected_launcher_rearms_reload() {
        let reload_scheduled = Arc::new(AtomicBool::new(true));
        let launcher: Arc<ReloadLauncher> =
            Arc::new(|_| Err(std::io::Error::other("fixture launch failure")));
        schedule_reload(
            Arc::clone(&reload_scheduled),
            std::path::PathBuf::from("/tmp/codex-test-launcher"),
            launcher,
        );
        timeout(Duration::from_secs(1), async {
            while reload_scheduled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed launcher should rearm reload");
    }
}
