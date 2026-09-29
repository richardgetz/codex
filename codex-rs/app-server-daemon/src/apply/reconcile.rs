//! Target-aware handoff reconciliation and recovery for daemon launcher updates.

use super::*;
use crate::apply_archive::ApplyArchiveOutcome;
use crate::apply_archive::archive_and_resolve;
use crate::apply_archive::is_proven_idle_orphan;
use crate::apply_receipt::HandoffResolutionOutcome;
use crate::apply_receipt::HandoffRpcError;
use crate::apply_receipt::ensure_handoff_id;

pub(super) struct HandoffResponse {
    pub(super) receipt: HandoffReceipt,
    pub(super) server_codex_home: PathBuf,
}

impl Daemon {
    pub(super) async fn recover_attempt(
        &self,
        mut attempt: ApplyAttemptReceipt,
        managed_codex_bin: &Path,
        info: client::ProbeInfo,
        resolution: Option<&str>,
    ) -> Result<ApplyOutput> {
        let server_home = match self
            .validate_server_home(Some(&attempt), &info.codex_home)
            .await
        {
            Ok(home) => home,
            Err(error) => {
                return self
                    .mark_needs_attention_kind(
                        &mut attempt,
                        ApplyFailureKind::HandoffStorageMismatch,
                        error,
                    )
                    .await;
            }
        };
        attempt.origin_codex_home = Some(server_home.clone());
        attempt.phase = ApplyPhase::Recovering;
        attempt.save(&self.apply_receipt_file).await?;

        let status = self
            .request_handoff(STATUS_METHOD, &attempt.handoff.handoff_id, None)
            .await;
        match status {
            Ok(status) => {
                if let Err(error) = self
                    .validate_server_home(Some(&attempt), &status.server_codex_home)
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
                if let Err(error) = ensure_handoff_id(&attempt.handoff.handoff_id, &status.receipt)
                {
                    return self
                        .mark_needs_attention_kind(
                            &mut attempt,
                            ApplyFailureKind::HandoffWorkPending,
                            error,
                        )
                        .await;
                }
                attempt.handoff = status.receipt;
                if attempt.handoff.state == "completed" {
                    return self
                        .archive_resolved_attempt(
                            attempt,
                            HandoffResolutionOutcome::Recovered,
                            managed_codex_bin,
                            &info.app_server_version,
                        )
                        .await;
                }
                let can_recover = matches!(
                    attempt.handoff.state.as_str(),
                    "prepared" | "draining" | "suspended" | "restoring"
                ) || (attempt.handoff.state == "needsAttention"
                    && attempt.handoff.transfer_started == Some(true)
                    && !attempt.handoff.quarantined);
                if !can_recover {
                    return self
                        .mark_needs_attention_kind(
                            &mut attempt,
                            ApplyFailureKind::HandoffWorkPending,
                            "coordinator state is not eligible for automatic recovery",
                        )
                        .await;
                }
                attempt.save(&self.apply_receipt_file).await?;
            }
            Err(error) if error.is_unknown_handoff() => {
                return self
                    .resolve_missing_journal(
                        attempt,
                        error,
                        managed_codex_bin,
                        &info.app_server_version,
                    )
                    .await;
            }
            Err(error) => {
                let mut failure = error.to_string();
                if let Some(receipt) = error.receipt.as_ref()
                    && ensure_handoff_id(&attempt.handoff.handoff_id, receipt).is_ok()
                {
                    attempt.handoff = receipt.clone();
                } else if error.receipt.is_some() {
                    failure.push_str("; app server returned a receipt for a different handoff");
                }
                return self
                    .mark_needs_attention_kind(
                        &mut attempt,
                        if error.is_codex_home_mismatch() {
                            ApplyFailureKind::HandoffStorageMismatch
                        } else {
                            ApplyFailureKind::HandoffWorkPending
                        },
                        failure,
                    )
                    .await;
            }
        }

        match self
            .request_handoff(RECOVER_METHOD, &attempt.handoff.handoff_id, resolution)
            .await
        {
            Ok(recovered) => {
                if let Err(error) = self
                    .validate_server_home(Some(&attempt), &recovered.server_codex_home)
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
                    ensure_handoff_id(&attempt.handoff.handoff_id, &recovered.receipt)
                {
                    return self
                        .mark_needs_attention_kind(
                            &mut attempt,
                            ApplyFailureKind::HandoffWorkPending,
                            error,
                        )
                        .await;
                }
                attempt.handoff = recovered.receipt;
                if attempt.handoff.state == "completed" {
                    self.archive_resolved_attempt(
                        attempt,
                        HandoffResolutionOutcome::Recovered,
                        managed_codex_bin,
                        &info.app_server_version,
                    )
                    .await
                } else {
                    self.mark_needs_attention_kind(
                        &mut attempt,
                        ApplyFailureKind::HandoffWorkPending,
                        "coordinator did not complete exact handoff recovery",
                    )
                    .await
                }
            }
            Err(error) if error.is_unknown_handoff() => {
                self.resolve_missing_journal(
                    attempt,
                    error,
                    managed_codex_bin,
                    &info.app_server_version,
                )
                .await
            }
            Err(error) => {
                let mut failure = error.to_string();
                if let Some(receipt) = error.receipt.as_ref()
                    && ensure_handoff_id(&attempt.handoff.handoff_id, receipt).is_ok()
                {
                    attempt.handoff = receipt.clone();
                } else if error.receipt.is_some() {
                    failure.push_str("; app server returned a receipt for a different handoff");
                }
                self.mark_needs_attention_kind(
                    &mut attempt,
                    if error.is_codex_home_mismatch() {
                        ApplyFailureKind::HandoffStorageMismatch
                    } else {
                        ApplyFailureKind::HandoffWorkPending
                    },
                    failure,
                )
                .await
            }
        }
    }

    async fn resolve_missing_journal(
        &self,
        mut attempt: ApplyAttemptReceipt,
        error: HandoffRpcError,
        managed_codex_bin: &Path,
        app_server_version: &str,
    ) -> Result<ApplyOutput> {
        let Some(server_home) = error.server_codex_home.as_deref() else {
            return self
                .mark_needs_attention_kind(
                    &mut attempt,
                    ApplyFailureKind::HandoffStorageMismatch,
                    "the server did not report its Codex home, so the missing handoff journal cannot be classified safely",
                )
                .await;
        };
        let server_home = match self.validate_server_home(Some(&attempt), server_home).await {
            Ok(home) => home,
            Err(home_error) => {
                return self
                    .mark_needs_attention_kind(
                        &mut attempt,
                        ApplyFailureKind::HandoffStorageMismatch,
                        home_error,
                    )
                    .await;
            }
        };
        attempt.origin_codex_home = Some(server_home.clone());
        if is_proven_idle_orphan(&attempt).await? {
            return self
                .archive_resolved_attempt(
                    attempt,
                    HandoffResolutionOutcome::RetiredIdleOrphan,
                    managed_codex_bin,
                    app_server_version,
                )
                .await;
        }
        self.mark_needs_attention_kind(
            &mut attempt,
            ApplyFailureKind::HandoffJournalMissing,
            format!(
                "{}; the handoff journal is absent from {} and the saved receipt does not prove an idle, fully stopped graph, so its nodes were preserved for diagnosis",
                error,
                server_home.display()
            ),
        )
        .await
    }

    async fn archive_resolved_attempt(
        &self,
        attempt: ApplyAttemptReceipt,
        outcome: HandoffResolutionOutcome,
        managed_codex_bin: &Path,
        app_server_version: &str,
    ) -> Result<ApplyOutput> {
        let archive_outcome = match outcome {
            HandoffResolutionOutcome::Recovered => ApplyArchiveOutcome::Recovered,
            HandoffResolutionOutcome::RetiredIdleOrphan => ApplyArchiveOutcome::RetiredIdleOrphan,
        };
        let resolved = archive_and_resolve(
            &self.apply_history_dir()?,
            &self.apply_receipt_file,
            &attempt,
            archive_outcome,
            HandoffResolution {
                handoff_id: attempt.handoff.handoff_id.clone(),
                outcome,
            },
            managed_codex_bin,
            self.managed_codex_version_best_effort(managed_codex_bin)
                .await,
        )
        .await?;
        Ok(resolved.output_with_running_version(
            &self.socket_path,
            Some(app_server_version.to_string()),
            /*error*/ None,
            self.running_managed_codex_version_best_effort().await,
        ))
    }

    pub(super) async fn request_handoff(
        &self,
        method: &str,
        handoff_id: &str,
        resolution: Option<&str>,
    ) -> std::result::Result<HandoffResponse, HandoffRpcError> {
        let params = match resolution {
            Some(resolution) => serde_json::json!({
                "handoffId": handoff_id,
                "resolution": resolution,
            }),
            None => serde_json::json!({ "handoffId": handoff_id }),
        };
        let expected_codex_home = self.codex_home().map_err(|error| HandoffRpcError {
            method: method.to_string(),
            message: error.to_string(),
            receipt: None,
            server_codex_home: None,
            codex_home_mismatch: false,
        })?;
        let response = client::request_in_codex_home(
            &self.socket_path,
            method,
            Some(params),
            &expected_codex_home,
        )
        .await
        .map_err(|error| {
            let home_mismatch = error.downcast_ref::<client::CodexHomeMismatch>();
            HandoffRpcError {
                method: method.to_string(),
                message: error.to_string(),
                receipt: None,
                server_codex_home: home_mismatch.map(|mismatch| mismatch.actual.clone()),
                codex_home_mismatch: home_mismatch.is_some(),
            }
        })?;
        let server_codex_home = response.codex_home;
        match parse_handoff_response(response.message, method) {
            Ok(receipt) => Ok(HandoffResponse {
                receipt,
                server_codex_home,
            }),
            Err(mut error) => {
                error.server_codex_home = Some(server_codex_home);
                Err(error)
            }
        }
    }
}
