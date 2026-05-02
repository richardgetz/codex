use super::*;

impl Session {
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
            || !self.effective_mcp_servers().await.contains_key(server)
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
            || !self.effective_mcp_servers().await.contains_key(server)
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
            || !self.effective_mcp_servers().await.contains_key(server)
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
            || !self.effective_mcp_servers().await.contains_key(server)
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
            || !self.effective_mcp_servers().await.contains_key(server)
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
        let mcp_servers = self.effective_mcp_servers().await;
        let config = self.get_config().await;
        self.refresh_mcp_servers_now(
            turn_context,
            mcp_servers,
            config.mcp_oauth_credentials_store_mode,
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

    async fn effective_mcp_servers(&self) -> HashMap<String, McpServerConfig> {
        let auth = self.services.auth_manager.auth().await;
        let config = self.get_config().await;
        let mcp_config = config
            .to_mcp_config(self.services.plugins_manager.as_ref())
            .await;
        with_codex_apps_mcp(
            mcp_config.configured_mcp_servers.clone(),
            auth.as_ref(),
            &mcp_config,
        )
    }

    async fn refresh_mcp_servers_inner(
        &self,
        turn_context: &TurnContext,
        mcp_servers: HashMap<String, McpServerConfig>,
        store_mode: OAuthCredentialsStoreMode,
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
        let mcp_servers = with_codex_apps_mcp(mcp_servers, auth.as_ref(), &mcp_config);
        let auth_statuses =
            compute_auth_statuses(mcp_servers.iter(), store_mode, auth.as_ref()).await;
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
            McpRuntimeEnvironment::new(
                turn_context
                    .environment
                    .clone()
                    .unwrap_or_else(|| self.services.environment_manager.local_environment()),
                turn_context.cwd.to_path_buf(),
            ),
            config.codex_home.to_path_buf(),
            codex_apps_tools_cache_key(auth.as_ref()),
            tool_plugin_provenance,
            auth.as_ref(),
        )
        .await;
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

    pub(crate) async fn refresh_mcp_servers_if_requested(&self, turn_context: &TurnContext) {
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

        self.refresh_mcp_servers_inner(turn_context, mcp_servers, store_mode)
            .await;
    }

    pub(crate) async fn refresh_mcp_servers_now(
        &self,
        turn_context: &TurnContext,
        mcp_servers: HashMap<String, McpServerConfig>,
        store_mode: OAuthCredentialsStoreMode,
    ) {
        self.refresh_mcp_servers_inner(turn_context, mcp_servers, store_mode)
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn refreshes_after_stale_mcp_client_errors() {
        assert!(should_retry_mcp_call_after_refresh(&anyhow!(
            "failed to get client: MCP startup failed"
        )));
        assert!(!should_retry_mcp_call_after_refresh(&anyhow!(
            "tool call failed for `imessage/imessage_get_config`: connection closed"
        )));
        assert!(should_refresh_mcp_manager_after_live_error(&anyhow!(
            "tool call failed for `imessage/imessage_get_config`: connection closed"
        )));
        assert!(should_refresh_mcp_manager_after_resource_error(&anyhow!(
            "resources/list failed for `imessage`: connection closed"
        )));
        assert!(!should_retry_mcp_call_after_refresh(&anyhow!(
            "tool 'send' is disabled for MCP server 'imessage'"
        )));
    }

    #[test]
    fn parses_non_app_mcp_placeholder_names() {
        assert_eq!(
            parse_non_app_mcp_tool_name(&ToolName::plain("mcp__imessage__imessage_get_config")),
            Some(("imessage".to_string(), "imessage_get_config".to_string()))
        );
        assert_eq!(
            parse_non_app_mcp_tool_name(&ToolName::namespaced(
                "mcp__imessage__",
                "imessage_get_config"
            )),
            Some(("imessage".to_string(), "imessage_get_config".to_string()))
        );
        assert_eq!(
            parse_non_app_mcp_tool_name(&ToolName::plain("mcp__codex_apps__calendar_create_event")),
            None
        );
    }
}
