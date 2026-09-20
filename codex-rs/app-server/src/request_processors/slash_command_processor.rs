use std::sync::Arc;

use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
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
use codex_app_server_protocol::SlashCommandSpec;
use codex_app_server_protocol::RequestId;

use super::AccountRequestProcessor;
use super::ConnectionRequestId;
use super::OutgoingMessageSender;
use super::invalid_request;

const MAX_OUTPUT_CHARS: usize = 20_000;

#[derive(Clone)]
pub(crate) struct SlashCommandRequestProcessor {
    account_processor: AccountRequestProcessor,
    outgoing: Arc<OutgoingMessageSender>,
}

impl SlashCommandRequestProcessor {
    pub(crate) fn new(
        account_processor: AccountRequestProcessor,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> Self {
        Self {
            account_processor,
            outgoing,
        }
    }

    pub(crate) fn list(&self, _params: SlashCommandListParams) -> SlashCommandListResponse {
        SlashCommandListResponse {
            commands: command_specs(),
            capabilities: SlashCommandCapabilities {
                status: true,
                spend: true,
                reload: true,
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
            "reload" => reload_result(),
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
                    result: result.clone(),
                },
            ))
            .await;
        Ok(Some(result.into()))
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

fn reload_result() -> SlashCommandExecuteResponse {
    let reload = SlashCommandReloadResult {
        eligible: false,
        state: "unavailable".to_string(),
        reason: Some(
            "Reload is available only through a managed local daemon with an explicit launcher; no live restart was attempted.".to_string(),
        ),
    };
    SlashCommandExecuteResponse {
        command: "reload".to_string(),
        ok: false,
        result_kind: SlashCommandResultKind::Reload,
        output: SlashCommandOutput {
            format: "markdown".to_string(),
            text: "`/reload` is unavailable for this app-server session.".to_string(),
        },
        reload: Some(reload),
    }
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

fn internal_error(error: impl std::fmt::Display) -> JSONRPCErrorError {
    crate::error_code::internal_error(error.to_string())
}

fn command_specs() -> Vec<SlashCommandSpec> {
    [
        ("status", "show current session configuration and token usage", false),
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
    .map(|(name, description, supports_inline_args)| SlashCommandSpec {
        name: name.to_string(),
        aliases: Vec::new(),
        description: description.to_string(),
        supports_inline_args,
        available: matches!(name, "status" | "spend" | "usage" | "reload"),
        unavailable_reason: (!matches!(name, "status" | "spend" | "usage" | "reload"))
            .then(|| "This command is currently TUI-only.".to_string()),
    })
    .collect()
}
