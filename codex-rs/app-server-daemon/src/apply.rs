//! Safe app-server daemon upgrades coordinated through the handoff journal.

use std::path::Path;

use anyhow::Result;
use anyhow::anyhow;

use crate::apply_receipt::ensure_transferable_handoff;
use crate::apply_receipt::parse_handoff_response;
use crate::apply_receipt::sanitize_failure;
use crate::apply_receipt::ApplyAttemptReceipt;
use crate::apply_receipt::ApplyOutput;
use crate::apply_receipt::ApplyPhase;
use crate::apply_receipt::HandoffRpcError;
use crate::apply_receipt::HandoffReceipt;
use crate::Daemon;
use crate::client;
use crate::settings::DaemonSettings;

const PREPARE_METHOD: &str = "thread/handoff/prepare";
const STATUS_METHOD: &str = "thread/handoff/status";
const RECOVER_METHOD: &str = "thread/handoff/recover";

fn ensure_apply_launcher(settings: &DaemonSettings) -> Result<()> {
    anyhow::ensure!(
        settings.managed_codex_path.is_some(),
        "app-server daemon apply/recover requires an explicitly configured Codex launcher (bootstrap --codex-bin PATH); standalone updates remain owned by the standalone updater"
    );
    Ok(())
}

impl Daemon {
    pub(crate) async fn apply(&self) -> Result<ApplyOutput> {
        let _operation_lock = self.acquire_operation_lock().await?;
        if let Some(receipt) = ApplyAttemptReceipt::load(&self.apply_receipt_file).await?
            && !receipt.is_resolved()
        {
            return Ok(receipt.output(
                &self.socket_path,
                client::probe(&self.socket_path)
                    .await
                    .ok()
                    .map(|info| info.app_server_version),
                None,
            ));
        }
        self.apply_fresh().await
    }

    pub(crate) async fn recover(&self) -> Result<ApplyOutput> {
        let _operation_lock = self.acquire_operation_lock().await?;
        let Some(mut attempt) = ApplyAttemptReceipt::load(&self.apply_receipt_file).await? else {
            return Err(anyhow!("no pending app-server handoff receipt to recover"));
        };
        if attempt.is_resolved() {
            return Ok(attempt.output(
                &self.socket_path,
                client::probe(&self.socket_path).await.ok().map(|info| info.app_server_version),
                None,
            ));
        }

        let settings = self.load_settings().await?;
        ensure_apply_launcher(&settings)?;
        let managed_codex_bin = self.configured_managed_codex_bin(&settings);
        if managed_codex_bin != attempt.managed_codex_path {
            return self
                .mark_needs_attention(
                    &mut attempt,
                    format!(
                        "configured Codex launcher changed from {} to {}; refusing recovery",
                        attempt.managed_codex_path.display(),
                        managed_codex_bin.display()
                    ),
                )
                .await;
        }
        self.ensure_managed_codex_bin(managed_codex_bin)?;

        let backend = self.running_backend_instance(&settings).await?;
        if backend.is_none() && client::probe(&self.socket_path).await.is_ok() {
            return self
                .mark_needs_attention(
                    &mut attempt,
                    "app server is running but is not managed by codex app-server daemon",
                )
                .await;
        }
        if backend.is_none() {
            attempt.phase = ApplyPhase::Starting;
            attempt.save(&self.apply_receipt_file).await?;
            if let Err(error) = self
                .start_managed_backend_with_bin(&settings, managed_codex_bin)
                .await
            {
                return self
                    .mark_needs_attention(&mut attempt, error.to_string())
                    .await;
            }
        }

        let info = match self.wait_until_ready(managed_codex_bin).await {
            Ok(info) => info,
            Err(error) => {
                return self
                    .mark_needs_attention(&mut attempt, error.to_string())
                    .await;
            }
        };
        self.recover_attempt(attempt, managed_codex_bin, info).await
    }

    pub(crate) async fn apply_status(&self) -> Result<ApplyOutput> {
        let Some(attempt) = ApplyAttemptReceipt::load(&self.apply_receipt_file).await? else {
            return Err(anyhow!("no app-server handoff receipt has been recorded"));
        };
        let app_server_version = client::probe(&self.socket_path)
            .await
            .ok()
            .map(|info| info.app_server_version);
        if attempt.is_resolved() || app_server_version.is_none() {
            return Ok(attempt.output(&self.socket_path, app_server_version, None));
        }
        let fallback = attempt.clone();
        match self
            .request_handoff(STATUS_METHOD, &attempt.handoff.handoff_id)
            .await
        {
            Ok(status) => {
                let mut view = attempt;
                view.handoff = status;
                view.phase = match view.handoff.state.as_str() {
                    "completed" => ApplyPhase::Applied,
                    "needsAttention" => ApplyPhase::NeedsAttention,
                    "prepared" => ApplyPhase::Prepared,
                    "draining" | "suspended" | "restoring" => ApplyPhase::Recovering,
                    _ => ApplyPhase::NeedsAttention,
                };
                Ok(view.output(&self.socket_path, app_server_version, None))
            }
            Err(error) => Ok(fallback.output(
                &self.socket_path,
                app_server_version,
                Some(sanitize_failure(&error.to_string())),
            )),
        }
    }

    async fn apply_fresh(&self) -> Result<ApplyOutput> {
        let settings = self.load_settings().await?;
        ensure_apply_launcher(&settings)?;
        let managed_codex_bin = self.configured_managed_codex_bin(&settings);
        self.ensure_managed_codex_bin(managed_codex_bin)?;
        let Some(backend) = self.running_backend_instance(&settings).await? else {
            if client::probe(&self.socket_path).await.is_ok() {
                return Err(anyhow!(
                    "app server is running but is not managed by codex app-server daemon"
                ));
            }
            return Err(anyhow!("app server is not running; start it before applying an update"));
        };

        let message =
            client::request(&self.socket_path, PREPARE_METHOD, None).await?;
        let handoff = match parse_handoff_response(message, PREPARE_METHOD) {
            Ok(receipt) => receipt,
            Err(error) => {
                let failure = sanitize_failure(&error.to_string());
                if let Some(receipt) = error.receipt {
                    let mut attempt = ApplyAttemptReceipt::new(
                        receipt,
                        managed_codex_bin.to_path_buf(),
                        self.managed_codex_version_best_effort(managed_codex_bin).await,
                    );
                    return self.mark_needs_attention(&mut attempt, failure).await;
                }
                return Err(anyhow!(failure));
            }
        };
        if let Err(error) = ensure_transferable_handoff(&handoff) {
            let mut attempt = ApplyAttemptReceipt::new(
                handoff,
                managed_codex_bin.to_path_buf(),
                self.managed_codex_version_best_effort(managed_codex_bin).await,
            );
            return self
                .mark_needs_attention(&mut attempt, sanitize_failure(&error.to_string()))
                .await;
        }

        let mut attempt = ApplyAttemptReceipt::new(
            handoff,
            managed_codex_bin.to_path_buf(),
            self.managed_codex_version_best_effort(managed_codex_bin).await,
        );
        attempt.save(&self.apply_receipt_file).await?;
        if let Err(error) = backend.stop().await {
            return self.mark_needs_attention(&mut attempt, error.to_string()).await;
        }

        attempt.phase = ApplyPhase::Starting;
        attempt.save(&self.apply_receipt_file).await?;
        if let Err(error) = self
            .start_managed_backend_with_bin(&settings, managed_codex_bin)
            .await
        {
            return self.mark_needs_attention(&mut attempt, error.to_string()).await;
        }
        let info = match self.wait_until_ready(managed_codex_bin).await {
            Ok(info) => info,
            Err(error) => {
                return self
                    .mark_needs_attention(&mut attempt, error.to_string())
                    .await;
            }
        };
        self.recover_attempt(attempt, managed_codex_bin, info).await
    }

    async fn recover_attempt(
        &self,
        mut attempt: ApplyAttemptReceipt,
        managed_codex_bin: &Path,
        info: client::ProbeInfo,
    ) -> Result<ApplyOutput> {
        attempt.phase = ApplyPhase::Recovering;
        attempt.save(&self.apply_receipt_file).await?;
        match self
            .request_handoff(STATUS_METHOD, &attempt.handoff.handoff_id)
            .await
        {
            Ok(status) => {
                attempt.handoff = status;
                if attempt.handoff.state == "completed" {
                    attempt.phase = ApplyPhase::Applied;
                    attempt.failure = None;
                    attempt.save(&self.apply_receipt_file).await?;
                    return Ok(attempt.output(
                        &self.socket_path,
                        Some(info.app_server_version.clone()),
                        None,
                    ));
                }
                if attempt.handoff.state == "needsAttention" {
                    return self
                        .mark_needs_attention(
                            &mut attempt,
                            "coordinator reported needsAttention before recovery",
                        )
                        .await;
                }
                if !matches!(
                    attempt.handoff.state.as_str(),
                    "prepared" | "draining" | "suspended" | "restoring"
                ) {
                    return self
                        .mark_needs_attention(&mut attempt, "coordinator returned an unknown handoff state")
                        .await;
                }
            }
            Err(error) => {
                let failure = error.to_string();
                if let Some(receipt) = error.receipt {
                    attempt.handoff = receipt;
                }
                return self.mark_needs_attention(&mut attempt, failure).await;
            }
        }

        match self
            .request_handoff(RECOVER_METHOD, &attempt.handoff.handoff_id)
            .await
        {
            Ok(receipt) if receipt.state == "completed" => {
                attempt.handoff = receipt;
                attempt.phase = ApplyPhase::Applied;
                attempt.failure = None;
                attempt.managed_codex_version =
                    self.managed_codex_version_best_effort(managed_codex_bin).await;
                attempt.save(&self.apply_receipt_file).await?;
                Ok(attempt.output(
                    &self.socket_path,
                    Some(info.app_server_version),
                    None,
                ))
            }
            Ok(receipt) => {
                attempt.handoff = receipt;
                self.mark_needs_attention(
                    &mut attempt,
                    "coordinator did not complete exact handoff recovery",
                )
                .await
            }
            Err(error) => {
                let failure = error.to_string();
                if let Some(receipt) = error.receipt {
                    attempt.handoff = receipt;
                }
                self.mark_needs_attention(&mut attempt, failure).await
            }
        }
    }

    async fn request_handoff(
        &self,
        method: &str,
        handoff_id: &str,
    ) -> std::result::Result<HandoffReceipt, HandoffRpcError> {
        let message = client::request(
            &self.socket_path,
            method,
            Some(serde_json::json!({ "handoffId": handoff_id })),
        )
        .await
        .map_err(|error| HandoffRpcError {
            method: method.to_string(),
            message: error.to_string(),
            receipt: None,
        })?;
        parse_handoff_response(message, method)
    }

    async fn mark_needs_attention(
        &self,
        attempt: &mut ApplyAttemptReceipt,
        error: impl std::fmt::Display,
    ) -> Result<ApplyOutput> {
        let failure = sanitize_failure(&error.to_string());
        attempt.phase = ApplyPhase::NeedsAttention;
        attempt.failure = Some(failure.clone());
        attempt.save(&self.apply_receipt_file).await?;
        Ok(attempt.output(
            &self.socket_path,
            client::probe(&self.socket_path).await.ok().map(|info| info.app_server_version),
            Some(failure),
        ))
    }
}


#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
