//! Safe app-server daemon upgrades coordinated through the handoff journal.

use std::future::Future;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;

use crate::ApplyOptions;
use crate::Daemon;
use crate::apply_receipt::ApplyAttemptReceipt;
use crate::apply_receipt::ApplyFailureKind;
use crate::apply_receipt::ApplyOutput;
use crate::apply_receipt::ApplyPhase;
use crate::apply_receipt::ApplyStatus;
use crate::apply_receipt::HandoffReceipt;
use crate::apply_receipt::HandoffResolution;
use crate::apply_receipt::ensure_handoff_id;
use crate::apply_receipt::ensure_transferable_handoff;
use crate::apply_receipt::parse_handoff_response;
use crate::apply_receipt::sanitize_failure;
use crate::client;
use crate::settings::DaemonSettings;

const PREPARE_METHOD: &str = "thread/handoff/prepare";
const STATUS_METHOD: &str = "thread/handoff/status";
const RECOVER_METHOD: &str = "thread/handoff/recover";

#[path = "apply/reconcile.rs"]
mod reconcile;
#[path = "apply/target.rs"]
mod target;

async fn canonical_home(path: &Path) -> Result<PathBuf> {
    tokio::fs::canonicalize(path)
        .await
        .with_context(|| format!("failed to resolve Codex home {}", path.display()))
}

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

fn launcher_update_required(
    managed_codex_bin: &Path,
    managed_backend_is_running: bool,
    running_managed_codex_version: Option<String>,
    managed_codex_version: Option<String>,
) -> Result<bool> {
    if !managed_backend_is_running {
        return Ok(false);
    }
    let running_version = running_managed_codex_version.ok_or_else(|| {
        anyhow!(
            "cannot safely verify selected launcher {} because the managed process has no full Codex launch identity; app-server version alone does not identify the fork build",
            managed_codex_bin.display()
        )
    })?;
    let managed_codex_version = managed_codex_version.ok_or_else(|| {
        anyhow!(
            "cannot safely reconcile selected launcher {} because its installed version is unavailable",
            managed_codex_bin.display()
        )
    })?;
    Ok(running_version != managed_codex_version)
}

impl Daemon {
    pub(crate) async fn apply(&self, options: ApplyOptions) -> Result<ApplyOutput> {
        let _operation_lock = self.acquire_operation_lock().await?;
        if let Some(managed_codex_path) = options.managed_codex_path {
            return self.apply_to_target(managed_codex_path).await;
        }
        let settings = self.load_settings().await?;
        ensure_apply_launcher(&settings)?;
        let managed_codex_bin = self.configured_managed_codex_bin(&settings).to_path_buf();
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
        self.apply_fresh(settings, &managed_codex_bin).await
    }

    pub(crate) async fn recover(&self) -> Result<ApplyOutput> {
        self.recover_with_resolution(None).await
    }

    pub(crate) async fn quarantine(&self) -> Result<ApplyOutput> {
        self.recover_with_resolution(Some("quarantine")).await
    }

    async fn recover_with_resolution(&self, resolution: Option<&str>) -> Result<ApplyOutput> {
        let _operation_lock = self.acquire_operation_lock().await?;
        self.recover_with_resolution_locked(resolution, None).await
    }

    async fn recover_with_resolution_locked(
        &self,
        resolution: Option<&str>,
        launcher_override: Option<&Path>,
    ) -> Result<ApplyOutput> {
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
        if resolution.is_none()
            && !attempt.blocks_new_apply()
            && !attempt.can_reconcile_empty_orphan()
        {
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
                    if let Err(launcher_error) = ensure_apply_launcher(&settings) {
                        return self
                            .mark_needs_attention(
                                &mut attempt,
                                format!(
                                    "cannot quarantine without a reachable app server: {error}; \
                                     configured local launcher is unavailable: {launcher_error}"
                                ),
                            )
                            .await;
                    }
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

        let mut settings = self.load_settings().await?;
        ensure_apply_launcher(&settings)?;
        if let Some(launcher_override) = launcher_override {
            self.ensure_managed_codex_bin(launcher_override)?;
        }
        let configured_managed_codex_bin = self.configured_managed_codex_bin(&settings);
        if launcher_override.is_none() && configured_managed_codex_bin != attempt.managed_codex_path
        {
            let failure = format!(
                "configured Codex launcher changed from {} to {}; refusing recovery",
                attempt.managed_codex_path.display(),
                configured_managed_codex_bin.display()
            );
            return self.mark_needs_attention(&mut attempt, failure).await;
        }
        if launcher_override.is_none() {
            self.ensure_managed_codex_bin(configured_managed_codex_bin)?;
        }

        let mut backend = self.running_backend_instance(&settings).await?;
        if let Some(launcher_override) = launcher_override {
            let server_home = if backend.is_some() {
                match client::probe(&self.socket_path).await {
                    Ok(info) => info.codex_home,
                    Err(error) => {
                        return self
                            .mark_needs_attention_kind(
                                &mut attempt,
                                ApplyFailureKind::HandoffWorkPending,
                                format!(
                                    "cannot verify the current app server before changing its selected launcher: {error}"
                                ),
                            )
                            .await;
                    }
                }
            } else {
                self.codex_home()?
            };
            if let Err(error) = self
                .persist_recovery_target_after_server_home_check(
                    &mut settings,
                    &attempt,
                    launcher_override,
                    &server_home,
                )
                .await
            {
                return self
                    .mark_needs_attention_kind(
                        &mut attempt,
                        ApplyFailureKind::HandoffStorageMismatch,
                        error,
                    )
                    .await;
            }
        }
        let managed_codex_bin = self.configured_managed_codex_bin(&settings);
        self.ensure_managed_codex_bin(managed_codex_bin)?;

        if resolution.is_none() && attempt.stop_completed != Some(true) {
            if let Some(backend_instance) = backend.as_ref() {
                let current_server = match client::probe(&self.socket_path).await {
                    Ok(info) => info,
                    Err(error) => {
                        return self
                            .mark_needs_attention_kind(
                                &mut attempt,
                                ApplyFailureKind::HandoffWorkPending,
                                format!(
                                    "cannot verify the current app server before stopping it: {error}"
                                ),
                            )
                            .await;
                    }
                };
                if let Err(error) = self
                    .validate_server_home(Some(&attempt), &current_server.codex_home)
                    .await
                {
                    return self
                        .mark_needs_attention_kind(
                            &mut attempt,
                            ApplyFailureKind::HandoffStorageMismatch,
                            error,
                        )
                        .await;
                }
                let prepared = match self
                    .request_handoff(STATUS_METHOD, &attempt.handoff.handoff_id, None)
                    .await
                {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        let mut failure = error.to_string();
                        let failure_kind = if let Some(server_home) =
                            error.server_codex_home.as_deref()
                            && let Err(home_error) =
                                self.validate_server_home(Some(&attempt), server_home).await
                        {
                            return self
                                .mark_needs_attention_kind(
                                    &mut attempt,
                                    ApplyFailureKind::HandoffStorageMismatch,
                                    home_error,
                                )
                                .await;
                        } else if error.is_unknown_handoff() {
                            ApplyFailureKind::HandoffJournalMissing
                        } else {
                            ApplyFailureKind::HandoffWorkPending
                        };
                        match error.receipt {
                            Some(receipt)
                                if ensure_handoff_id(&attempt.handoff.handoff_id, &receipt)
                                    .is_ok() =>
                            {
                                attempt.handoff = receipt;
                            }
                            Some(_) => {
                                failure.push_str(
                                    "; app server returned a receipt for a different handoff",
                                );
                            }
                            None => {}
                        }
                        return self
                            .mark_needs_attention_kind(
                                &mut attempt,
                                failure_kind,
                                format!(
                                    "prepared handoff could not be verified before stopping the daemon: {failure}"
                                ),
                            )
                            .await;
                    }
                };
                if let Err(error) = self
                    .validate_server_home(Some(&attempt), &prepared.server_codex_home)
                    .await
                {
                    return self
                        .mark_needs_attention_kind(
                            &mut attempt,
                            ApplyFailureKind::HandoffStorageMismatch,
                            error,
                        )
                        .await;
                }
                if let Err(error) =
                    ensure_handoff_id(&attempt.handoff.handoff_id, &prepared.receipt)
                {
                    return self
                        .mark_needs_attention_kind(
                            &mut attempt,
                            ApplyFailureKind::HandoffWorkPending,
                            error,
                        )
                        .await;
                }
                if let Err(error) = ensure_transferable_handoff(&prepared.receipt) {
                    return self
                        .mark_needs_attention_kind(
                            &mut attempt,
                            ApplyFailureKind::HandoffWorkPending,
                            format!(
                                "prepared journal is no longer transferable before daemon stop: {error}"
                            ),
                        )
                        .await;
                }
                attempt.handoff = prepared.receipt;
                attempt.origin_codex_home = Some(prepared.server_codex_home);
                attempt.save(&self.apply_receipt_file).await?;
                if let Err(error) = stop_backend_with_receipt(
                    &mut attempt,
                    &self.apply_receipt_file,
                    backend_instance.stop_with_grace(settings.shutdown_grace_seconds),
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
        if attempt.is_resolved() {
            let mut output = attempt.output_with_running_version(
                &self.socket_path,
                app_server_version,
                /*error*/ None,
                running_managed_codex_version,
            );
            self.verify_applied_target(&mut output, &attempt.managed_codex_path)
                .await?;
            return Ok(output);
        }
        if app_server_version.is_none() {
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
                if let Err(error) = self
                    .validate_server_home(Some(&attempt), &status.server_codex_home)
                    .await
                {
                    let mut output = fallback.output_with_running_version(
                        &self.socket_path,
                        app_server_version,
                        Some(sanitize_failure(&error.to_string())),
                        running_managed_codex_version,
                    );
                    output.failure_kind = Some(ApplyFailureKind::HandoffStorageMismatch);
                    return Ok(output);
                }
                if let Err(error) = ensure_handoff_id(&attempt.handoff.handoff_id, &status.receipt)
                {
                    let mut output = fallback.output_with_running_version(
                        &self.socket_path,
                        app_server_version,
                        Some(sanitize_failure(&error.to_string())),
                        running_managed_codex_version,
                    );
                    output.failure_kind = Some(ApplyFailureKind::HandoffWorkPending);
                    return Ok(output);
                }
                let mut view = attempt;
                view.handoff = status.receipt;
                view.phase = match view.handoff.state.as_str() {
                    "completed" => ApplyPhase::Applied,
                    "needsAttention" => ApplyPhase::NeedsAttention,
                    "prepared" => ApplyPhase::Prepared,
                    "draining" | "suspended" | "restoring" => ApplyPhase::Recovering,
                    _ => ApplyPhase::NeedsAttention,
                };
                let mut output = view.output_with_running_version(
                    &self.socket_path,
                    app_server_version,
                    /*error*/ None,
                    running_managed_codex_version,
                );
                self.verify_applied_target(&mut output, &view.managed_codex_path)
                    .await?;
                Ok(output)
            }
            Err(error) => {
                let mut output = fallback.output_with_running_version(
                    &self.socket_path,
                    app_server_version,
                    Some(sanitize_failure(&error.to_string())),
                    running_managed_codex_version,
                );
                if let Some(server_home) = error.server_codex_home.as_deref()
                    && let Err(home_error) =
                        self.validate_server_home(Some(&attempt), server_home).await
                {
                    output.error = Some(sanitize_failure(&home_error.to_string()));
                    output.failure_kind = Some(ApplyFailureKind::HandoffStorageMismatch);
                } else if error.is_unknown_handoff() {
                    output.failure_kind = Some(ApplyFailureKind::HandoffJournalMissing);
                }
                Ok(output)
            }
        }
    }

    async fn apply_fresh(
        &self,
        settings: DaemonSettings,
        managed_codex_bin: &Path,
    ) -> Result<ApplyOutput> {
        self.apply_fresh_with_resolution(settings, managed_codex_bin, Vec::new())
            .await
    }

    async fn apply_fresh_with_resolution(
        &self,
        settings: DaemonSettings,
        managed_codex_bin: &Path,
        handoff_resolutions: Vec<HandoffResolution>,
    ) -> Result<ApplyOutput> {
        let latest_resolution = handoff_resolutions.last().cloned();
        ensure_apply_launcher(&settings)?;
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

        let expected_codex_home = self.codex_home()?;
        let response = client::request_in_codex_home(
            &self.socket_path,
            PREPARE_METHOD,
            None,
            &expected_codex_home,
        )
        .await?;
        let server_home = response.codex_home.clone();
        let handoff = match parse_handoff_response(response.message, PREPARE_METHOD) {
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
                    attempt.origin_codex_home =
                        Some(self.validate_server_home(None, &server_home).await?);
                    attempt.handoff_resolution = latest_resolution.clone();
                    attempt.handoff_resolutions = handoff_resolutions.clone();
                    return self
                        .mark_needs_attention_kind(
                            &mut attempt,
                            ApplyFailureKind::HandoffWorkPending,
                            failure,
                        )
                        .await;
                }
                return Err(anyhow!(failure));
            }
        };
        let origin_codex_home = match self.validate_server_home(None, &server_home).await {
            Ok(home) => home,
            Err(error) => {
                let mut attempt = ApplyAttemptReceipt::new(
                    handoff,
                    managed_codex_bin.to_path_buf(),
                    self.managed_codex_version_best_effort(managed_codex_bin)
                        .await,
                );
                attempt.failure_kind = Some(ApplyFailureKind::HandoffStorageMismatch);
                attempt.handoff_resolution = latest_resolution.clone();
                attempt.handoff_resolutions = handoff_resolutions.clone();
                return self.mark_needs_attention(&mut attempt, error).await;
            }
        };
        if let Err(error) = ensure_transferable_handoff(&handoff) {
            let mut attempt = ApplyAttemptReceipt::new(
                handoff,
                managed_codex_bin.to_path_buf(),
                self.managed_codex_version_best_effort(managed_codex_bin)
                    .await,
            );
            attempt.origin_codex_home = Some(origin_codex_home);
            attempt.handoff_resolution = latest_resolution.clone();
            attempt.handoff_resolutions = handoff_resolutions.clone();
            return self
                .mark_needs_attention_kind(
                    &mut attempt,
                    ApplyFailureKind::HandoffWorkPending,
                    sanitize_failure(&error.to_string()),
                )
                .await;
        }

        let mut attempt = ApplyAttemptReceipt::new(
            handoff,
            managed_codex_bin.to_path_buf(),
            self.managed_codex_version_best_effort(managed_codex_bin)
                .await,
        );
        attempt.origin_codex_home = Some(origin_codex_home);
        attempt.handoff_resolution = latest_resolution;
        attempt.handoff_resolutions = handoff_resolutions;
        attempt.save(&self.apply_receipt_file).await?;
        let prepared = match self
            .request_handoff(STATUS_METHOD, &attempt.handoff.handoff_id, None)
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(server_home) = error.server_codex_home.as_deref()
                    && let Err(home_error) =
                        self.validate_server_home(Some(&attempt), server_home).await
                {
                    return self
                        .mark_needs_attention_kind(
                            &mut attempt,
                            ApplyFailureKind::HandoffStorageMismatch,
                            home_error,
                        )
                        .await;
                }
                return self
                    .mark_needs_attention_kind(
                        &mut attempt,
                        ApplyFailureKind::HandoffWorkPending,
                        format!(
                            "prepared handoff was not readable before stopping the daemon: {error}"
                        ),
                    )
                    .await;
            }
        };
        if let Err(error) = self
            .validate_server_home(Some(&attempt), &prepared.server_codex_home)
            .await
        {
            return self
                .mark_needs_attention_kind(
                    &mut attempt,
                    ApplyFailureKind::HandoffStorageMismatch,
                    error,
                )
                .await;
        }
        if let Err(error) = ensure_handoff_id(&attempt.handoff.handoff_id, &prepared.receipt) {
            return self
                .mark_needs_attention_kind(
                    &mut attempt,
                    ApplyFailureKind::HandoffWorkPending,
                    error,
                )
                .await;
        }
        if let Err(error) = ensure_transferable_handoff(&prepared.receipt) {
            return self
                .mark_needs_attention_kind(
                    &mut attempt,
                    ApplyFailureKind::HandoffWorkPending,
                    format!("prepared journal was not transferable before daemon stop: {error}"),
                )
                .await;
        }
        attempt.handoff = prepared.receipt;
        attempt.save(&self.apply_receipt_file).await?;
        if let Err(error) = stop_backend_with_receipt(
            &mut attempt,
            &self.apply_receipt_file,
            backend.stop_with_grace(settings.shutdown_grace_seconds),
        )
        .await
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
        let mut output = self
            .recover_attempt(attempt, managed_codex_bin, info, None)
            .await?;
        self.verify_applied_target(&mut output, managed_codex_bin)
            .await?;
        Ok(output)
    }

    async fn mark_needs_attention(
        &self,
        attempt: &mut ApplyAttemptReceipt,
        error: impl std::fmt::Display,
    ) -> Result<ApplyOutput> {
        self.mark_needs_attention_kind(attempt, ApplyFailureKind::HandoffWorkPending, error)
            .await
    }

    async fn mark_needs_attention_kind(
        &self,
        attempt: &mut ApplyAttemptReceipt,
        failure_kind: ApplyFailureKind,
        error: impl std::fmt::Display,
    ) -> Result<ApplyOutput> {
        let failure = sanitize_failure(&error.to_string());
        attempt.phase = ApplyPhase::NeedsAttention;
        attempt.failure = Some(failure.clone());
        attempt.failure_kind = Some(failure_kind);
        attempt.save(&self.apply_receipt_file).await?;
        let mut output = attempt.output_with_running_version(
            &self.socket_path,
            client::probe(&self.socket_path)
                .await
                .ok()
                .map(|info| info.app_server_version),
            Some(failure),
            self.running_managed_codex_version_best_effort().await,
        );
        output.failure_kind = Some(failure_kind);
        Ok(output)
    }
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
