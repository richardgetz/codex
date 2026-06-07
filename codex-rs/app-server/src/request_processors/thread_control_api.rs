use super::*;

enum RouterControlLookup {
    Loaded,
    Failed,
}

fn should_keep_loaded_for_router_lookup(lookup: RouterControlLookup) -> bool {
    match lookup {
        RouterControlLookup::Failed => true,
        RouterControlLookup::Loaded => false,
    }
}

pub(super) async fn should_keep_loaded_for_active_router_control(
    conversation_id: ThreadId,
    state_db: &StateDbHandle,
) -> bool {
    match state_db.get_active_thread_control(conversation_id).await {
        Ok(control) => {
            let _ = control;
            should_keep_loaded_for_router_lookup(RouterControlLookup::Loaded)
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
        let _thread_uuid = ThreadId::from_string(&params.thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;
        let reason = params.reason.trim().to_string();
        if reason.is_empty() {
            return Err(invalid_request("reason must not be empty".to_string()));
        }
        Err(invalid_request(
            "orchestrator mode is no longer available".to_string(),
        ))
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
}

pub(super) fn thread_control_from_state_record(
    _record: ThreadControlRecord,
) -> Option<ThreadControl> {
    None
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
    fn router_unload_lookup_keeps_thread_loaded_only_for_lookup_failures() {
        assert_eq!(
            should_keep_loaded_for_router_lookup(RouterControlLookup::Failed),
            true
        );
        assert_eq!(
            should_keep_loaded_for_router_lookup(RouterControlLookup::Loaded),
            false
        );
    }

    #[test]
    fn legacy_continuous_control_is_not_reported_as_orchestrator_control() {
        let router = thread_control_record(ThreadControlMode::Router);
        let continuous = thread_control_record(ThreadControlMode::Continuous);

        assert_eq!(thread_control_from_state_record(router), None);
        assert_eq!(thread_control_from_state_record(continuous), None);
    }
}
