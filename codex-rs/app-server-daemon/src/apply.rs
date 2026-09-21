//! Safe app-server daemon upgrades coordinated through the handoff journal.

use std::future::Future;
use std::path::Path;

use anyhow::Result;
use anyhow::anyhow;

use crate::Daemon;
use crate::apply_receipt::ApplyAttemptReceipt;
use crate::apply_receipt::ApplyOutput;
use crate::apply_receipt::ApplyPhase;
use crate::apply_receipt::HandoffReceipt;
use crate::apply_receipt::HandoffRpcError;
use crate::apply_receipt::ensure_transferable_handoff;
use crate::apply_receipt::parse_handoff_response;
use crate::apply_receipt::sanitize_failure;
use crate::client;
use crate::settings::DaemonSettings;

const PREPARE_METHOD: &str = "thread/handoff/prepare";
const STATUS_METHOD: &str = "thread/handoff/status";
const RECOVER_METHOD: &str = "thread/handoff/recover";

async fn stop_backend_with_receipt<F>(
    attempt: &mut ApplyAttemptReceipt,
    receipt_path: &Path,
    stop: F,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    // Persist ownership before awaiting the external stop. A crash or cancellation here leaves
    // a receipt that retries the same managed backend instead of assuming it was replaced.
    attempt.stop_started = Some(true);
    attempt.save(receipt_path).await?;
    stop.await?;
    attempt.stop_completed = Some(true);
    attempt.phase = ApplyPhase::Starting;
    attempt.save(receipt_path).await?;
    Ok(())
}

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
            && receipt.blocks_new_apply()
        {
            return Ok(receipt.output_with_running_version(
                &self.socket_path,
                client::probe(&self.socket_path)
                    .await
                    .ok()
                    .map(|info| info.app_server_version),
                /*error*/ None,
                self.running_managed_codex_version_best_effort().await,
            ));
        }
        self.apply_fresh().await
    }

    pub(crate) async fn recover(&self) -> Result<ApplyOutput> {
        self.recover_with_resolution(None).await
    }

    pub(crate) async fn quarantine(&self) -> Result<ApplyOutput> {
        self.recover_with_resolution(Some("quarantine")).await
    }

    async fn recover_with_resolution(&self, resolution: Option<&str>) -> Result<ApplyOutput> {
        let _operation_lock = self.acquire_operation_lock().await?;
        let Some(mut attempt) = ApplyAttemptReceipt::load(&self.apply_receipt_file).await? else {
            return Err(anyhow!("no pending app-server handoff receipt to recover"));
        };
        if attempt.is_resolved() {
            return Ok(attempt.output_with_running_version(
                &self.socket_path,
                client::probe(&self.socket_path)
                    .await
                    .ok()
                    .map(|info| info.app_server_version),
                /*error*/ None,
                self.running_managed_codex_version_best_effort().await,
            ));
        }
        if resolution.is_none() && !attempt.blocks_new_apply() {
            return Ok(attempt.output_with_running_version(
                &self.socket_path,
                client::probe(&self.socket_path)
                    .await
                    .ok()
                    .map(|info| info.app_server_version),
                /*error*/ None,
                self.running_managed_codex_version_best_effort().await,
            ));
        }
        if resolution.is_none()
            && attempt.phase == ApplyPhase::NeedsAttention
            && attempt.handoff.state == "needsAttention"
            && attempt.handoff.transfer_started == Some(false)
        {
            // Preparation failed before the old runtime crossed the drain boundary. Keep that
            // runtime alive for an explicit quarantine or a fresh apply; restarting it here would
            // lose the only owner that can still report the unresolved blocker.
            return Ok(attempt.output_with_running_version(
                &self.socket_path,
                client::probe(&self.socket_path)
                    .await
                    .ok()
                    .map(|info| info.app_server_version),
                /*error*/ None,
                self.running_managed_codex_version_best_effort().await,
            ));
        }
        if resolution.is_none() && attempt.should_hold_legacy_owner() {
            // A legacy receipt cannot prove whether the old runtime crossed the drain boundary.
            // Keep it alive until an operator explicitly retries with a newer receipt or
            // quarantines the affected roots; stopping an ambiguous owner would discard the only
            // process that can still report its exact blocker.
            return Ok(attempt.output_with_running_version(
                &self.socket_path,
                client::probe(&self.socket_path)
                    .await
                    .ok()
                    .map(|info| info.app_server_version),
                /*error*/ None,
                self.running_managed_codex_version_best_effort().await,
            ));
        }

        if resolution == Some("quarantine") {
            // Quarantine normally uses the currently running coordinator and durable state DB.
            // If the managed backend and socket are both gone, start a replacement without
            // stopping anything: an absent managed PID is positive evidence that there is no
            // daemon-owned process left to discard, while a stale unmanaged process still makes
            // the bind/start operation fail closed.
            let mut managed_codex_path = attempt.managed_codex_path.clone();
            let info = match client::probe(&self.socket_path).await {
                Ok(info) => info,
                Err(error) => {
                    let settings = match self.load_settings().await {
                        Ok(settings) => settings,
                        Err(settings_error) => {
                            return self
                                .mark_needs_attention(
                                    &mut attempt,
                                    format!(
                                        "cannot quarantine without a reachable app server ({error}); \
                                         loading daemon settings also failed: {settings_error}"
                                    ),
                                )
                                .await;
                        }
                    };
                    let backend = match self.running_backend_instance(&settings).await {
                        Ok(backend) => backend,
                        Err(backend_error) => {
                            return self
                                .mark_needs_attention(
                                    &mut attempt,
                                    format!(
                                        "cannot quarantine without a reachable app server ({error}); \
                                         checking managed backend failed: {backend_error}"
                                    ),
                                )
                                .await;
                        }
                    };
                    if backend.is_some() {
                        return self
                            .mark_needs_attention(
                                &mut attempt,
                                format!(
                                    "cannot quarantine without a reachable app server: {error}; \
                                     managed backend is still running, so it was left untouched"
                                ),
                            )
                            .await;
                    }

                    managed_codex_path = self.configured_managed_codex_bin(&settings).to_path_buf();
                    if let Err(start_error) = self.ensure_managed_codex_bin(&managed_codex_path) {
                        return self
                            .mark_needs_attention(
                                &mut attempt,
                                format!(
                                    "cannot quarantine without a reachable app server: {error}; \
                                     replacement launcher is unavailable: {start_error}"
                                ),
                            )
                            .await;
                    }
                    if let Err(start_error) = self
                        .start_managed_backend_with_bin(&settings, &managed_codex_path)
                        .await
                    {
                        return self
                            .mark_needs_attention(
                                &mut attempt,
                                format!(
                                    "cannot quarantine without a reachable app server: {error}; \
                                     replacement start failed: {start_error}"
                                ),
                            )
                            .await;
                    }
                    match self.wait_until_ready(&managed_codex_path).await {
                        Ok(info) => info,
                        Err(start_error) => {
                            return self
                                .mark_needs_attention(
                                    &mut attempt,
                                    format!(
                                        "cannot quarantine without a reachable app server: {error}; \
                                         replacement did not become ready: {start_error}"
                                    ),
                                )
                                .await;
                        }
                    }
                }
            };
            return self
                .recover_attempt(attempt, &managed_codex_path, info, resolution)
                .await;
        }

        let settings = self.load_settings().await?;
        ensure_apply_launcher(&settings)?;
        let managed_codex_bin = self.configured_managed_codex_bin(&settings);
        if managed_codex_bin != attempt.managed_codex_path {
            let failure = format!(
                "configured Codex launcher changed from {} to {}; refusing recovery",
                attempt.managed_codex_path.display(),
                managed_codex_bin.display()
            );
            return self.mark_needs_attention(&mut attempt, failure).await;
        }
        self.ensure_managed_codex_bin(managed_codex_bin)?;

        let mut backend = self.running_backend_instance(&settings).await?;
        if resolution.is_none() && attempt.stop_completed != Some(true) {
            if let Some(backend_instance) = backend.as_ref() {
                if let Err(error) = stop_backend_with_receipt(
                    &mut attempt,
                    &self.apply_receipt_file,
                    backend_instance.stop(),
                )
                .await
                {
                    return self
                        .mark_needs_attention(&mut attempt, error.to_string())
                        .await;
                }
                // `running_backend_instance` is a snapshot; do not use it to skip launching the
                // replacement after the old process has just been stopped.
                backend = None;
            } else if client::probe(&self.socket_path).await.is_ok() {
                return self
                    .mark_needs_attention(
                        &mut attempt,
                        "app server is running but is not managed by codex app-server daemon",
                    )
                    .await;
            }
            if attempt.stop_completed != Some(true) {
                attempt.stop_completed = Some(true);
                attempt.phase = ApplyPhase::Starting;
                attempt.save(&self.apply_receipt_file).await?;
            }
        }
        if backend.is_none()
            && let Err(error) = self
                .start_managed_backend_with_bin(&settings, managed_codex_bin)
                .await
        {
            return self
                .mark_needs_attention(&mut attempt, error.to_string())
                .await;
        }

        let info = match self.wait_until_ready(managed_codex_bin).await {
            Ok(info) => info,
            Err(error) => {
                return self
                    .mark_needs_attention(&mut attempt, error.to_string())
                    .await;
            }
        };
        self.recover_attempt(attempt, managed_codex_bin, info, resolution)
            .await
    }

    pub(crate) async fn apply_status(&self) -> Result<ApplyOutput> {
        let Some(attempt) = ApplyAttemptReceipt::load(&self.apply_receipt_file).await? else {
            return Err(anyhow!("no app-server handoff receipt has been recorded"));
        };
        let app_server_version = client::probe(&self.socket_path)
            .await
            .ok()
            .map(|info| info.app_server_version);
        let running_managed_codex_version = self.running_managed_codex_version_best_effort().await;
        if attempt.is_resolved() || app_server_version.is_none() {
            return Ok(attempt.output_with_running_version(
                &self.socket_path,
                app_server_version,
                /*error*/ None,
                running_managed_codex_version,
            ));
        }
        let fallback = attempt.clone();
        match self
            .request_handoff(STATUS_METHOD, &attempt.handoff.handoff_id, None)
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
                Ok(view.output_with_running_version(
                    &self.socket_path,
                    app_server_version,
                    /*error*/ None,
                    running_managed_codex_version,
                ))
            }
            Err(error) => Ok(fallback.output_with_running_version(
                &self.socket_path,
                app_server_version,
                Some(sanitize_failure(&error.to_string())),
                running_managed_codex_version,
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
            return Err(anyhow!(
                "app server is not running; start it before applying an update"
            ));
        };

        let message = client::request(&self.socket_path, PREPARE_METHOD, None).await?;
        let handoff = match parse_handoff_response(message, PREPARE_METHOD) {
            Ok(receipt) => receipt,
            Err(error) => {
                let failure = sanitize_failure(&error.to_string());
                if let Some(receipt) = error.receipt {
                    let mut attempt = ApplyAttemptReceipt::new(
                        receipt,
                        managed_codex_bin.to_path_buf(),
                        self.managed_codex_version_best_effort(managed_codex_bin)
                            .await,
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
                self.managed_codex_version_best_effort(managed_codex_bin)
                    .await,
            );
            return self
                .mark_needs_attention(&mut attempt, sanitize_failure(&error.to_string()))
                .await;
        }

        let mut attempt = ApplyAttemptReceipt::new(
            handoff,
            managed_codex_bin.to_path_buf(),
            self.managed_codex_version_best_effort(managed_codex_bin)
                .await,
        );
        attempt.save(&self.apply_receipt_file).await?;
        if let Err(error) =
            stop_backend_with_receipt(&mut attempt, &self.apply_receipt_file, backend.stop()).await
        {
            return self
                .mark_needs_attention(&mut attempt, error.to_string())
                .await;
        }
        if let Err(error) = self
            .start_managed_backend_with_bin(&settings, managed_codex_bin)
            .await
        {
            return self
                .mark_needs_attention(&mut attempt, error.to_string())
                .await;
        }
        let info = match self.wait_until_ready(managed_codex_bin).await {
            Ok(info) => info,
            Err(error) => {
                return self
                    .mark_needs_attention(&mut attempt, error.to_string())
                    .await;
            }
        };
        self.recover_attempt(attempt, managed_codex_bin, info, None)
            .await
    }

    async fn recover_attempt(
        &self,
        mut attempt: ApplyAttemptReceipt,
        managed_codex_bin: &Path,
        info: client::ProbeInfo,
        resolution: Option<&str>,
    ) -> Result<ApplyOutput> {
        attempt.phase = ApplyPhase::Recovering;
        attempt.save(&self.apply_receipt_file).await?;
        match self
            .request_handoff(STATUS_METHOD, &attempt.handoff.handoff_id, None)
            .await
        {
            Ok(status) => {
                attempt.handoff = status;
                if attempt.handoff.state == "completed" {
                    attempt.phase = ApplyPhase::Applied;
                    attempt.failure = None;
                    attempt.save(&self.apply_receipt_file).await?;
                    return Ok(attempt.output_with_running_version(
                        &self.socket_path,
                        Some(info.app_server_version.clone()),
                        /*error*/ None,
                        self.running_managed_codex_version_best_effort().await,
                    ));
                }
                if attempt.handoff.state == "needsAttention" && resolution.is_none() {
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
                ) && !(resolution.is_some() && attempt.handoff.state == "needsAttention")
                {
                    return self
                        .mark_needs_attention(
                            &mut attempt,
                            "coordinator returned an unknown handoff state",
                        )
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
            .request_handoff(RECOVER_METHOD, &attempt.handoff.handoff_id, resolution)
            .await
        {
            Ok(receipt) if receipt.state == "completed" => {
                attempt.handoff = receipt;
                attempt.phase = ApplyPhase::Applied;
                attempt.failure = None;
                attempt.managed_codex_version = self
                    .managed_codex_version_best_effort(managed_codex_bin)
                    .await;
                attempt.save(&self.apply_receipt_file).await?;
                Ok(attempt.output_with_running_version(
                    &self.socket_path,
                    Some(info.app_server_version),
                    /*error*/ None,
                    self.running_managed_codex_version_best_effort().await,
                ))
            }
            Ok(receipt) if receipt.quarantined => {
                attempt.handoff = receipt;
                attempt.phase = ApplyPhase::NeedsAttention;
                attempt.save(&self.apply_receipt_file).await?;
                Ok(attempt.output_with_running_version(
                    &self.socket_path,
                    Some(info.app_server_version),
                    /*error*/ None,
                    self.running_managed_codex_version_best_effort().await,
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
        resolution: Option<&str>,
    ) -> std::result::Result<HandoffReceipt, HandoffRpcError> {
        let params = match resolution {
            Some(resolution) => serde_json::json!({
                "handoffId": handoff_id,
                "resolution": resolution,
            }),
            None => serde_json::json!({ "handoffId": handoff_id }),
        };
        let message = client::request(&self.socket_path, method, Some(params))
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
        Ok(attempt.output_with_running_version(
            &self.socket_path,
            client::probe(&self.socket_path)
                .await
                .ok()
                .map(|info| info.app_server_version),
            Some(failure),
            self.running_managed_codex_version_best_effort().await,
        ))
    }
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
