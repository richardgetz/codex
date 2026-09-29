//! Target-aware launcher selection and automatic apply reconciliation.

use super::*;
use crate::apply_archive::ApplyArchiveOutcome;
use crate::apply_archive::archive_and_remove;
use std::ffi::OsStr;

impl Daemon {
    pub(super) async fn apply_to_target(&self, target: PathBuf) -> Result<ApplyOutput> {
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
                output.failure_kind = Some(ApplyFailureKind::HandoffWorkPending);
                return Ok(output);
            }
            let has_daemon_configuration = tokio::fs::try_exists(&self.settings_file)
                .await
                .context("failed to check app-server daemon settings")?
                || receipt.is_some()
                || self.running_backend_instance(&settings).await?.is_some();
            if !has_daemon_configuration {
                if client::probe(&self.socket_path).await.is_ok() {
                    return Err(anyhow!(
                        "app server is running but is not managed by codex app-server daemon"
                    ));
                }
                return Ok(ApplyOutput::without_handoff(
                    ApplyStatus::NotConfigured,
                    None,
                    None,
                    &self.socket_path,
                    None,
                ));
            }
        }
        self.ensure_managed_codex_bin(&target)?;

        if let Some(attempt) = receipt.as_ref().filter(|receipt| receipt.is_resolved())
            && self.running_target_matches(&target).await
        {
            let probe = self
                .persist_target_after_server_home_check(&mut settings, &target)
                .await?;
            let mut output = attempt.output_with_running_version(
                &self.socket_path,
                Some(probe.app_server_version),
                /*error*/ None,
                self.running_managed_codex_version_best_effort().await,
            );
            self.verify_applied_target(&mut output, &target).await?;
            return Ok(output);
        }

        let mut handoff_resolutions = receipt
            .as_ref()
            .map(ApplyAttemptReceipt::handoff_resolution_history)
            .unwrap_or_default();
        if let Some(attempt) = receipt.as_ref().filter(|receipt| !receipt.is_resolved()) {
            if attempt.can_retry_preparation()
                || (attempt.handoff.quarantined && !attempt.blocks_new_apply())
            {
                let archive_outcome = if attempt.handoff.quarantined {
                    ApplyArchiveOutcome::Quarantined
                } else {
                    ApplyArchiveOutcome::RetriedPreparation
                };
                archive_and_remove(
                    &self.apply_history_dir()?,
                    &self.apply_receipt_file,
                    attempt,
                    archive_outcome,
                )
                .await?;
            } else {
                let mut output = self
                    .recover_with_resolution_locked(None, Some(&attempt.managed_codex_path))
                    .await?;
                if output.status != ApplyStatus::Applied {
                    if attempt.managed_codex_path != target {
                        let recovery_error = output
                            .error
                            .take()
                            .unwrap_or_else(|| "the saved handoff is still unresolved".to_string());
                        output.error = Some(format!(
                            "selected launcher {} is waiting for handoff {} recovery on its saved launcher {}: {recovery_error}",
                            target.display(),
                            attempt.handoff.handoff_id,
                            attempt.managed_codex_path.display()
                        ));
                    }
                    return Ok(output);
                }
                handoff_resolutions = output.handoff_resolutions.clone();
                if self.running_target_matches(&target).await {
                    self.verify_applied_target(&mut output, &target).await?;
                    return Ok(output);
                }
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
            let mut output = ApplyOutput::without_handoff(
                ApplyStatus::Deferred,
                Some(target.clone()),
                self.managed_codex_version_best_effort(&target).await,
                &self.socket_path,
                None,
            );
            output.handoff_resolution = handoff_resolutions.last().cloned();
            output.handoff_resolutions = handoff_resolutions;
            return Ok(output);
        }

        let probe = self
            .persist_target_after_server_home_check(&mut settings, &target)
            .await?;

        if self.running_target_matches(&target).await {
            let mut output = ApplyOutput::without_handoff(
                ApplyStatus::Applied,
                Some(target.clone()),
                self.managed_codex_version_best_effort(&target).await,
                &self.socket_path,
                None,
            );
            output.running_managed_codex_version =
                self.running_managed_codex_version_best_effort().await;
            output.app_server_version = Some(probe.app_server_version);
            output.handoff_resolution = handoff_resolutions.last().cloned();
            output.handoff_resolutions = handoff_resolutions.clone();
            return Ok(output);
        }

        let mut output = self
            .apply_fresh_with_resolution(settings, &target, handoff_resolutions)
            .await?;
        self.verify_applied_target(&mut output, &target).await?;
        Ok(output)
    }

    pub(super) fn apply_history_dir(&self) -> Result<PathBuf> {
        Ok(self
            .apply_receipt_file
            .parent()
            .context("apply receipt path has no parent directory")?
            .join("apply-history"))
    }

    pub(super) fn codex_home(&self) -> Result<PathBuf> {
        let state_dir = self
            .settings_file
            .parent()
            .context("daemon settings path has no parent directory")?;
        if state_dir.file_name() == Some(OsStr::new(crate::STATE_DIR_NAME)) {
            return state_dir
                .parent()
                .map(Path::to_path_buf)
                .context("daemon state directory has no Codex home");
        }
        Ok(state_dir.to_path_buf())
    }

    pub(super) async fn validate_server_home(
        &self,
        attempt: Option<&ApplyAttemptReceipt>,
        server_codex_home: &Path,
    ) -> Result<PathBuf> {
        let expected_home = canonical_home(&self.codex_home()?).await?;
        let server_home = canonical_home(server_codex_home).await?;
        anyhow::ensure!(
            server_home == expected_home,
            "app server Codex home {} does not match daemon home {}; refusing handoff mutation",
            server_home.display(),
            expected_home.display()
        );
        if let Some(attempt) = attempt
            && let Some(origin_home) = attempt.origin_codex_home.as_deref()
        {
            let origin_home = canonical_home(origin_home).await?;
            anyhow::ensure!(
                origin_home == expected_home,
                "handoff {} originated in Codex home {}, not current daemon home {}; refusing recovery",
                attempt.handoff.handoff_id,
                origin_home.display(),
                expected_home.display()
            );
        }
        Ok(server_home)
    }

    pub(super) async fn persist_target_after_server_home_check(
        &self,
        settings: &mut DaemonSettings,
        target: &Path,
    ) -> Result<client::ProbeInfo> {
        let probe = client::probe(&self.socket_path).await?;
        self.validate_server_home(None, &probe.codex_home).await?;
        if settings.managed_codex_path.as_deref() != Some(target) {
            settings.managed_codex_path = Some(target.to_path_buf());
            settings.save(&self.settings_file).await?;
        }
        Ok(probe)
    }

    pub(super) async fn persist_recovery_target_after_server_home_check(
        &self,
        settings: &mut DaemonSettings,
        attempt: &ApplyAttemptReceipt,
        target: &Path,
        server_codex_home: &Path,
    ) -> Result<()> {
        self.validate_server_home(Some(attempt), server_codex_home)
            .await?;
        if settings.managed_codex_path.as_deref() != Some(target) {
            settings.managed_codex_path = Some(target.to_path_buf());
            settings.save(&self.settings_file).await?;
        }
        Ok(())
    }

    async fn running_target_matches(&self, target: &Path) -> bool {
        let Some(installed) = self.managed_codex_version_best_effort(target).await else {
            return false;
        };
        let Some(running) = self.running_managed_codex_version_best_effort().await else {
            return false;
        };
        installed == running
    }

    pub(super) async fn verify_applied_target(
        &self,
        output: &mut ApplyOutput,
        target: &Path,
    ) -> Result<()> {
        let installed = self.managed_codex_version_best_effort(target).await;
        let running = self.running_managed_codex_version_best_effort().await;
        output.managed_codex_path = Some(target.to_path_buf());
        output.managed_codex_version = installed.clone();
        output.running_managed_codex_version = running.clone();
        if output.status == ApplyStatus::Applied && (installed.is_none() || installed != running) {
            output.status = ApplyStatus::NeedsAttention;
            output.failure_kind = Some(ApplyFailureKind::RunningLauncherMismatch);
            let error = format!(
                "selected launcher {} has installed version {}, but the running daemon reports {}; full fork versions must match",
                target.display(),
                installed.as_deref().unwrap_or("unknown"),
                running.as_deref().unwrap_or("unknown")
            );
            output.error = Some(error.clone());
            if let Some(mut attempt) = ApplyAttemptReceipt::load(&self.apply_receipt_file).await?
                && attempt.is_resolved()
                && attempt.managed_codex_path.as_path() == target
            {
                attempt.failure_kind = Some(ApplyFailureKind::RunningLauncherMismatch);
                attempt.failure = Some(error);
                attempt.save(&self.apply_receipt_file).await?;
            }
        } else if output.status == ApplyStatus::Applied
            && output.failure_kind == Some(ApplyFailureKind::RunningLauncherMismatch)
            && let Some(mut attempt) = ApplyAttemptReceipt::load(&self.apply_receipt_file).await?
            && attempt.is_resolved()
            && attempt.managed_codex_path.as_path() == target
            && attempt.failure_kind == Some(ApplyFailureKind::RunningLauncherMismatch)
        {
            attempt.failure_kind = None;
            attempt.failure = None;
            attempt.save(&self.apply_receipt_file).await?;
            output.failure_kind = None;
            output.error = None;
        }
        Ok(())
    }

    pub(crate) async fn reconcile_launcher_update(&self) -> Result<Option<ApplyOutput>> {
        let _operation_lock = self.acquire_operation_lock().await?;
        let settings = self.load_settings().await?;
        let Some(managed_codex_bin) = settings.managed_codex_path.clone() else {
            return Ok(None);
        };
        self.ensure_managed_codex_bin(&managed_codex_bin)?;

        let mut handoff_resolutions = Vec::new();
        if let Some(attempt) = ApplyAttemptReceipt::load(&self.apply_receipt_file).await?
            && !attempt.is_resolved()
        {
            handoff_resolutions = attempt.handoff_resolution_history();
            if attempt.can_retry_preparation()
                || (attempt.handoff.quarantined && !attempt.blocks_new_apply())
            {
                let outcome = if attempt.handoff.quarantined {
                    ApplyArchiveOutcome::Quarantined
                } else {
                    ApplyArchiveOutcome::RetriedPreparation
                };
                archive_and_remove(
                    &self.apply_history_dir()?,
                    &self.apply_receipt_file,
                    &attempt,
                    outcome,
                )
                .await?;
            } else {
                let output = self
                    .recover_with_resolution_locked(None, Some(&managed_codex_bin))
                    .await?;
                if output.status != ApplyStatus::Applied {
                    return Ok(Some(output));
                }
                handoff_resolutions = output.handoff_resolutions;
            }
        }

        let managed_backend_is_running = self.running_backend_instance(&settings).await?.is_some();
        let running_managed_codex_version = self.running_managed_codex_version_best_effort().await;
        let installed_version = self
            .managed_codex_version_best_effort(&managed_codex_bin)
            .await;
        if managed_backend_is_running && running_managed_codex_version.is_none() {
            let mut output = ApplyOutput::without_handoff(
                ApplyStatus::NeedsAttention,
                Some(managed_codex_bin.clone()),
                installed_version,
                &self.socket_path,
                Some("the running daemon PID has no full Codex launch identity; app-server version alone cannot establish the fork build".to_string()),
            );
            output.failure_kind = Some(ApplyFailureKind::RunningLauncherMismatch);
            output.running_managed_codex_version = None;
            output.app_server_version = client::probe(&self.socket_path)
                .await
                .ok()
                .map(|info| info.app_server_version);
            return Ok(Some(output));
        }
        if !launcher_update_required(
            &managed_codex_bin,
            managed_backend_is_running,
            running_managed_codex_version,
            installed_version,
        )? {
            return Ok(None);
        }
        self.apply_fresh_with_resolution(settings, &managed_codex_bin, handoff_resolutions)
            .await
            .map(Some)
    }
}

#[cfg(test)]
#[path = "target_tests.rs"]
mod tests;
