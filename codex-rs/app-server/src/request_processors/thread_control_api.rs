use super::*;

enum RouterControlLookup<'a> {
    Loaded(Option<&'a ThreadControlRecord>),
    Failed,
}

fn should_keep_loaded_for_router_lookup(lookup: RouterControlLookup<'_>) -> bool {
    match lookup {
        RouterControlLookup::Failed => true,
        RouterControlLookup::Loaded(Some(control))
            if matches!(control.mode, codex_state::ThreadControlMode::Router) =>
        {
            true
        }
        RouterControlLookup::Loaded(_) => false,
    }
}

pub(super) async fn should_keep_loaded_for_active_router_control(
    conversation_id: ThreadId,
    state_db: &StateDbHandle,
) -> bool {
    match state_db.get_active_thread_control(conversation_id).await {
        Ok(control) => {
            should_keep_loaded_for_router_lookup(RouterControlLookup::Loaded(control.as_ref()))
        }
        Err(err) => {
            tracing::warn!(
                thread_id = %conversation_id,
                "failed to load router control before unloading thread: {err}"
            );
            should_keep_loaded_for_router_lookup(RouterControlLookup::Failed)
        }
    }
}

impl ThreadRequestProcessor {
    pub(crate) async fn thread_control_read(
        &self,
        params: ThreadControlReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_uuid = ThreadId::from_string(&params.thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;
        let loaded_thread = self.thread_manager.get_thread(thread_uuid).await.ok();
        let mut state_db_ctx = loaded_thread.as_ref().and_then(|thread| thread.state_db());
        if state_db_ctx.is_none() {
            state_db_ctx = self.state_db.clone();
        }
        let state_db_ctx = state_db_ctx.ok_or_else(|| {
            internal_error(format!(
                "sqlite state db unavailable for thread {thread_uuid}"
            ))
        })?;

        let control = state_db_ctx
            .get_active_thread_control(thread_uuid)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to load thread control for {thread_uuid}: {err}"
                ))
            })?
            .and_then(thread_control_from_state_record);

        Ok(Some(ThreadControlReadResponse { control }.into()))
    }

    pub(crate) async fn thread_control_set(
        &self,
        params: ThreadControlSetParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_uuid = ThreadId::from_string(&params.thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;
        let reason = params.reason.trim().to_string();
        if reason.is_empty() {
            return Err(invalid_request("reason must not be empty".to_string()));
        }
        if matches!(params.mode, ThreadControlMode::Orchestrator)
            && matches!(params.watch_interval_seconds, Some(0) | None)
        {
            return Err(invalid_request(
                "orchestrator mode requires watchIntervalSeconds > 0".to_string(),
            ));
        }
        let loaded_thread = self.thread_manager.get_thread(thread_uuid).await.ok();
        if matches!(params.mode, ThreadControlMode::Orchestrator) && loaded_thread.is_none() {
            return Err(invalid_request(
                "orchestrator mode currently requires a loaded thread".to_string(),
            ));
        }
        let target_thread_ids = match params.target_thread_ids {
            Some(target_thread_ids) => {
                let mut parsed = Vec::with_capacity(target_thread_ids.len());
                for (index, target_thread_id) in target_thread_ids.into_iter().enumerate() {
                    let target_thread_id =
                        ThreadId::from_string(&target_thread_id).map_err(|err| {
                            invalid_request(format!(
                                "targetThreadIds[{index}] is not a valid thread id: {err}"
                            ))
                        })?;
                    if target_thread_id == thread_uuid {
                        return Err(invalid_request(
                            "targetThreadIds must not include the control thread itself"
                                .to_string(),
                        ));
                    }
                    parsed.push(target_thread_id);
                }
                parsed.sort_by_key(ToString::to_string);
                parsed.dedup();
                parsed
            }
            None => Vec::new(),
        };
        let mut state_db_ctx = loaded_thread.as_ref().and_then(|thread| thread.state_db());
        if state_db_ctx.is_none() {
            state_db_ctx = self.state_db.clone();
        }
        let state_db_ctx = state_db_ctx.ok_or_else(|| {
            internal_error(format!(
                "sqlite state db unavailable for thread {thread_uuid}"
            ))
        })?;

        self.ensure_thread_metadata_row_exists(thread_uuid, &state_db_ctx, loaded_thread.as_ref())
            .await?;

        let control = ThreadControlRecord {
            thread_id: thread_uuid,
            mode: match params.mode {
                ThreadControlMode::Orchestrator => StateThreadControlMode::Router,
            },
            reason,
            release_channel: params.release_channel,
            watch_interval_seconds: params.watch_interval_seconds,
            released_at: None,
            updated_at: Utc::now(),
            target_thread_ids,
        };
        state_db_ctx
            .upsert_thread_control(&control)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to persist thread control for {thread_uuid}: {err}"
                ))
            })?;
        if let Some(loaded_thread) = loaded_thread.as_ref() {
            loaded_thread
                .set_active_thread_control(Some(control.clone()))
                .await;
        }

        let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
        if matches!(control.mode, StateThreadControlMode::Router) {
            if let Some(loaded_thread) = loaded_thread {
                refresh_router_tick(loaded_thread, thread_state, Arc::clone(&state_db_ctx)).await;
            }
        } else {
            clear_router_tick(&thread_state).await;
        }

        let control = thread_control_from_state_record(control).ok_or_else(|| {
            internal_error("new thread control records must use a public API mode".to_string())
        })?;
        Ok(Some(ThreadControlSetResponse { control }.into()))
    }

    pub(crate) async fn thread_control_release(
        &self,
        params: ThreadControlReleaseParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_uuid = ThreadId::from_string(&params.thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;
        let loaded_thread = self.thread_manager.get_thread(thread_uuid).await.ok();
        let mut state_db_ctx = loaded_thread.as_ref().and_then(|thread| thread.state_db());
        if state_db_ctx.is_none() {
            state_db_ctx = self.state_db.clone();
        }
        let state_db_ctx = state_db_ctx.ok_or_else(|| {
            internal_error(format!(
                "sqlite state db unavailable for thread {thread_uuid}"
            ))
        })?;

        let control = state_db_ctx
            .release_thread_control(thread_uuid, Utc::now())
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to release thread control for {thread_uuid}: {err}"
                ))
            })?
            .and_then(thread_control_from_state_record);
        if let Some(loaded_thread) = loaded_thread.as_ref() {
            loaded_thread
                .set_active_thread_control(/*control*/ None)
                .await;
        }

        let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
        clear_router_tick(&thread_state).await;

        Ok(Some(ThreadControlReleaseResponse { control }.into()))
    }

    async fn ensure_thread_metadata_row_exists(
        &self,
        thread_uuid: ThreadId,
        state_db_ctx: &StateDbHandle,
        loaded_thread: Option<&Arc<CodexThread>>,
    ) -> Result<(), JSONRPCErrorError> {
        match state_db_ctx.get_thread(thread_uuid).await {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(err) => {
                return Err(internal_error(format!(
                    "failed to load thread metadata for {thread_uuid}: {err}"
                )));
            }
        }

        if let Some(thread) = loaded_thread {
            let Some(rollout_path) = thread.rollout_path() else {
                return Err(invalid_request(format!(
                    "ephemeral thread does not support metadata updates: {thread_uuid}"
                )));
            };

            reconcile_rollout(
                Some(state_db_ctx),
                rollout_path.as_path(),
                self.config.model_provider_id.as_str(),
                /*builder*/ None,
                &[],
                /*archived_only*/ None,
                /*new_thread_memory_mode*/ None,
            )
            .await;

            match state_db_ctx.get_thread(thread_uuid).await {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {}
                Err(err) => {
                    return Err(internal_error(format!(
                        "failed to load reconciled thread metadata for {thread_uuid}: {err}"
                    )));
                }
            }

            let config_snapshot = thread.config_snapshot().await;
            let model_provider = config_snapshot.model_provider_id.clone();
            let mut builder = ThreadMetadataBuilder::new(
                thread_uuid,
                rollout_path,
                Utc::now(),
                config_snapshot.session_source.clone(),
            );
            builder.model_provider = Some(model_provider.clone());
            builder.cwd = config_snapshot.cwd.to_path_buf();
            builder.cli_version = Some(env!("CARGO_PKG_VERSION").to_string());
            builder.sandbox_policy = config_snapshot.sandbox_policy();
            builder.approval_mode = config_snapshot.approval_policy;
            let metadata = builder.build(model_provider.as_str());
            state_db_ctx
                .insert_thread_if_absent(&metadata)
                .await
                .map_err(|err| {
                    internal_error(format!(
                        "failed to create thread metadata for {thread_uuid}: {err}"
                    ))
                })?;
            return Ok(());
        }

        let rollout_path = match find_thread_path_by_id_str(
            &self.config.codex_home,
            &thread_uuid.to_string(),
            self.state_db.as_deref(),
        )
        .await
        {
            Ok(Some(path)) => path,
            Ok(None) => match find_archived_thread_path_by_id_str(
                &self.config.codex_home,
                &thread_uuid.to_string(),
                self.state_db.as_deref(),
            )
            .await
            {
                Ok(Some(path)) => path,
                Ok(None) => {
                    return Err(invalid_request(format!("thread not found: {thread_uuid}")));
                }
                Err(err) => {
                    return Err(internal_error(format!(
                        "failed to locate archived thread id {thread_uuid}: {err}"
                    )));
                }
            },
            Err(err) => {
                return Err(internal_error(format!(
                    "failed to locate thread id {thread_uuid}: {err}"
                )));
            }
        };

        reconcile_rollout(
            Some(state_db_ctx),
            rollout_path.as_path(),
            self.config.model_provider_id.as_str(),
            /*builder*/ None,
            &[],
            /*archived_only*/ None,
            /*new_thread_memory_mode*/ None,
        )
        .await;

        match state_db_ctx.get_thread(thread_uuid).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(internal_error(format!(
                "failed to create thread metadata from rollout for {thread_uuid}"
            ))),
            Err(err) => Err(internal_error(format!(
                "failed to load reconciled thread metadata for {thread_uuid}: {err}"
            ))),
        }
    }
}

pub(super) fn thread_control_from_state_record(
    record: ThreadControlRecord,
) -> Option<ThreadControl> {
    let mode = match record.mode {
        StateThreadControlMode::Router => ThreadControlMode::Orchestrator,
        StateThreadControlMode::Continuous => return None,
    };
    Some(ThreadControl {
        thread_id: record.thread_id.to_string(),
        mode,
        reason: record.reason,
        release_channel: record.release_channel,
        watch_interval_seconds: record.watch_interval_seconds,
        released_at: record
            .released_at
            .map(|released_at| released_at.timestamp()),
        updated_at: record.updated_at.timestamp(),
        target_thread_ids: record
            .target_thread_ids
            .into_iter()
            .map(|thread_id| thread_id.to_string())
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::RouterControlLookup;
    use super::should_keep_loaded_for_router_lookup;
    use super::thread_control_from_state_record;
    use chrono::TimeZone;
    use chrono::Utc;
    use codex_protocol::ThreadId;
    use codex_state::ThreadControlMode;
    use codex_state::ThreadControlRecord;
    use pretty_assertions::assert_eq;

    fn thread_control_record(mode: ThreadControlMode) -> ThreadControlRecord {
        ThreadControlRecord {
            thread_id: ThreadId::from_string("00000000-0000-0000-0000-000000000011")
                .expect("thread id"),
            mode,
            reason: "Keep routing work".to_string(),
            release_channel: Some("imessage".to_string()),
            watch_interval_seconds: Some(30),
            released_at: None,
            updated_at: Utc
                .timestamp_opt(1_700_000_123, 0)
                .single()
                .expect("updated_at"),
            target_thread_ids: Vec::new(),
        }
    }

    #[test]
    fn router_unload_lookup_keeps_thread_loaded_for_failures_and_active_router_control() {
        let router = thread_control_record(ThreadControlMode::Router);
        let continuous = thread_control_record(ThreadControlMode::Continuous);

        assert_eq!(
            should_keep_loaded_for_router_lookup(RouterControlLookup::Failed),
            true
        );
        assert_eq!(
            should_keep_loaded_for_router_lookup(RouterControlLookup::Loaded(Some(&router))),
            true
        );
        assert_eq!(
            should_keep_loaded_for_router_lookup(RouterControlLookup::Loaded(Some(&continuous))),
            false
        );
        assert_eq!(
            should_keep_loaded_for_router_lookup(RouterControlLookup::Loaded(None)),
            false
        );
    }

    #[test]
    fn legacy_continuous_control_is_not_reported_as_orchestrator_control() {
        let continuous = thread_control_record(ThreadControlMode::Continuous);

        assert_eq!(thread_control_from_state_record(continuous), None);
    }
}
