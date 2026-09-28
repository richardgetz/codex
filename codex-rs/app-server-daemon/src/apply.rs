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
use crate::apply_receipt::ApplyOutput;
use crate::apply_receipt::ApplyPhase;
use crate::apply_receipt::ApplyStatus;
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

fn launcher_update_required(
    managed_codex_bin: &Path,
    managed_backend_is_running: bool,
    running_managed_codex_version: Option<String>,
    running_app_server_version: Option<String>,
    managed_codex_version: Option<String>,
) -> Result<bool> {
    if !managed_backend_is_running {
        return Ok(false);
    }
    let running_version = running_managed_codex_version
        .or(running_app_server_version)
        .ok_or_else(|| {
            anyhow!(
                "cannot safely reconcile selected launcher {} because the running daemon version is unavailable",
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

    async fn apply_to_target(&self, target: PathBuf) -> Result<ApplyOutput> {
        anyhow::ensure!(
            target.is_absolute(),
            "selected Codex launcher path must be absolute: {}",
            target.display()
        );
        let mut settings = self.load_settings().await?;
        let receipt = ApplyAttemptReceipt::load(&self.apply_receipt_file).await?;
        if settings.managed_codex_path.is_none() {
            if let Some(receipt) = receipt.as_ref().filter(|receipt| !receipt.is_resolved()) {
                let error = format!(
                    "cannot select launcher {} while handoff {} is unresolved and no managed launcher is configured; recovery is pinned to {}",
                    target.display(),
                    receipt.handoff.handoff_id,
                    receipt.managed_codex_path.display()
                );
                let mut output = receipt.output_with_running_version(
                    &self.socket_path,
                    client::probe(&self.socket_path)
                        .await
                        .ok()
                        .map(|info| info.app_server_version),
                    Some(error),
                    self.running_managed_codex_version_best_effort().await,
                );
                output.status = ApplyStatus::NeedsAttention;
                return Ok(output);
            }
            return Ok(ApplyOutput::without_handoff(
                ApplyStatus::NotConfigured,
                None,
                None,
                &self.socket_path,
                None,
            ));
        }
        self.ensure_managed_codex_bin(&target)?;

        if let Some(receipt) = receipt.filter(|receipt| !receipt.is_resolved()) {
            if receipt.managed_codex_path != target {
                let error = format!(
                    "cannot select launcher {} while handoff {} is unresolved; recovery is pinned to {}",
                    target.display(),
                    receipt.handoff.handoff_id,
                    receipt.managed_codex_path.display()
                );
                let mut output = receipt.output_with_running_version(
                    &self.socket_path,
                    client::probe(&self.socket_path)
                        .await
                        .ok()
                        .map(|info| info.app_server_version),
                    Some(error),
                    self.running_managed_codex_version_best_effort().await,
                );
                output.status = ApplyStatus::NeedsAttention;
                return Ok(output);
            }
            if receipt.blocks_new_apply() {
                if settings.managed_codex_path.as_deref() != Some(target.as_path()) {
                    settings.managed_codex_path = Some(target);
                    settings.save(&self.settings_file).await?;
                }
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
        }

        if self.running_backend_instance(&settings).await?.is_none() {
            if client::probe(&self.socket_path).await.is_ok() {
                return Err(anyhow!(
                    "app server is running but is not managed by codex app-server daemon"
                ));
            }
            settings.managed_codex_path = Some(target.clone());
            settings.save(&self.settings_file).await?;
            return Ok(ApplyOutput::without_handoff(
                ApplyStatus::Deferred,
                Some(target.clone()),
                self.managed_codex_version_best_effort(&target).await,
                &self.socket_path,
                None,
            ));
        }

        settings.managed_codex_path = Some(target.clone());
        // Persist the selected launcher before the handoff begins. A pre-transfer blocker
        // leaves the current process alive, while later reloads continue to select this target.
        settings.save(&self.settings_file).await?;
        self.apply_fresh(settings, &target).await
    }

    pub(crate) async fn recover(&self) -> Result<ApplyOutput> {
        self.recover_with_resolution(None).await
    }

    pub(crate) async fn quarantine(&self) -> Result<ApplyOutput> {
        self.recover_with_resolution(Some("quarantine")).await
    }

    async fn recover_with_resolution(&self, resolution: Option<&str>) -> Result<ApplyOutput> {
        let _operation_lock = self.acquire_operation_lock().await?;
        self.recover_with_resolution_locked(resolution).await
    }

    async fn recover_with_resolution_locked(
        &self,
        resolution: Option<&str>,
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

    pub(crate) async fn reconcile_launcher_update(&self) -> Result<Option<ApplyOutput>> {
        let _operation_lock = self.acquire_operation_lock().await?;
        let settings = self.load_settings().await?;
        let Some(managed_codex_bin) = settings.managed_codex_path.clone() else {
            return Ok(None);
        };
        self.ensure_managed_codex_bin(&managed_codex_bin)?;

        if let Some(attempt) = ApplyAttemptReceipt::load(&self.apply_receipt_file).await?
            && !attempt.is_resolved()
        {
            if attempt.phase == ApplyPhase::NeedsAttention && !attempt.blocks_new_apply() {
                return self
                    .apply_fresh(settings, &managed_codex_bin)
                    .await
                    .map(Some);
            }
            if attempt.blocks_new_apply() {
                let output = self.recover_with_resolution_locked(None).await?;
                if output.status != ApplyStatus::Applied {
                    return Ok(Some(output));
                }
            }
        }

        let running_managed_codex_version = self.running_managed_codex_version_best_effort().await;
        let managed_backend_is_running = self.running_backend_instance(&settings).await?.is_some();
        let running_app_server_version = if managed_backend_is_running
            && running_managed_codex_version.is_none()
        {
            Some(
                client::probe(&self.socket_path)
                    .await
                    .with_context(|| {
                        format!(
                            "cannot safely compare selected launcher {} because its active daemon PID record has no launch version and the app-server version probe failed",
                            managed_codex_bin.display()
                        )
                    })?
                    .app_server_version,
            )
        } else {
            None
        };
        let installed_version = self
            .managed_codex_version_best_effort(&managed_codex_bin)
            .await;
        if !launcher_update_required(
            &managed_codex_bin,
            managed_backend_is_running,
            running_managed_codex_version,
            running_app_server_version,
            installed_version,
        )? {
            return Ok(None);
        }
        self.apply_fresh(settings, &managed_codex_bin)
            .await
            .map(Some)
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

    async fn apply_fresh(
        &self,
        settings: DaemonSettings,
        managed_codex_bin: &Path,
    ) -> Result<ApplyOutput> {
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
                if attempt.handoff.state == "needsAttention"
                    && resolution.is_none()
                    && !attempt.can_reconcile_empty_orphan()
                {
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
                ) && !(attempt.can_reconcile_empty_orphan()
                    || (resolution.is_some() && attempt.handoff.state == "needsAttention"))
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
                if error.is_unknown_handoff() && attempt.can_reconcile_empty_orphan() {
                    return self
                        .mark_empty_orphan_applied(&mut attempt, managed_codex_bin, &info)
                        .await;
                }
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
                if error.is_unknown_handoff() && attempt.can_reconcile_empty_orphan() {
                    return self
                        .mark_empty_orphan_applied(&mut attempt, managed_codex_bin, &info)
                        .await;
                }
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

    async fn mark_empty_orphan_applied(
        &self,
        attempt: &mut ApplyAttemptReceipt,
        managed_codex_bin: &Path,
        info: &client::ProbeInfo,
    ) -> Result<ApplyOutput> {
        attempt.handoff.state = "completed".to_string();
        attempt.phase = ApplyPhase::Applied;
        attempt.failure = None;
        attempt.managed_codex_version = self
            .managed_codex_version_best_effort(managed_codex_bin)
            .await;
        attempt.save(&self.apply_receipt_file).await?;
        Ok(attempt.output_with_running_version(
            &self.socket_path,
            Some(info.app_server_version.clone()),
            /*error*/ None,
            self.running_managed_codex_version_best_effort().await,
        ))
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
