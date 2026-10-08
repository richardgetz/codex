//! Account-scoped Daybreak refusal guidance and per-turn program selection.
//! Pending or failed discovery never implies access.

use crate::app_server_session::AppServerSession;
use crate::legacy_core::config::Config;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::AuthMode;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::GetAuthStatusParams;
use codex_app_server_protocol::GetAuthStatusResponse;
use codex_app_server_protocol::RequestId;
use codex_http_client::ClientRouteClass;
use codex_http_client::RouteAwareClientPool;
use codex_login::CodexAuth;
use codex_protocol::openai_models::ModelAccessPrograms;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::turn_input::CyberAccessProgram;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

pub(crate) type NoticeCache = Arc<OnceCell<Notice>>;

/// Fetch account-scoped eligibility without blocking startup or turn handling.
pub(crate) fn prefetch_notice(config: &Config, server: &AppServerSession, cache: NoticeCache) {
    if config.model_provider_id != "openai" || server.uses_remote_workspace() || cache.get().is_some()
    {
        return;
    }
    let config = config.clone();
    let request_handle = server.request_handle();
    tokio::spawn(async move {
        cache
            .get_or_init(|| read_notice(&config, &request_handle))
            .await;
    });
}

async fn read_notice(config: &Config, request_handle: &AppServerRequestHandle) -> Notice {
    tokio::time::timeout(Duration::from_secs(3), async {
        let status: GetAuthStatusResponse = request_handle
            .request_typed(ClientRequest::GetAuthStatus {
                request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
                params: GetAuthStatusParams {
                    include_token: Some(true),
                    refresh_token: Some(false),
                },
            })
            .await
            .ok()?;
        let auth = config
            .auth_config()
            .load_auth(/*enable_codex_api_key_env*/ false)
            .await
            .ok()
            .flatten()?;
        let CodexAuth::Chatgpt(_) = &auth else {
            return None;
        };
        if status.auth_method != Some(AuthMode::Chatgpt)
            || status.auth_token.as_deref() != Some(auth.get_token().ok()?.as_str())
        {
            return None;
        }
        let client = RouteAwareClientPool::new_without_redirects(
            config.http_client_factory(),
            ClientRouteClass::Api,
        );
        let url = format!(
            "{}/accounts/verified_access",
            config.chatgpt_base_url.trim_end_matches('/')
        );
        let response = client
            .get(url)
            .headers(codex_model_provider::auth_provider_from_auth(&auth).to_auth_headers())
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let access = response.json::<VerifiedAccess>().await.ok()?;
        let current_auth = config
            .auth_config()
            .load_auth(/*enable_codex_api_key_env*/ false)
            .await
            .ok()
            .flatten()?;
        if current_auth.get_token().ok()? != auth.get_token().ok()?
            || current_auth.get_account_id() != auth.get_account_id()
        {
            return None;
        }
        Some(access.notice())
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

#[derive(Deserialize)]
struct VerifiedAccess {
    programs: Vec<Program>,
}

#[derive(Deserialize)]
#[serde(tag = "program", rename_all = "snake_case")]
enum Program {
    Cyber {
        state: String,
        grants: Vec<serde::de::IgnoredAny>,
    },
    #[serde(other)]
    Other,
}

impl VerifiedAccess {
    fn notice(&self) -> Notice {
        let absent = self.programs.iter().all(|program| match program {
            Program::Cyber { state, grants } => state == "inactive" && grants.is_empty(),
            Program::Other => true,
        });
        if absent {
            Notice::Apply
        } else {
            Notice::Limited
        }
    }
}

pub(crate) fn program_for_turn(
    models: &[ModelPreset],
    model: &str,
    eligible_account: bool,
    enabled: bool,
) -> Result<Option<CyberAccessProgram>, String> {
    if !eligible_account {
        return if enabled {
            Err("Daybreak requires a signed-in ChatGPT account and the OpenAI provider. Turn it off to continue.".into())
        } else {
            Ok(None)
        };
    }
    let programs = models
        .iter()
        .find(|entry| entry.model == model)
        .and_then(|entry| entry.available_access_programs.as_ref());
    if enabled {
        programs
            .and_then(codex_protocol::openai_models::ModelAccessPrograms::daybreak)
            .map(Some)
            .ok_or_else(|| format!("Daybreak support for model {model} could not be confirmed by the connected server. Use /daybreak to turn it off, or choose a compatible model and server."))
    } else {
        Ok(programs.and_then(codex_protocol::openai_models::ModelAccessPrograms::standard))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Notice {
    Apply,
    Astra,
    #[default]
    Limited,
    Disabled,
    Enabled,
}

impl Notice {
    /// Preserve the stable refusal copy where the catalog does not provide a more specific one.
    pub(crate) fn for_model(self, model: &str) -> Self {
        match model {
            "gpt-6-astra" | "gpt-6-astra-wm" => Self::Astra,
            _ => self,
        }
    }
}

pub(crate) fn notice_for_setting(
    models: &[ModelPreset],
    model: &str,
    enabled: bool,
    can_enable_daybreak: bool,
) -> Notice {
    if enabled {
        return Notice::Enabled;
    }
    if can_enable_daybreak {
        let programs = models
            .iter()
            .find(|entry| entry.model == model)
            .and_then(|entry| entry.available_access_programs.as_ref());
        if matches!(model, "gpt-6-astra" | "gpt-6-astra-wm")
            && programs.is_some_and(|programs| {
                programs.cyber.contains(&CyberAccessProgram::Standard)
                    && programs.daybreak().is_none()
            })
        {
            return Notice::Astra;
        }
        if programs.and_then(ModelAccessPrograms::daybreak).is_some() {
            return Notice::Disabled;
        }
    }
    Notice::Apply
}

pub(crate) fn available(models: &[ModelPreset]) -> bool {
    models.iter().any(|model| {
        model
            .available_access_programs
            .as_ref()
            .and_then(codex_protocol::openai_models::ModelAccessPrograms::daybreak)
            .is_some()
    })
}

/// Missing or empty program lists do not establish that the account lacks access.
pub(crate) fn availability(models: &[ModelPreset]) -> Option<bool> {
    if available(models) {
        return Some(true);
    }
    if models.is_empty() {
        return None;
    }
    models
        .iter()
        .all(|model| {
            model
                .available_access_programs
                .as_ref()
                .is_some_and(|programs| !programs.cyber.is_empty())
        })
        .then_some(false)
}

#[cfg(test)]
#[path = "daybreak_tests.rs"]
mod tests;
