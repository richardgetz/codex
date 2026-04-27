//! Harness-side built-in scratchpad recovery for compaction events.
//!
//! The recovery path is intentionally mechanical: the TUI reads the built-in scratchpad JSON for
//! the active thread/session and only then injects a compact state summary back into the model.

use super::*;

impl App {
    pub(super) fn recover_scratchpad_after_compaction(&mut self, thread_id: ThreadId) {
        if !self.pending_scratchpad_recoveries.insert(thread_id) {
            return;
        }

        let app_event_tx = self.app_event_tx.clone();
        let codex_home = self.config.codex_home.to_path_buf();
        tokio::spawn(async move {
            let result = build_compaction_recovery_message(&codex_home, thread_id)
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::ScratchpadCompactionRecoveryLoaded { thread_id, result });
        });
    }

    pub(super) fn finish_scratchpad_compaction_recovery(
        &mut self,
        thread_id: ThreadId,
        result: Result<String, String>,
    ) {
        self.pending_scratchpad_recoveries.remove(&thread_id);
        match result {
            Ok(message) => self.chat_widget.submit_external_user_message(message),
            Err(err) => {
                tracing::warn!("built-in scratchpad compaction recovery failed: {err}");
                self.chat_widget.submit_external_user_message(format!(
                    "Post-compaction scratchpad recovery check failed mechanically for thread {thread_id}: {err}. Continue carefully, and if the task remains non-trivial, open or update the built-in scratchpad before proceeding."
                ));
            }
        }
    }
}

fn build_compaction_recovery_message(codex_home: &Path, thread_id: ThreadId) -> Result<String> {
    let scratchpad_id = thread_id.to_string();
    let path = codex_home
        .join("scratchpad")
        .join("entries")
        .join(format!("{scratchpad_id}.json"));

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(format!(
                "Post-compaction scratchpad check completed mechanically for thread {scratchpad_id}. No built-in scratchpad exists yet at {}. If this is continuing non-trivial work, open the built-in scratchpad now and keep it updated as the active recovery ledger.",
                path.display()
            ));
        }
        Err(err) => return Err(err.into()),
    };

    let value: serde_json::Value = serde_json::from_str(&text)
        .wrap_err_with(|| format!("scratchpad file `{}` is invalid JSON", path.display()))?;
    let summary = compact_scratchpad_summary(&value);
    Ok(format!(
        "Post-compaction scratchpad check completed mechanically for thread {scratchpad_id}. Continue from this recovered built-in scratchpad state and keep the scratchpad updated before future waits, delegation, or risky actions:\n\n```json\n{}\n```",
        serde_json::to_string_pretty(&summary)?
    ))
}

fn compact_scratchpad_summary(value: &serde_json::Value) -> serde_json::Value {
    let mut summary = serde_json::Map::new();
    for key in [
        "scratchpad_id",
        "objective",
        "status",
        "session_key",
        "completed",
        "next_steps",
        "pending_waits",
        "stop_conditions",
        "resume_instructions",
        "final_guard",
        "updated_at",
        "archived_at",
    ] {
        if let Some(item) = value.get(key) {
            summary.insert(key.to_string(), item.clone());
        }
    }
    if let Some(notes) = value.get("notes").and_then(serde_json::Value::as_array) {
        let recent_notes = notes
            .iter()
            .rev()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        summary.insert(
            "recent_notes".to_string(),
            serde_json::Value::Array(recent_notes),
        );
        summary.insert("notes_count".to_string(), serde_json::json!(notes.len()));
    }
    serde_json::Value::Object(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[test]
    fn compaction_recovery_reads_thread_scratchpad_summary() {
        let home = TempDir::new().expect("temp home");
        let thread_id =
            ThreadId::from_string("019dc9d0-b833-75b2-8eed-54725b0afef3").expect("thread id");
        let entries = home.path().join("scratchpad").join("entries");
        std::fs::create_dir_all(&entries).expect("create entries");
        std::fs::write(
            entries.join(format!("{thread_id}.json")),
            serde_json::json!({
                "scratchpad_id": thread_id.to_string(),
                "objective": "ship scratchpad recovery",
                "status": "waiting",
                "next_steps": ["finish hook"],
                "pending_waits": [{"target": "ci"}],
                "notes": [
                    {"summary": "one"},
                    {"summary": "two"},
                    {"summary": "three"},
                    {"summary": "four"},
                    {"summary": "five"},
                    {"summary": "six"}
                ],
                "updated_at": "2026-04-27T00:00:00Z"
            })
            .to_string(),
        )
        .expect("write scratchpad");

        let message =
            build_compaction_recovery_message(home.path(), thread_id).expect("recovery message");

        assert!(message.contains("completed mechanically"));
        assert!(message.contains("ship scratchpad recovery"));
        assert!(message.contains("\"notes_count\": 6"));
        assert!(!message.contains("\"summary\": \"one\""));
        assert!(message.contains("\"summary\": \"six\""));
    }

    #[test]
    fn compaction_recovery_handles_missing_thread_scratchpad() {
        let home = TempDir::new().expect("temp home");
        let thread_id =
            ThreadId::from_string("019dc9d0-b833-75b2-8eed-54725b0afef3").expect("thread id");

        let message =
            build_compaction_recovery_message(home.path(), thread_id).expect("recovery message");

        assert!(message.contains("No built-in scratchpad exists yet"));
        assert!(message.contains(&thread_id.to_string()));
    }

    #[test]
    fn compact_summary_keeps_only_recent_notes() {
        let value = serde_json::json!({
            "scratchpad_id": "sp",
            "objective": "obj",
            "notes": [
                {"summary": "1"},
                {"summary": "2"},
                {"summary": "3"},
                {"summary": "4"},
                {"summary": "5"},
                {"summary": "6"}
            ],
            "ignored": "not included"
        });

        let summary = compact_scratchpad_summary(&value);

        assert_eq!(
            summary,
            serde_json::json!({
                "scratchpad_id": "sp",
                "objective": "obj",
                "recent_notes": [
                    {"summary": "2"},
                    {"summary": "3"},
                    {"summary": "4"},
                    {"summary": "5"},
                    {"summary": "6"}
                ],
                "notes_count": 6
            })
        );
    }
}
