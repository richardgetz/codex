use super::*;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_protocol::ResponseItemId;
use codex_protocol::items::AgentMessageContent;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tracing_subscriber::prelude::*;

struct RewriteAgentMessageContributor;

impl TurnItemContributor for RewriteAgentMessageContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.content = vec![AgentMessageContent::Text {
                    text: "plan contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "1")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn message_with_text_chunks(role: &str, chunks: &[&str]) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: chunks
            .iter()
            .map(|text| ContentItem::InputText {
                text: (*text).to_string(),
            })
            .collect(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn test_mcp_tool(server_name: &str) -> codex_mcp::ToolInfo {
    codex_mcp::ToolInfo {
        server_name: server_name.to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: "lookup".to_string(),
        callable_namespace: format!("mcp__{server_name}"),
        namespace_description: None,
        tool: rmcp::model::Tool::new(
            "lookup".to_string(),
            "Test MCP tool".to_string(),
            Arc::new(rmcp::model::JsonObject::default()),
        ),
        connector_id: None,
        connector_name: None,
        plugin_display_names: Vec::new(),
        openai_file_input_optional_fields: Default::default(),
    }
}

fn test_connector(id: &str, name: &str) -> connectors::AppInfo {
    connectors::AppInfo {
        id: id.to_string(),
        name: name.to_string(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        icon_assets: None,
        icon_dark_assets: None,
        distribution_channel: None,
        branding: None,
        app_metadata: None,
        labels: None,
        install_url: None,
        is_accessible: true,
        is_enabled: true,
        plugin_display_names: Vec::new(),
    }
}

#[test]
fn per_input_mcp_selection_uses_user_text_and_server_name_boundaries() {
    let mcp_tools = [test_mcp_tool("db")];
    let assistant_mention = [message_with_text_chunks("assistant", &["Use db for lookup."])];
    assert!(
        explicitly_referenced_mcp_servers_for_input(&assistant_mention, &mcp_tools).is_empty()
    );

    let substring_only = [message_with_text_chunks("user", &["Search the database."])];
    assert!(explicitly_referenced_mcp_servers_for_input(&substring_only, &mcp_tools).is_empty());

    let exact_user_mention = [message_with_text_chunks("user", &["Search with db."])];
    assert_eq!(
        explicitly_referenced_mcp_servers_for_input(&exact_user_mention, &mcp_tools),
        HashSet::from(["db".to_string()])
    );

    let mention_in_prior_prompt_message = [
        message_with_text_chunks("user", &["Use db to look this up."]),
        message_with_text_chunks("user", &["Continue with the same request."]),
    ];
    assert_eq!(
        explicitly_referenced_mcp_servers_for_input(&mention_in_prior_prompt_message, &mcp_tools),
        HashSet::from(["db".to_string()])
    );
}

#[test]
fn per_input_connector_selection_uses_only_complete_user_message_text() {
    let connectors = [test_connector("calendar", "Calendar")];
    let assistant_mention = [message_with_text_chunks(
        "assistant",
        &["Use [$calendar](app://calendar) for the next step."],
    )];
    assert!(filter_connectors_for_input(
        &connectors,
        &assistant_mention,
        &HashSet::new(),
        &HashMap::new(),
    )
    .is_empty());

    let user_mention_in_later_chunk = [message_with_text_chunks(
        "user",
        &["Please use ", "[$calendar](app://calendar) for this request."],
    )];
    assert_eq!(
        filter_connectors_for_input(
            &connectors,
            &user_mention_in_later_chunk,
            &HashSet::new(),
            &HashMap::new(),
        ),
        connectors
    );
}

#[test]
fn post_sampling_token_estimate_is_disabled_by_always_on_sinks() {
    let feedback = codex_feedback::CodexFeedback::new();
    let subscriber = tracing_subscriber::registry()
        .with(feedback.logger_layer())
        .with(tracing_subscriber::fmt::layer().with_filter(codex_state::log_db::default_filter()));

    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        assert!(!tracing::event_enabled!(
            target: POST_SAMPLING_TOKEN_ESTIMATE_TARGET,
            tracing::Level::TRACE,
            turn_id,
            estimated_token_count,
            message
        ));
    });
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &step_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}

#[test]
fn realtime_user_verification_notice_excludes_request_payload() {
    let event = EventMsg::ElicitationRequest(codex_protocol::approvals::ElicitationRequestEvent {
        turn_id: None,
        server_name: "private-server-name".to_string(),
        id: codex_protocol::mcp::RequestId::String("private-request-id".to_string()),
        request: codex_protocol::approvals::ElicitationRequest::UserVerification {
            meta: None,
            title: "private-title".to_string(),
            description: "private-description".to_string(),
            challenge: "private-challenge".to_string(),
        },
    });
    assert_eq!(
        realtime_text_for_event(&event),
        Some(RealtimeEventText::Handoff(
            "<user_verification_notice>User verification is required. Please respond in the app.</user_verification_notice>".to_string(),
            None,
        )),
    );
}

#[test]
fn continuous_run_block_message_points_back_to_scratchpad_policy() {
    let message = build_continuous_run_block_message(&serde_json::json!({
        "scratchpad_id": "thread-123",
        "action_policy": {
            "repos": {
                "persona-api": {
                    "forbidden_base_branches": ["main"]
                }
            }
        },
        "next_steps": ["finish registry"],
        "pending_waits": [{
            "id": "keepalive",
            "description": "Stop keepalive",
            "details": "session_id=abc"
        }]
    }));
    assert!(message.contains("Scratchpad: thread-123"));
    assert!(message.contains("Action policy:\n```json\n"));
    assert!(message.contains("\"persona-api\""));
    assert!(message.contains("\"forbidden_base_branches\""));
    assert!(message.contains("Next up:\n- finish registry"));
    assert!(message.contains("Waiting:\n- Stop keepalive (id: keepalive; details: session_id=abc)"));
}
