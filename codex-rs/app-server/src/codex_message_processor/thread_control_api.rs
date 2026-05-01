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
    state_db: &Arc<StateRuntime>,
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

impl CodexMessageProcessor {
    pub(super) async fn thread_control_read(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadControlReadParams,
    ) {
        let thread_uuid = match ThreadId::from_string(&params.thread_id) {
            Ok(id) => id,
            Err(err) => {
                self.send_invalid_request_error(request_id, format!("invalid thread id: {err}"))
                    .await;
                return;
            }
        };
        let loaded_thread = self.thread_manager.get_thread(thread_uuid).await.ok();
        let mut state_db_ctx = loaded_thread.as_ref().and_then(|thread| thread.state_db());
        if state_db_ctx.is_none() {
            state_db_ctx = get_state_db(&self.config).await;
        }
        let Some(state_db_ctx) = state_db_ctx else {
            self.send_internal_error(
                request_id,
                format!("sqlite state db unavailable for thread {thread_uuid}"),
            )
            .await;
            return;
        };

        let control = match state_db_ctx.get_active_thread_control(thread_uuid).await {
            Ok(control) => control.and_then(thread_control_from_state_record),
            Err(err) => {
                self.send_internal_error(
                    request_id,
                    format!("failed to load thread control for {thread_uuid}: {err}"),
                )
                .await;
                return;
            }
        };

        self.outgoing
            .send_response(request_id, ThreadControlReadResponse { control })
            .await;
    }

    pub(super) async fn thread_control_set(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadControlSetParams,
    ) {
        let thread_uuid = match ThreadId::from_string(&params.thread_id) {
            Ok(id) => id,
            Err(err) => {
                self.send_invalid_request_error(request_id, format!("invalid thread id: {err}"))
                    .await;
                return;
            }
        };
        let reason = params.reason.trim().to_string();
        if reason.is_empty() {
            self.send_invalid_request_error(request_id, "reason must not be empty".to_string())
                .await;
            return;
        }
        if matches!(params.mode, ThreadControlMode::Orchestrator)
            && matches!(params.watch_interval_seconds, Some(0) | None)
        {
            self.send_invalid_request_error(
                request_id,
                "orchestrator mode requires watchIntervalSeconds > 0".to_string(),
            )
            .await;
            return;
        }
        let loaded_thread = self.thread_manager.get_thread(thread_uuid).await.ok();
        if matches!(params.mode, ThreadControlMode::Orchestrator) && loaded_thread.is_none() {
            self.send_invalid_request_error(
                request_id,
                "orchestrator mode currently requires a loaded thread".to_string(),
            )
            .await;
            return;
        }
        let target_thread_ids = match params.target_thread_ids {
            Some(target_thread_ids) => {
                let mut parsed = Vec::with_capacity(target_thread_ids.len());
                for (index, target_thread_id) in target_thread_ids.into_iter().enumerate() {
                    let target_thread_id = match ThreadId::from_string(&target_thread_id) {
                        Ok(id) => id,
                        Err(err) => {
                            self.send_invalid_request_error(
                                request_id,
                                format!("targetThreadIds[{index}] is not a valid thread id: {err}"),
                            )
                            .await;
                            return;
                        }
                    };
                    if target_thread_id == thread_uuid {
                        self.send_invalid_request_error(
                            request_id,
                            "targetThreadIds must not include the control thread itself"
                                .to_string(),
                        )
                        .await;
                        return;
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
            state_db_ctx = get_state_db(&self.config).await;
        }
        let Some(state_db_ctx) = state_db_ctx else {
            self.send_internal_error(
                request_id,
                format!("sqlite state db unavailable for thread {thread_uuid}"),
            )
            .await;
            return;
        };

        if let Err(error) = self
            .ensure_thread_metadata_row_exists(thread_uuid, &state_db_ctx, loaded_thread.as_ref())
            .await
        {
            self.outgoing.send_error(request_id, error).await;
            return;
        }

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
        if let Err(err) = state_db_ctx.upsert_thread_control(&control).await {
            self.send_internal_error(
                request_id,
                format!("failed to persist thread control for {thread_uuid}: {err}"),
            )
            .await;
            return;
        }
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

        self.outgoing
            .send_response(
                request_id,
                ThreadControlSetResponse {
                    control: thread_control_from_state_record(control)
                        .expect("new thread control records are public API modes"),
                },
            )
            .await;
    }

    pub(super) async fn thread_control_release(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadControlReleaseParams,
    ) {
        let thread_uuid = match ThreadId::from_string(&params.thread_id) {
            Ok(id) => id,
            Err(err) => {
                self.send_invalid_request_error(request_id, format!("invalid thread id: {err}"))
                    .await;
                return;
            }
        };
        let loaded_thread = self.thread_manager.get_thread(thread_uuid).await.ok();
        let mut state_db_ctx = loaded_thread.as_ref().and_then(|thread| thread.state_db());
        if state_db_ctx.is_none() {
            state_db_ctx = get_state_db(&self.config).await;
        }
        let Some(state_db_ctx) = state_db_ctx else {
            self.send_internal_error(
                request_id,
                format!("sqlite state db unavailable for thread {thread_uuid}"),
            )
            .await;
            return;
        };

        let control = match state_db_ctx
            .release_thread_control(thread_uuid, Utc::now())
            .await
        {
            Ok(control) => control.and_then(thread_control_from_state_record),
            Err(err) => {
                self.send_internal_error(
                    request_id,
                    format!("failed to release thread control for {thread_uuid}: {err}"),
                )
                .await;
                return;
            }
        };
        if let Some(loaded_thread) = loaded_thread.as_ref() {
            loaded_thread
                .set_active_thread_control(/*control*/ None)
                .await;
        }

        let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
        clear_router_tick(&thread_state).await;

        self.outgoing
            .send_response(request_id, ThreadControlReleaseResponse { control })
            .await;
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
