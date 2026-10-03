//! Narrow history tools backed by the parent's current MCP connection and authorization.
//! No executable binding or policy snapshot survives between calls. App approval gates fail closed.

use std::sync::Arc;
use std::sync::Weak;
use std::time::Instant;

use anyhow::Context;
use codex_features::Feature;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpBinding;
use codex_tools::FunctionCallError;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolExecutorFuture;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolSpec;
use codex_tools::mcp_tool_to_responses_api_tool;
use codex_utils_output_truncation::TruncationPolicy;
use serde_json::json;

use crate::CodexThread;
use crate::session::session::Session;
use crate::tools::context::McpToolOutput;

use super::app_tool_policy;
use super::mcp_tool_metadata;
use super::requires_mcp_tool_approval_for_mode;
use super::with_mcp_tool_call_ids_meta;

const MAX_HISTORY_OUTPUT_TOKENS: usize = 8_000;
const MAX_HISTORY_OUTPUT_BYTES: usize = 8_192;

/// Exposes only history search/read. The caller owns exposure and the maximum output budget;
/// each invocation resolves the parent's live client, account, and app/tool policy again.
pub async fn conversation_history_tools(
    parent: &Arc<CodexThread>,
    max_output_tokens: usize,
) -> anyhow::Result<Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>>> {
    let max_output_tokens = max_output_tokens.min(MAX_HISTORY_OUTPUT_TOKENS);
    let binding = history_binding(&parent.session).await?;
    let mut tools = Vec::new();
    for name in ["search_messages", "read_messages"] {
        let raw_name = format!("user_message.{name}");
        let Some(prepared) = binding.prepare_call(CODEX_APPS_MCP_SERVER_NAME, &raw_name) else {
            continue;
        };
        if !prepared.is_host_owned_apps() {
            continue;
        }
        let name = ToolName::namespaced("user_message", name);
        let function = mcp_tool_to_responses_api_tool(
            &name,
            &prepared.tool_info().tool,
            /*schema_max_bytes*/ None,
        )?;
        tools.push(Arc::new(HistoryTool {
            parent: Arc::downgrade(&parent.session),
            function,
            max_output_tokens,
        })
            as Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>);
    }
    Ok(tools)
}

async fn history_binding(parent: &Arc<Session>) -> anyhow::Result<Arc<McpBinding>> {
    let config = parent.get_config().await;
    anyhow::ensure!(
        config.features.enabled(Feature::Apps)
            && config
                .features
                .enabled(Feature::GuardianConversationHistoryTools),
        "Conversation history is disabled on the parent"
    );
    parent.refresh_mcp_if_dirty().await;
    parent
        .services
        .mcp_runtime
        .current_binding_for_call(CODEX_APPS_MCP_SERVER_NAME)
        .await
        .context("Parent conversation history is unavailable")
}

struct HistoryTool {
    parent: Weak<Session>,
    function: ResponsesApiTool,
    max_output_tokens: usize,
}

impl<'call> ToolExecutor<ToolCall<'call>> for HistoryTool {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("user_message", &self.function.name)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "user_message".to_owned(),
            description: "Search and read the owner's conversation history.".to_owned(),
            tools: vec![ResponsesApiNamespaceTool::Function(self.function.clone())],
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let result: anyhow::Result<Box<dyn ToolOutput>> = async {
                let parent = self.parent.upgrade().context("Parent thread has stopped")?;
                let binding = history_binding(&parent).await?;
                let raw_name = format!("user_message.{}", self.function.name);
                let prepared = binding
                    .prepare_call(CODEX_APPS_MCP_SERVER_NAME, &raw_name)
                    .context("History tool is disabled or unavailable on the parent")?;
                anyhow::ensure!(
                    prepared.is_host_owned_apps(),
                    "History requires the parent's hosted Apps connection"
                );
                let arguments = serde_json::from_str(call.function_arguments()?)?;
                let metadata = mcp_tool_metadata(
                    prepared.tool_info(),
                    prepared.plugin_id(),
                    Some(&arguments),
                )?;
                let policy = app_tool_policy(prepared.config(), &metadata, &raw_name);
                anyhow::ensure!(policy.enabled, "MCP tool call blocked by app configuration");
                anyhow::ensure!(
                    !requires_mcp_tool_approval_for_mode(
                        metadata.annotations.as_ref(),
                        policy.approval
                    ),
                    "History tool requires approval on the parent; unavailable during review"
                );
                let mut apps_meta = metadata.codex_apps_meta.unwrap_or_default();
                apps_meta.insert("call_id".to_owned(), json!(call.call_id));
                let meta = with_mcp_tool_call_ids_meta(
                    Some(json!({"callId": call.call_id, "_codex_apps": apps_meta})),
                    &parent.thread_id.to_string(),
                    &parent.session_id().to_string(),
                    /*originating_call*/ None,
                );
                let limit = prepared
                    .output_token_limit()
                    .unwrap_or(self.max_output_tokens)
                    .min(self.max_output_tokens);
                let call_byte_limit = match call.truncation_policy {
                    TruncationPolicy::Bytes(bytes) => bytes,
                    TruncationPolicy::Tokens(tokens) => tokens,
                };
                let output_byte_limit = limit.min(call_byte_limit).min(MAX_HISTORY_OUTPUT_BYTES);
                let started = Instant::now();
                let mut result = prepared
                    .call(Some(arguments.clone()), meta, /*timeout*/ None)
                    .await?;
                let mut text_segments = Vec::new();
                let mut omitted_content = false;
                let mut has_encrypted_content = false;
                for content in &result.content {
                    let encrypted = content
                        .get("_meta")
                        .and_then(|meta| meta.get("codex/encryptedContent"))
                        .and_then(serde_json::Value::as_bool)
                        == Some(true);
                    has_encrypted_content |= encrypted;
                    if content.get("type").and_then(serde_json::Value::as_str) == Some("text")
                        && !encrypted
                    {
                        if let Some(text) = content.get("text").and_then(serde_json::Value::as_str)
                        {
                            text_segments.push(text.to_owned());
                        } else {
                            omitted_content = true;
                        }
                    } else {
                        omitted_content = true;
                    }
                }
                // Structured content may duplicate an encrypted block. Match the shared MCP
                // output conversion: once an encrypted block is present, suppress the whole
                // structured payload rather than exposing a duplicate through this text path.
                if !has_encrypted_content
                    && let Some(structured_content) = &result.structured_content
                {
                    text_segments.push(serde_json::to_string(structured_content)?);
                }
                if omitted_content {
                    text_segments.insert(
                        0,
                        "[Some non-text, encrypted, or unknown hosted history content was omitted.]"
                            .to_owned(),
                    );
                }
                let history_text = text_segments.join("\n");
                result.content = vec![json!({"type": "text", "text": history_text})];
                result.structured_content = None;
                result.meta = None;
                Ok(Box::new(McpToolOutput {
                    result,
                    tool_input: arguments,
                    result_metadata_capture_allowed: false,
                    wall_time: started.elapsed(),
                    original_image_detail_supported: false,
                    truncation_policy: TruncationPolicy::Bytes(output_byte_limit),
                    serialized_output_max_bytes: Some(MAX_HISTORY_OUTPUT_BYTES),
                }) as Box<dyn ToolOutput>)
            }
            .await;
            result.map_err(|error| FunctionCallError::RespondToModel(error.to_string()))
        })
    }
}
