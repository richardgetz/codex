use super::*;
use codex_mcp::ElicitationReviewRequest;
use codex_mcp::ElicitationReviewer;
use codex_mcp::ElicitationReviewerHandle;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_KEY as MCP_ELICITATION_APPROVAL_KIND_KEY;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_MCP_TOOL_CALL as MCP_ELICITATION_APPROVAL_KIND_MCP_TOOL_CALL;
use codex_protocol::mcp_approval_meta::APPROVALS_REVIEWER_KEY as MCP_ELICITATION_APPROVALS_REVIEWER_KEY;
use codex_protocol::mcp_approval_meta::CONNECTOR_DESCRIPTION_KEY as MCP_ELICITATION_CONNECTOR_DESCRIPTION_KEY;
use codex_protocol::mcp_approval_meta::CONNECTOR_ID_KEY as MCP_ELICITATION_CONNECTOR_ID_KEY;
use codex_protocol::mcp_approval_meta::CONNECTOR_NAME_KEY as MCP_ELICITATION_CONNECTOR_NAME_KEY;
use codex_protocol::mcp_approval_meta::REQUEST_TYPE_APPROVAL_REQUEST as MCP_ELICITATION_REQUEST_TYPE_APPROVAL_REQUEST;
use codex_protocol::mcp_approval_meta::REQUEST_TYPE_KEY as MCP_ELICITATION_REQUEST_TYPE_KEY;
use codex_protocol::mcp_approval_meta::TOOL_DESCRIPTION_KEY as MCP_ELICITATION_TOOL_DESCRIPTION_KEY;
use codex_protocol::mcp_approval_meta::TOOL_NAME_KEY as MCP_ELICITATION_TOOL_NAME_KEY;
use codex_protocol::mcp_approval_meta::TOOL_PARAMS_KEY as MCP_ELICITATION_TOOL_PARAMS_KEY;
use codex_protocol::mcp_approval_meta::TOOL_TITLE_KEY as MCP_ELICITATION_TOOL_TITLE_KEY;
use rmcp::model::CreateElicitationRequestParams;
use rmcp::model::ElicitationAction;
use rmcp::model::Meta;
use serde_json::Map;

const MCP_ELICITATION_DECLINE_MESSAGE_KEY: &str = "message";

#[derive(Debug, PartialEq)]
enum GuardianElicitationReview {
    NotRequested,
    Decline(&'static str),
    ApprovalRequest(Box<crate::guardian::GuardianApprovalRequest>),
}

struct GuardianMcpElicitationReviewer {
    session: std::sync::Weak<Session>,
}

impl GuardianMcpElicitationReviewer {
    fn new(session: &Arc<Session>) -> Self {
        Self {
            session: Arc::downgrade(session),
        }
    }
}

impl ElicitationReviewer for GuardianMcpElicitationReviewer {
    fn review(
        &self,
        request: ElicitationReviewRequest,
    ) -> BoxFuture<'static, anyhow::Result<Option<ElicitationResponse>>> {
        let session = self.session.clone();
        Box::pin(async move {
            let Some(session) = session.upgrade() else {
                return Ok(None);
            };
            review_guardian_mcp_elicitation(session, request).await
        })
    }
}

impl Session {
    pub(crate) fn mcp_elicitation_reviewer(self: &Arc<Self>) -> ElicitationReviewerHandle {
        Arc::new(GuardianMcpElicitationReviewer::new(self))
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn request_mcp_server_elicitation(
        &self,
        turn_context: &TurnContext,
        request_id: RequestId,
        params: McpServerElicitationRequestParams,
    ) -> Option<ElicitationResponse> {
        if self
            .services
            .mcp_connection_manager
            .read()
            .await
            .elicitations_auto_deny()
        {
            return Some(ElicitationResponse {
                action: codex_rmcp_client::ElicitationAction::Accept,
                content: Some(serde_json::json!({})),
                meta: None,
            });
        }

        let server_name = params.server_name.clone();
        let request = match params.request {
            McpServerElicitationRequest::Form {
                meta,
                message,
                requested_schema,
            } => {
                let requested_schema = match serde_json::to_value(requested_schema) {
                    Ok(requested_schema) => requested_schema,
                    Err(err) => {
                        warn!(
                            "failed to serialize MCP elicitation schema for server_name: {server_name}, request_id: {request_id}: {err:#}"
                        );
                        return None;
                    }
                };
                codex_protocol::approvals::ElicitationRequest::Form {
                    meta,
                    message,
                    requested_schema,
                }
            }
            McpServerElicitationRequest::Url {
                meta,
                message,
                url,
                elicitation_id,
            } => codex_protocol::approvals::ElicitationRequest::Url {
                meta,
                message,
                url,
                elicitation_id,
            },
        };

        let (tx_response, rx_response) = oneshot::channel();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_elicitation(
                        server_name.clone(),
                        request_id.clone(),
                        tx_response,
                    )
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!(
                "Overwriting existing pending elicitation for server_name: {server_name}, request_id: {request_id}"
            );
        }
        let id = match request_id {
            rmcp::model::NumberOrString::String(value) => {
                codex_protocol::mcp::RequestId::String(value.to_string())
            }
            rmcp::model::NumberOrString::Number(value) => {
                codex_protocol::mcp::RequestId::Integer(value)
            }
        };
        let event = EventMsg::ElicitationRequest(ElicitationRequestEvent {
            turn_id: params.turn_id,
            server_name,
            id,
            request,
        });
        self.send_event(turn_context, event).await;
        rx_response.await.ok()
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and manager fallback must stay serialized"
    )]
    pub async fn resolve_elicitation(
        &self,
        server_name: String,
        id: RequestId,
        response: ElicitationResponse,
    ) -> anyhow::Result<()> {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_elicitation(&server_name, &id)
                }
                None => None,
            }
        };
        if let Some(tx_response) = entry {
            tx_response
                .send(response)
                .map_err(|e| anyhow::anyhow!("failed to send elicitation response: {e:?}"))?;
            return Ok(());
        }

        self.services
            .mcp_connection_manager
            .read()
            .await
            .resolve_elicitation(server_name, id, response)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "MCP resource calls are serialized through the session-owned manager guard"
    )]
    pub async fn list_resources(
        &self,
        server: &str,
        params: Option<PaginatedRequestParams>,
    ) -> anyhow::Result<ListResourcesResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .list_resources(server, params)
            .await
    }

    pub async fn list_resources_with_reconnect(
        &self,
        turn_context: &TurnContext,
        server: &str,
        params: Option<PaginatedRequestParams>,
    ) -> anyhow::Result<ListResourcesResult> {
        let first_error = match self.list_resources(server, params.clone()).await {
            Ok(result) => return Ok(result),
            Err(error) => error,
        };

        if !should_refresh_mcp_manager_after_resource_error(&first_error)
            || !self.effective_mcp_server_names().await.contains(server)
        {
            return Err(first_error);
        }

        self.refresh_mcp_servers_after_call_error(
            turn_context,
            server,
            "resources/list",
            &first_error,
        )
        .await;
        self.list_resources(server, params).await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "MCP resource calls are serialized through the session-owned manager guard"
    )]
    pub async fn list_resource_templates(
        &self,
        server: &str,
        params: Option<PaginatedRequestParams>,
    ) -> anyhow::Result<ListResourceTemplatesResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .list_resource_templates(server, params)
            .await
    }

    pub async fn list_resource_templates_with_reconnect(
        &self,
        turn_context: &TurnContext,
        server: &str,
        params: Option<PaginatedRequestParams>,
    ) -> anyhow::Result<ListResourceTemplatesResult> {
        let first_error = match self.list_resource_templates(server, params.clone()).await {
            Ok(result) => return Ok(result),
            Err(error) => error,
        };

        if !should_refresh_mcp_manager_after_resource_error(&first_error)
            || !self.effective_mcp_server_names().await.contains(server)
        {
            return Err(first_error);
        }

        self.refresh_mcp_servers_after_call_error(
            turn_context,
            server,
            "resources/templates/list",
            &first_error,
        )
        .await;
        self.list_resource_templates(server, params).await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "MCP resource calls are serialized through the session-owned manager guard"
    )]
    pub async fn read_resource(
        &self,
        server: &str,
        params: ReadResourceRequestParams,
    ) -> anyhow::Result<ReadResourceResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .read_resource(server, params)
            .await
    }

    pub async fn read_resource_with_reconnect(
        &self,
        turn_context: &TurnContext,
        server: &str,
        params: ReadResourceRequestParams,
    ) -> anyhow::Result<ReadResourceResult> {
        let first_error = match self.read_resource(server, params.clone()).await {
            Ok(result) => return Ok(result),
            Err(error) => error,
        };

        if !should_refresh_mcp_manager_after_resource_error(&first_error)
            || !self.effective_mcp_server_names().await.contains(server)
        {
            return Err(first_error);
        }

        self.refresh_mcp_servers_after_call_error(
            turn_context,
            server,
            "resources/read",
            &first_error,
        )
        .await;
        self.read_resource(server, params).await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "MCP tool calls are serialized through the session-owned manager guard"
    )]
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Value>,
        meta: Option<serde_json::Value>,
    ) -> anyhow::Result<CallToolResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .call_tool(server, tool, arguments, meta)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "lazy MCP startup/tool listing is serialized through the session-owned manager guard"
    )]
    pub async fn list_tools_for_server_with_reconnect(
        &self,
        turn_context: &TurnContext,
        server: &str,
    ) -> anyhow::Result<Vec<ToolInfo>> {
        let first_error = {
            let manager = self.services.mcp_connection_manager.read().await;
            match manager.list_tools_for_server(server).await {
                Ok(tools) => return Ok(tools),
                Err(error) => error,
            }
        };

        if !should_retry_mcp_call_after_refresh(&first_error)
            || !self.effective_mcp_server_names().await.contains(server)
        {
            return Err(first_error);
        }

        self.refresh_mcp_servers_after_call_error(turn_context, server, "tools/list", &first_error)
            .await;

        let manager = self.services.mcp_connection_manager.read().await;
        manager.list_tools_for_server(server).await
    }

    pub async fn call_tool_with_reconnect(
        &self,
        turn_context: &TurnContext,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Value>,
        meta: Option<serde_json::Value>,
    ) -> anyhow::Result<CallToolResult> {
        let first_error = match self
            .call_tool(server, tool, arguments.clone(), meta.clone())
            .await
        {
            Ok(result) => return Ok(result),
            Err(error) => error,
        };

        let should_retry = should_retry_mcp_call_after_refresh(&first_error);
        if !(should_retry || should_refresh_mcp_manager_after_live_error(&first_error))
            || !self.effective_mcp_server_names().await.contains(server)
        {
            return Err(first_error);
        }

        self.refresh_mcp_servers_after_call_error(turn_context, server, tool, &first_error)
            .await;

        if should_retry {
            self.call_tool(server, tool, arguments, meta).await
        } else {
            Err(first_error)
        }
    }

    async fn refresh_mcp_servers_after_call_error(
        &self,
        turn_context: &TurnContext,
        server: &str,
        operation: &str,
        first_error: &anyhow::Error,
    ) {
        warn!(
            "refreshing MCP servers after call failed for server '{server}', operation '{operation}': {first_error:#}"
        );
        let mcp_servers = self.configured_mcp_servers().await;
        let config = self.get_config().await;
        self.refresh_mcp_servers_now(
            turn_context,
            mcp_servers,
            config.mcp_oauth_credentials_store_mode,
            /*elicitation_reviewer*/ None,
        )
        .await;
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "MCP tool metadata reads through the session-owned manager guard"
    )]
    pub(crate) async fn resolve_mcp_tool_info(&self, tool_name: &ToolName) -> Option<ToolInfo> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .resolve_tool_info(tool_name)
            .await
    }

    pub(crate) async fn resolve_configured_mcp_tool_call(
        &self,
        turn_context: &TurnContext,
        tool_name: &ToolName,
    ) -> Option<(ToolName, String, String)> {
        let (server, tool) = parse_non_app_mcp_tool_name(tool_name)?;
        let callable_namespace = format!("mcp__{server}__");
        if !crate::enablement::mcp_tool_parts_allowed_in_mode(
            &turn_context.config,
            turn_context.collaboration_mode.mode,
            &server,
            &callable_namespace,
            &tool,
        ) {
            return None;
        }

        if !self
            .services
            .mcp_connection_manager
            .read()
            .await
            .has_server(&server)
        {
            return None;
        }

        Some((
            ToolName::namespaced(callable_namespace, tool.clone()),
            server,
            tool,
        ))
    }

    async fn configured_mcp_servers(&self) -> HashMap<String, McpServerConfig> {
        let config = self.get_config().await;
        let mcp_config = config
            .to_mcp_config(self.services.plugins_manager.as_ref())
            .await;
        mcp_config.configured_mcp_servers
    }

    async fn effective_mcp_server_names(&self) -> HashSet<String> {
        let auth = self.services.auth_manager.auth().await;
        let config = self.get_config().await;
        let mcp_config = config
            .to_mcp_config(self.services.plugins_manager.as_ref())
            .await;
        effective_mcp_servers_from_configured(
            mcp_config.configured_mcp_servers.clone(),
            &mcp_config,
            auth.as_ref(),
        )
        .into_keys()
        .collect()
    }

    async fn refresh_mcp_servers_inner(
        &self,
        turn_context: &TurnContext,
        mcp_servers: HashMap<String, McpServerConfig>,
        store_mode: OAuthCredentialsStoreMode,
        elicitation_reviewer: Option<ElicitationReviewerHandle>,
    ) {
        let auth = self.services.auth_manager.auth().await;
        let config = self.get_config().await;
        let mcp_config = config
            .to_mcp_config(self.services.plugins_manager.as_ref())
            .await;
        let tool_plugin_provenance = self
            .services
            .mcp_manager
            .tool_plugin_provenance(config.as_ref())
            .await;
        let mcp_servers =
            effective_mcp_servers_from_configured(mcp_servers, &mcp_config, auth.as_ref());
        let host_owned_codex_apps_enabled =
            host_owned_codex_apps_enabled(&mcp_config, auth.as_ref());
        let auth_statuses =
            compute_auth_statuses(mcp_servers.iter(), store_mode, auth.as_ref()).await;
        let mcp_runtime_environment = match turn_context.environments.primary() {
            Some(turn_environment) => McpRuntimeEnvironment::new(
                Arc::clone(&turn_environment.environment),
                turn_environment.cwd.to_path_buf(),
            ),
            None => McpRuntimeEnvironment::new(
                self.services
                    .environment_manager
                    .default_environment()
                    .unwrap_or_else(|| self.services.environment_manager.local_environment()),
                turn_context.cwd.to_path_buf(),
            ),
        };
        {
            let mut guard = self.services.mcp_startup_cancellation_token.lock().await;
            guard.cancel();
            *guard = CancellationToken::new();
        }
        let (refreshed_manager, cancel_token) = McpConnectionManager::new(
            &mcp_servers,
            store_mode,
            auth_statuses,
            &turn_context.approval_policy,
            turn_context.sub_id.clone(),
            self.get_tx_event(),
            turn_context.permission_profile(),
            mcp_runtime_environment,
            config.codex_home.to_path_buf(),
            codex_apps_tools_cache_key(auth.as_ref()),
            host_owned_codex_apps_enabled,
            tool_plugin_provenance,
            auth.as_ref(),
            elicitation_reviewer,
        )
        .await;
        {
            let current_manager = self.services.mcp_connection_manager.read().await;
            refreshed_manager.set_elicitations_auto_deny(current_manager.elicitations_auto_deny());
        }
        {
            let mut guard = self.services.mcp_startup_cancellation_token.lock().await;
            if guard.is_cancelled() {
                cancel_token.cancel();
            }
            *guard = cancel_token;
        }

        let mut old_manager = {
            let mut manager = self.services.mcp_connection_manager.write().await;
            std::mem::replace(&mut *manager, refreshed_manager)
        };
        old_manager.shutdown().await;
    }

    pub(crate) async fn refresh_mcp_servers_if_requested(
        &self,
        turn_context: &TurnContext,
        elicitation_reviewer: Option<ElicitationReviewerHandle>,
    ) {
        let refresh_config = { self.pending_mcp_server_refresh_config.lock().await.take() };
        let Some(refresh_config) = refresh_config else {
            return;
        };

        let McpServerRefreshConfig {
            mcp_servers,
            mcp_oauth_credentials_store_mode,
        } = refresh_config;

        let mcp_servers =
            match serde_json::from_value::<HashMap<String, McpServerConfig>>(mcp_servers) {
                Ok(servers) => servers,
                Err(err) => {
                    warn!("failed to parse MCP server refresh config: {err}");
                    return;
                }
            };
        let store_mode = match serde_json::from_value::<OAuthCredentialsStoreMode>(
            mcp_oauth_credentials_store_mode,
        ) {
            Ok(mode) => mode,
            Err(err) => {
                warn!("failed to parse MCP OAuth refresh config: {err}");
                return;
            }
        };

        self.refresh_mcp_servers_inner(turn_context, mcp_servers, store_mode, elicitation_reviewer)
            .await;
    }

    pub(crate) async fn refresh_mcp_servers_now(
        &self,
        turn_context: &TurnContext,
        mcp_servers: HashMap<String, McpServerConfig>,
        store_mode: OAuthCredentialsStoreMode,
        elicitation_reviewer: Option<ElicitationReviewerHandle>,
    ) {
        self.refresh_mcp_servers_inner(turn_context, mcp_servers, store_mode, elicitation_reviewer)
            .await;
    }

    #[cfg(test)]
    pub(crate) async fn mcp_startup_cancellation_token(&self) -> CancellationToken {
        self.services
            .mcp_startup_cancellation_token
            .lock()
            .await
            .clone()
    }

    pub(crate) async fn cancel_mcp_startup(&self) {
        self.services
            .mcp_startup_cancellation_token
            .lock()
            .await
            .cancel();
    }
}

fn should_retry_mcp_call_after_refresh(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("failed to get client") || message.contains("unknown MCP server")
}

fn should_refresh_mcp_manager_after_live_error(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("tool call failed for `")
}

fn should_refresh_mcp_manager_after_resource_error(error: &anyhow::Error) -> bool {
    should_retry_mcp_call_after_refresh(error) || format!("{error:#}").contains(" failed for `")
}

fn parse_non_app_mcp_tool_name(tool_name: &ToolName) -> Option<(String, String)> {
    let (server, tool) = match tool_name.namespace.as_deref() {
        Some(namespace) => {
            let server = namespace
                .strip_prefix("mcp__")?
                .strip_suffix("__")?
                .to_string();
            (server, tool_name.name.clone())
        }
        None => {
            let raw = tool_name.name.strip_prefix("mcp__")?;
            let (server, tool) = raw.split_once("__")?;
            (server.to_string(), tool.to_string())
        }
    };

    if server.is_empty() || tool.is_empty() || server == codex_mcp::CODEX_APPS_MCP_SERVER_NAME {
        return None;
    }

    Some((server, tool))
}

async fn review_guardian_mcp_elicitation(
    session: Arc<Session>,
    request: ElicitationReviewRequest,
) -> anyhow::Result<Option<ElicitationResponse>> {
    let Some((turn_context, _cancellation_token)) =
        session.active_turn_context_and_cancellation_token().await
    else {
        return Ok(None);
    };

    if !crate::guardian::routes_approval_to_guardian(turn_context.as_ref()) {
        return Ok(None);
    }

    let guardian_request = match guardian_elicitation_review_request(&request) {
        GuardianElicitationReview::NotRequested => return Ok(None),
        GuardianElicitationReview::Decline(reason) => {
            warn!(
                server_name = %request.server_name,
                request_id = %mcp_elicitation_request_id(&request.request_id),
                reason,
                "declining Guardian MCP elicitation before review"
            );
            return Ok(Some(mcp_elicitation_decline_without_message()));
        }
        GuardianElicitationReview::ApprovalRequest(guardian_request) => *guardian_request,
    };

    let review_id = crate::guardian::new_guardian_review_id();
    let decision = crate::guardian::review_approval_request(
        &session,
        &turn_context,
        review_id.clone(),
        guardian_request,
        /*retry_reason*/ None,
    )
    .await;
    Ok(Some(
        mcp_elicitation_response_from_guardian_decision(session.as_ref(), &review_id, decision)
            .await,
    ))
}

fn guardian_elicitation_review_request(
    request: &ElicitationReviewRequest,
) -> GuardianElicitationReview {
    let (meta, requested_schema) = match &request.elicitation {
        CreateElicitationRequestParams::FormElicitationParams {
            meta,
            requested_schema,
            ..
        } => (meta, Some(requested_schema)),
        CreateElicitationRequestParams::UrlElicitationParams { meta, .. } => {
            return if meta_requests_approval_request(meta) {
                GuardianElicitationReview::Decline(
                    "guardian MCP elicitation review only supports form elicitations",
                )
            } else {
                GuardianElicitationReview::NotRequested
            };
        }
    };

    let Some(meta) = meta.as_ref().map(|meta| &meta.0) else {
        return GuardianElicitationReview::NotRequested;
    };
    if metadata_str(meta, MCP_ELICITATION_REQUEST_TYPE_KEY)
        != Some(MCP_ELICITATION_REQUEST_TYPE_APPROVAL_REQUEST)
    {
        return GuardianElicitationReview::NotRequested;
    }
    if metadata_str(meta, MCP_ELICITATION_APPROVAL_KIND_KEY)
        != Some(MCP_ELICITATION_APPROVAL_KIND_MCP_TOOL_CALL)
    {
        return GuardianElicitationReview::Decline(
            "guardian MCP elicitation metadata must declare mcp_tool_call approval kind",
        );
    }
    if requested_schema.is_some_and(|schema| !schema.properties.is_empty()) {
        return GuardianElicitationReview::Decline(
            "guardian MCP elicitation review only supports empty form schemas",
        );
    }

    let Some(tool_name) = metadata_owned_string(meta, MCP_ELICITATION_TOOL_NAME_KEY) else {
        return GuardianElicitationReview::Decline(
            "guardian MCP elicitation metadata must include a non-empty tool_name",
        );
    };
    let arguments = match meta.get(MCP_ELICITATION_TOOL_PARAMS_KEY) {
        Some(value @ Value::Object(_)) => Some(value.clone()),
        Some(_) => {
            return GuardianElicitationReview::Decline(
                "guardian MCP elicitation tool_params must be an object",
            );
        }
        None => Some(Value::Object(Map::new())),
    };

    GuardianElicitationReview::ApprovalRequest(Box::new(
        crate::guardian::GuardianApprovalRequest::McpToolCall {
            id: format!(
                "mcp_elicitation:{}:{}",
                request.server_name,
                mcp_elicitation_request_id(&request.request_id)
            ),
            server: request.server_name.clone(),
            tool_name,
            arguments,
            connector_id: metadata_owned_string(meta, MCP_ELICITATION_CONNECTOR_ID_KEY),
            connector_name: metadata_owned_string(meta, MCP_ELICITATION_CONNECTOR_NAME_KEY),
            connector_description: metadata_owned_string(
                meta,
                MCP_ELICITATION_CONNECTOR_DESCRIPTION_KEY,
            ),
            tool_title: metadata_owned_string(meta, MCP_ELICITATION_TOOL_TITLE_KEY),
            tool_description: metadata_owned_string(meta, MCP_ELICITATION_TOOL_DESCRIPTION_KEY),
            annotations: None,
        },
    ))
}

fn meta_requests_approval_request(meta: &Option<Meta>) -> bool {
    meta.as_ref()
        .and_then(|meta| metadata_str(&meta.0, MCP_ELICITATION_REQUEST_TYPE_KEY))
        == Some(MCP_ELICITATION_REQUEST_TYPE_APPROVAL_REQUEST)
}

fn metadata_str<'a>(meta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key).and_then(Value::as_str)
}

fn metadata_owned_string(meta: &Map<String, Value>, key: &str) -> Option<String> {
    metadata_str(meta, key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn mcp_elicitation_request_id(id: &RequestId) -> String {
    match id {
        rmcp::model::NumberOrString::String(value) => value.to_string(),
        rmcp::model::NumberOrString::Number(value) => value.to_string(),
    }
}

async fn mcp_elicitation_response_from_guardian_decision(
    session: &Session,
    review_id: &str,
    decision: ReviewDecision,
) -> ElicitationResponse {
    let denial_message = match decision {
        ReviewDecision::Denied => {
            Some(crate::guardian::guardian_rejection_message(session, review_id).await)
        }
        _ => None,
    };
    mcp_elicitation_response_from_guardian_decision_parts(decision, denial_message)
}

fn mcp_elicitation_response_from_guardian_decision_parts(
    decision: ReviewDecision,
    denial_message: Option<String>,
) -> ElicitationResponse {
    match decision {
        ReviewDecision::Approved
        | ReviewDecision::ApprovedForSession
        | ReviewDecision::ApprovedExecpolicyAmendment { .. }
        | ReviewDecision::NetworkPolicyAmendment { .. } => ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({})),
            meta: Some(mcp_elicitation_auto_meta()),
        },
        ReviewDecision::Denied => mcp_elicitation_decline_with_message(
            denial_message.unwrap_or_else(|| "Guardian denied this request.".to_string()),
        ),
        ReviewDecision::TimedOut => {
            mcp_elicitation_decline_with_message(crate::guardian::guardian_timeout_message())
        }
        ReviewDecision::Abort => ElicitationResponse {
            action: ElicitationAction::Cancel,
            content: None,
            meta: Some(mcp_elicitation_auto_meta()),
        },
    }
}

fn mcp_elicitation_decline_with_message(message: String) -> ElicitationResponse {
    ElicitationResponse {
        action: ElicitationAction::Decline,
        content: None,
        meta: Some(serde_json::json!({
            MCP_ELICITATION_DECLINE_MESSAGE_KEY: message,
            MCP_ELICITATION_APPROVALS_REVIEWER_KEY: ApprovalsReviewer::AutoReview,
        })),
    }
}

fn mcp_elicitation_decline_without_message() -> ElicitationResponse {
    ElicitationResponse {
        action: ElicitationAction::Decline,
        content: None,
        meta: Some(mcp_elicitation_auto_meta()),
    }
}

fn mcp_elicitation_auto_meta() -> serde_json::Value {
    serde_json::json!({
        MCP_ELICITATION_APPROVALS_REVIEWER_KEY: ApprovalsReviewer::AutoReview,
    })
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
