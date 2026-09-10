use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codex_core::SleepFuture;
use codex_core::TimeFuture;
use codex_core::TimeProvider;
use codex_core::TurnInput;
use codex_core::TurnInputRequest;
use codex_core::config::CurrentTimeReminderConfig;
use codex_features::CurrentTimeSource;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::ThreadUsagePolicy;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::submit_thread_settings;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const FUTURE_RESET_AT: &str = "4102444800";
const TEST_NOW: i64 = 1_700_000_000;

fn enable_external_clock(config: &mut codex_core::config::Config) {
    config
        .features
        .enable(Feature::CurrentTimeReminder)
        .expect("test config should allow current-time reminders");
    config.current_time_reminder = Some(CurrentTimeReminderConfig {
        clock_source: CurrentTimeSource::External,
        ..CurrentTimeReminderConfig::default()
    });
}

struct ImmediateTimeProvider {
    now: AtomicI64,
    last_sleep_seconds: AtomicU64,
}

impl ImmediateTimeProvider {
    fn new(now: i64) -> Self {
        Self {
            now: AtomicI64::new(now),
            last_sleep_seconds: AtomicU64::new(0),
        }
    }
}

impl TimeProvider for ImmediateTimeProvider {
    fn current_time(&self, _thread_id: ThreadId) -> TimeFuture<'_> {
        let now = self.now.load(Ordering::Relaxed);
        Box::pin(async move {
            DateTime::<Utc>::from_timestamp(now, 0).context("test timestamp should be valid")
        })
    }

    fn sleep(&self, _thread_id: ThreadId, duration: Duration) -> SleepFuture<'_> {
        self.last_sleep_seconds
            .store(duration.as_secs(), Ordering::Relaxed);
        self.now.fetch_add(
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
            Ordering::Relaxed,
        );
        Box::pin(async { Ok(()) })
    }
}

async fn mount_account_usage(
    server: &MockServer,
    used_percent: i32,
    reset_at: i32,
    delay: Option<Duration>,
) {
    let mut response = ResponseTemplate::new(200).set_body_json(json!({
        "plan_type": "plus",
        "rate_limit": {
            "allowed": used_percent < 100,
            "limit_reached": used_percent >= 100,
            "primary_window": {
                "used_percent": used_percent,
                "limit_window_seconds": 300,
                "reset_after_seconds": 0,
                "reset_at": reset_at
            }
        },
        "credits": null,
        "spend_control": null
    }));
    if let Some(delay) = delay {
        response = response.set_delay(delay);
    }
    Mock::given(method("GET"))
        .and(path_regex(".*/api/codex/usage$"))
        .respond_with(response)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_policy_and_provider_budget_are_model_visible() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let first_response = ResponseTemplate::new(429)
        .insert_header("x-codex-primary-used-percent", "70.0")
        .insert_header("x-codex-secondary-used-percent", "80.0")
        .insert_header("x-codex-primary-window-minutes", "300")
        .insert_header("x-codex-secondary-window-minutes", "10080")
        .insert_header("x-codex-primary-reset-at", FUTURE_RESET_AT)
        .insert_header("x-codex-secondary-reset-at", FUTURE_RESET_AT)
        .insert_header("x-codex-rate-limit-reached-type", "rate_limit_reached")
        .set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "limit reached",
                "resets_at": 1
            }
        }));
    let second_response = sse(vec![
        ev_response_created("response-2"),
        ev_assistant_message("message-2", "status recorded"),
        ev_completed("response-2"),
    ]);
    let third_response = sse_response(sse(vec![
        ev_response_created("response-3"),
        ev_assistant_message("message-3", "status observed"),
        ev_completed("response-3"),
    ]));
    let responses = mount_response_sequence(
        &server,
        vec![
            first_response,
            sse_response(second_response),
            third_response,
        ],
    )
    .await;

    let mut builder = test_codex();
    let test = builder.build_with_auto_env(&server).await?;
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            usage_policy: Some(ThreadUsagePolicy {
                auto_resume: true,
                minimum_remaining_percent: Some(20),
            }),
            ..Default::default()
        },
    )
    .await?;

    test.submit_text_turn("record provider usage").await?;
    test.submit_text_turn("show provider usage").await?;

    let request = responses
        .requests()
        .get(2)
        .cloned()
        .context("explicit turn request should be captured")?;
    let usage_context = request
        .message_input_texts("developer")
        .into_iter()
        .rfind(|text| text.starts_with("<thread_usage_limits>"))
        .context("provider usage should be visible to the model")?;
    assert!(usage_context.contains("Automatic resume after a reset is enabled for this thread."));
    assert!(usage_context.contains("less than 20% remaining"));
    assert!(
        usage_context.contains("5-hour window: 30% remaining"),
        "unexpected usage context: {usage_context}"
    );
    assert!(usage_context.contains("weekly window: 20% remaining"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_resume_is_disabled_by_default() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let limited_response = ResponseTemplate::new(429).set_body_json(json!({
        "error": {
            "type": "usage_limit_reached",
            "message": "limit reached",
            "resets_at": 1
        }
    }));
    let responses = mount_response_sequence(&server, vec![limited_response]).await;

    let mut builder = test_codex();
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_text_turn("do not resume automatically").await?;

    assert_eq!(responses.requests().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_resume_retries_a_resettable_usage_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let limited_response = ResponseTemplate::new(429)
        .insert_header("x-codex-primary-used-percent", "100.0")
        .insert_header("x-codex-primary-window-minutes", "300")
        .insert_header("x-codex-primary-reset-at", "1")
        .insert_header("x-codex-rate-limit-reached-type", "rate_limit_reached")
        .set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "limit reached",
                "resets_at": 1
            }
        }));
    let completion = sse_response(sse(vec![
        ev_response_created("response-2"),
        ev_assistant_message("message-2", "resumed"),
        ev_completed("response-2"),
    ]));
    let responses = mount_response_sequence(&server, vec![limited_response, completion]).await;

    let time_provider = Arc::new(ImmediateTimeProvider::new(TEST_NOW));
    let mut builder = test_codex()
        .with_config(|config| {
            config.tui_usage_auto_resume.check_interval_minutes = 1;
            enable_external_clock(config);
        })
        .with_external_time_provider(time_provider.clone());
    let test = builder.build_with_auto_env(&server).await?;
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            usage_policy: Some(ThreadUsagePolicy {
                auto_resume: true,
                minimum_remaining_percent: None,
            }),
            ..Default::default()
        },
    )
    .await?;

    test.submit_text_turn("resume after the reset").await?;

    assert_eq!(responses.requests().len(), 2);
    assert_eq!(time_provider.last_sleep_seconds.load(Ordering::Relaxed), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_resume_refreshes_account_usage_before_retrying() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let limited_response = ResponseTemplate::new(429)
        .insert_header("x-codex-primary-used-percent", "100.0")
        .insert_header("x-codex-primary-window-minutes", "300")
        .insert_header("x-codex-primary-reset-at", (TEST_NOW + 600).to_string())
        .insert_header("x-codex-rate-limit-reached-type", "rate_limit_reached")
        .set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "limit reached",
                "resets_at": TEST_NOW + 600
            }
        }));
    let completion = sse_response(sse(vec![
        ev_response_created("response-2"),
        ev_assistant_message("message-2", "resumed after account refresh"),
        ev_completed("response-2"),
    ]));
    let responses = mount_response_sequence(&server, vec![limited_response, completion]).await;
    mount_account_usage(&server, 20, (TEST_NOW + 600) as i32, None).await;

    let time_provider = Arc::new(ImmediateTimeProvider::new(TEST_NOW));
    let chatgpt_base_url = server.uri();
    let model_provider_base_url = format!("{chatgpt_base_url}/backend-api/codex");
    let mut builder = test_codex()
        .with_auth(CodexAuth::from_external_chatgpt_tokens(
            "header.e30.external",
            "test-account",
            Some("plus"),
        )?)
        .with_config(move |config| {
            config.tui_usage_auto_resume.check_interval_minutes = 1;
            config.chatgpt_base_url = chatgpt_base_url;
            config.model_provider.base_url = Some(model_provider_base_url);
            enable_external_clock(config);
        })
        .with_external_time_provider(time_provider.clone());
    let test = builder.build_with_auto_env(&server).await?;
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            usage_policy: Some(ThreadUsagePolicy {
                auto_resume: true,
                minimum_remaining_percent: None,
            }),
            ..Default::default()
        },
    )
    .await?;

    test.submit_text_turn("refresh account usage before retrying")
        .await?;

    assert_eq!(responses.requests().len(), 2);
    assert_eq!(time_provider.last_sleep_seconds.load(Ordering::Relaxed), 60);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_auto_resume_during_account_refresh_prevents_retry() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let limited_response = ResponseTemplate::new(429)
        .insert_header("x-codex-primary-used-percent", "100.0")
        .insert_header("x-codex-primary-window-minutes", "300")
        .insert_header("x-codex-primary-reset-at", (TEST_NOW + 600).to_string())
        .insert_header("x-codex-rate-limit-reached-type", "rate_limit_reached")
        .set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "limit reached",
                "resets_at": TEST_NOW + 600
            }
        }));
    let responses = mount_response_sequence(&server, vec![limited_response]).await;
    mount_account_usage(
        &server,
        20,
        (TEST_NOW + 600) as i32,
        Some(Duration::from_millis(500)),
    )
    .await;

    let chatgpt_base_url = server.uri();
    let model_provider_base_url = format!("{chatgpt_base_url}/backend-api/codex");
    let mut builder = test_codex()
        .with_auth(CodexAuth::from_external_chatgpt_tokens(
            "header.e30.external",
            "test-account",
            Some("plus"),
        )?)
        .with_config(move |config| {
            config.tui_usage_auto_resume.check_interval_minutes = 1;
            config.chatgpt_base_url = chatgpt_base_url;
            config.model_provider.base_url = Some(model_provider_base_url);
            enable_external_clock(config);
        })
        .with_external_time_provider(Arc::new(ImmediateTimeProvider::new(TEST_NOW)));
    let test = builder.build_with_auto_env(&server).await?;
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            usage_policy: Some(ThreadUsagePolicy {
                auto_resume: true,
                minimum_remaining_percent: None,
            }),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_turn_if_idle(TurnInputRequest::new(TurnInput::ResponseItem(
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "disable recovery while usage is refreshing".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
        )))
        .await?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let saw_usage_refresh = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|request| request.url.path().ends_with("/api/codex/usage"));
        if saw_usage_refresh {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for account usage refresh request");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            usage_policy: Some(ThreadUsagePolicy {
                auto_resume: false,
                minimum_remaining_percent: None,
            }),
            ..Default::default()
        },
    )
    .await?;

    loop {
        if matches!(
            wait_for_event(&test.codex, |_| true).await,
            EventMsg::TurnComplete(_)
        ) {
            break;
        }
    }

    assert_eq!(responses.requests().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_floor_stops_a_harness_follow_up() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "plan-tool-call";
    let plan_args = json!({
        "explanation": "Usage floor test",
        "plan": [{"step": "continue", "status": "in_progress"}]
    })
    .to_string();
    let response = sse_response(sse(vec![
        ev_response_created("response-1"),
        ev_function_call(call_id, "update_plan", &plan_args),
        ev_completed("response-1"),
    ]))
    .insert_header("x-codex-primary-used-percent", "90.0")
    .insert_header("x-codex-primary-window-minutes", "300")
    .insert_header("x-codex-primary-reset-at", FUTURE_RESET_AT);
    let responses = mount_response_sequence(&server, vec![response]).await;

    let mut builder = test_codex().with_config(|config| config.update_plan_enabled = true);
    let test = builder.build_with_auto_env(&server).await?;
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            usage_policy: Some(ThreadUsagePolicy {
                auto_resume: false,
                minimum_remaining_percent: Some(20),
            }),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_turn_if_idle(TurnInputRequest::new(TurnInput::ResponseItem(
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "update the plan".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
        )))
        .await?;

    let mut warnings = Vec::new();
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Warning(warning) => warnings.push(warning.message),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(responses.requests().len(), 1);
    assert_eq!(
        warnings,
        vec![
            "Automatic continuation stopped because known provider usage is below the configured 20% remaining floor."
                .to_string()
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_floor_auto_resume_waits_for_recovered_account_usage() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "plan-tool-call";
    let plan_args = json!({
        "explanation": "Usage floor auto-resume test",
        "plan": [{"step": "continue", "status": "in_progress"}]
    })
    .to_string();
    let first_response = sse_response(sse(vec![
        ev_response_created("response-1"),
        ev_function_call(call_id, "update_plan", &plan_args),
        ev_completed("response-1"),
    ]))
    .insert_header("x-codex-primary-used-percent", "90.0")
    .insert_header("x-codex-primary-window-minutes", "300")
    .insert_header("x-codex-primary-reset-at", (TEST_NOW + 600).to_string());
    let completion = sse_response(sse(vec![
        ev_response_created("response-2"),
        ev_assistant_message("message-2", "resumed after floor recovery"),
        ev_completed("response-2"),
    ]));
    let responses = mount_response_sequence(&server, vec![first_response, completion]).await;
    mount_account_usage(&server, 70, (TEST_NOW + 600) as i32, None).await;

    let time_provider = Arc::new(ImmediateTimeProvider::new(TEST_NOW));
    let chatgpt_base_url = server.uri();
    let model_provider_base_url = format!("{chatgpt_base_url}/backend-api/codex");
    let mut builder = test_codex()
        .with_auth(CodexAuth::from_external_chatgpt_tokens(
            "header.e30.external",
            "test-account",
            Some("plus"),
        )?)
        .with_config(move |config| {
            config.update_plan_enabled = true;
            config.tui_usage_auto_resume.check_interval_minutes = 1;
            config.chatgpt_base_url = chatgpt_base_url;
            config.model_provider.base_url = Some(model_provider_base_url);
            enable_external_clock(config);
        })
        .with_external_time_provider(time_provider.clone());
    let test = builder.build_with_auto_env(&server).await?;
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            usage_policy: Some(ThreadUsagePolicy {
                auto_resume: true,
                minimum_remaining_percent: Some(20),
            }),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_turn_if_idle(TurnInputRequest::new(TurnInput::ResponseItem(
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "update the plan after usage recovery".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
        )))
        .await?;

    let mut saw_recovery_warning = false;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Warning(warning) => {
                saw_recovery_warning |= warning.message.contains("Usage reset detected");
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(responses.requests().len(), 2);
    assert!(saw_recovery_warning);
    assert_eq!(time_provider.last_sleep_seconds.load(Ordering::Relaxed), 60);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_floor_does_not_block_an_explicit_user_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "plan-tool-call";
    let plan_args = json!({
        "explanation": "Explicit turn floor test",
        "plan": [{"step": "continue", "status": "in_progress"}]
    })
    .to_string();
    let first_response = sse_response(sse(vec![
        ev_response_created("response-1"),
        ev_function_call(call_id, "update_plan", &plan_args),
        ev_completed("response-1"),
    ]))
    .insert_header("x-codex-primary-used-percent", "90.0")
    .insert_header("x-codex-primary-window-minutes", "300")
    .insert_header("x-codex-primary-reset-at", "1700000000");
    let completion = sse_response(sse(vec![
        ev_response_created("response-2"),
        ev_assistant_message("message-2", "finished"),
        ev_completed("response-2"),
    ]));
    let responses = mount_response_sequence(&server, vec![first_response, completion]).await;

    let mut builder = test_codex().with_config(|config| config.update_plan_enabled = true);
    let test = builder.build_with_auto_env(&server).await?;
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            usage_policy: Some(ThreadUsagePolicy {
                auto_resume: false,
                minimum_remaining_percent: Some(20),
            }),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "update the plan".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let mut warnings = Vec::new();
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Warning(warning) => warnings.push(warning.message),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(responses.requests().len(), 2);
    assert!(warnings.iter().all(|warning| {
        !warning.contains("Automatic continuation stopped because known provider usage")
    }));
    Ok(())
}
