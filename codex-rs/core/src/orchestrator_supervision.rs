use crate::agent::AgentStatus;
use codex_config::types::OrchestratorEscalationConfig;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_template::Template;
use serde::Deserialize;
use serde::Serialize;
use std::fs as std_fs;
use std::sync::Arc;
use std::sync::LazyLock;
use tokio::fs;
use tokio::sync::Mutex;
use tokio::sync::watch::Receiver;
use tracing::warn;

static ORCHESTRATOR_SUPERVISION_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    Template::parse(include_str!(
        "../templates/orchestrator_supervision/read_path.md"
    ))
    .unwrap_or_else(|err| {
        panic!("embedded template orchestrator_supervision/read_path.md is invalid: {err}")
    })
});

pub(crate) fn root(codex_home: &AbsolutePathBuf) -> AbsolutePathBuf {
    codex_home.join("orchestrator_supervision")
}

#[derive(Clone)]
pub(crate) struct OrchestratorSupervisionStore {
    codex_home: AbsolutePathBuf,
    write_lock: Arc<Mutex<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorSupervisionPollState {
    pub last_updated_at: Option<String>,
    pub has_supervised_workers: bool,
    pub has_nonterminal_workers: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct SupervisionLedger {
    threads: Vec<SupervisedThreadState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SupervisedThreadState {
    thread_id: String,
    updated_at: String,
    workers: Vec<SupervisedWorkerState>,
    #[serde(default)]
    watched_sessions: Vec<WatchedSessionState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SupervisedWorkerState {
    worker_thread_id: String,
    nickname: Option<String>,
    role: Option<String>,
    prompt_preview: String,
    collaboration_mode: Option<ModeKind>,
    status: SupervisedWorkerStatus,
    last_status_summary: Option<String>,
    created_at: String,
    updated_at: String,
    last_instruction_at: Option<String>,
    last_checked_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WatchedSessionState {
    thread_id: String,
    title: Option<String>,
    cwd: Option<String>,
    status: WatchedSessionStatus,
    last_event_summary: Option<String>,
    created_at: String,
    updated_at: String,
    last_instruction_at: Option<String>,
    last_checked_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SupervisedWorkerStatus {
    PendingInit,
    Running,
    Completed,
    Interrupted,
    Errored,
    Shutdown,
    NotFound,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WatchedSessionStatus {
    Watched,
    Completed,
    Interrupted,
    Errored,
}

impl OrchestratorSupervisionStore {
    pub(crate) fn new(codex_home: AbsolutePathBuf) -> Self {
        Self {
            codex_home,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn root(&self) -> AbsolutePathBuf {
        root(&self.codex_home)
    }

    fn state_path(&self) -> AbsolutePathBuf {
        self.root().join("state.json")
    }

    pub(crate) async fn register_worker(
        &self,
        parent_thread_id: ThreadId,
        worker_thread_id: ThreadId,
        nickname: Option<String>,
        role: Option<String>,
        prompt_preview: String,
        collaboration_mode: Option<ModeKind>,
    ) -> std::io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let worker_thread_id = worker_thread_id.to_string();
        self.mutate_ledger_sync(move |ledger| {
            let now = now_rfc3339();
            let parent = ensure_thread_entry(ledger, parent_thread_id, &now);
            if let Some(existing) = parent
                .workers
                .iter_mut()
                .find(|worker| worker.worker_thread_id == worker_thread_id)
            {
                existing.nickname = nickname;
                existing.role = role;
                existing.prompt_preview = prompt_preview;
                existing.collaboration_mode = collaboration_mode;
                existing.updated_at = now.clone();
            } else {
                parent.workers.push(SupervisedWorkerState {
                    worker_thread_id,
                    nickname,
                    role,
                    prompt_preview,
                    collaboration_mode,
                    status: SupervisedWorkerStatus::PendingInit,
                    last_status_summary: None,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    last_instruction_at: None,
                    last_checked_at: None,
                });
            }
            parent.updated_at = now;
        })
    }

    pub(crate) async fn note_status(
        &self,
        parent_thread_id: ThreadId,
        worker_thread_id: ThreadId,
        status: &AgentStatus,
    ) -> std::io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let worker_thread_id = worker_thread_id.to_string();
        let worker_status = SupervisedWorkerStatus::from(status);
        let summary = status_summary(status);
        self.mutate_ledger_sync(move |ledger| {
            let now = now_rfc3339();
            let parent = ensure_thread_entry(ledger, parent_thread_id, &now);
            if let Some(worker) = parent
                .workers
                .iter_mut()
                .find(|worker| worker.worker_thread_id == worker_thread_id)
            {
                worker.status = worker_status;
                if let Some(summary) = summary.clone() {
                    worker.last_status_summary = Some(summary);
                }
                worker.updated_at = now.clone();
            }
            parent.updated_at = now;
        })
    }

    pub(crate) async fn note_instruction(
        &self,
        parent_thread_id: ThreadId,
        worker_thread_id: ThreadId,
    ) -> std::io::Result<()> {
        self.note_timestamp(
            parent_thread_id,
            worker_thread_id,
            TimestampKind::Instruction,
        )
        .await
    }

    pub(crate) async fn note_check(
        &self,
        parent_thread_id: ThreadId,
        worker_thread_id: ThreadId,
    ) -> std::io::Result<()> {
        self.note_timestamp(parent_thread_id, worker_thread_id, TimestampKind::Check)
            .await
    }

    pub(crate) async fn register_watched_session(
        &self,
        orchestrator_thread_id: ThreadId,
        target_thread_id: ThreadId,
        title: Option<String>,
        cwd: Option<String>,
    ) -> std::io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let target_thread_id = target_thread_id.to_string();
        self.mutate_ledger_sync(move |ledger| {
            let now = now_rfc3339();
            let parent = ensure_thread_entry(ledger, orchestrator_thread_id, &now);
            if let Some(existing) = parent
                .watched_sessions
                .iter_mut()
                .find(|session| session.thread_id == target_thread_id)
            {
                existing.title = title;
                existing.cwd = cwd;
                existing.status = WatchedSessionStatus::Watched;
                existing.updated_at = now.clone();
            } else {
                parent.watched_sessions.push(WatchedSessionState {
                    thread_id: target_thread_id,
                    title,
                    cwd,
                    status: WatchedSessionStatus::Watched,
                    last_event_summary: None,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    last_instruction_at: None,
                    last_checked_at: None,
                });
            }
            parent.updated_at = now;
        })
    }

    pub(crate) async fn remove_watched_session(
        &self,
        orchestrator_thread_id: ThreadId,
        target_thread_id: ThreadId,
    ) -> std::io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let target_thread_id = target_thread_id.to_string();
        self.mutate_ledger_sync(move |ledger| {
            let now = now_rfc3339();
            let parent = ensure_thread_entry(ledger, orchestrator_thread_id, &now);
            parent
                .watched_sessions
                .retain(|session| session.thread_id != target_thread_id);
            parent.updated_at = now;
        })
    }

    pub(crate) async fn note_watched_session_instruction(
        &self,
        orchestrator_thread_id: ThreadId,
        target_thread_id: ThreadId,
    ) -> std::io::Result<()> {
        self.note_watched_session_timestamp(
            orchestrator_thread_id,
            target_thread_id,
            TimestampKind::Instruction,
        )
        .await
    }

    pub(crate) async fn note_watched_session_event(
        &self,
        orchestrator_thread_id: ThreadId,
        target_thread_id: ThreadId,
        status: &AgentStatus,
    ) -> std::io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let target_thread_id = target_thread_id.to_string();
        let watched_status = WatchedSessionStatus::from(status);
        let summary = status_summary(status);
        self.mutate_ledger_sync(move |ledger| {
            let now = now_rfc3339();
            let parent = ensure_thread_entry(ledger, orchestrator_thread_id, &now);
            if let Some(session) = parent
                .watched_sessions
                .iter_mut()
                .find(|session| session.thread_id == target_thread_id)
            {
                session.status = watched_status;
                if let Some(summary) = summary.clone() {
                    session.last_event_summary = Some(summary);
                }
                session.updated_at = now.clone();
            } else {
                parent.watched_sessions.push(WatchedSessionState {
                    thread_id: target_thread_id,
                    title: None,
                    cwd: None,
                    status: watched_status,
                    last_event_summary: summary,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    last_instruction_at: None,
                    last_checked_at: None,
                });
            }
            parent.updated_at = now;
        })
    }

    async fn note_timestamp(
        &self,
        parent_thread_id: ThreadId,
        worker_thread_id: ThreadId,
        kind: TimestampKind,
    ) -> std::io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let worker_thread_id = worker_thread_id.to_string();
        self.mutate_ledger_sync(move |ledger| {
            let now = now_rfc3339();
            let parent = ensure_thread_entry(ledger, parent_thread_id, &now);
            if let Some(worker) = parent
                .workers
                .iter_mut()
                .find(|worker| worker.worker_thread_id == worker_thread_id)
            {
                match kind {
                    TimestampKind::Instruction => worker.last_instruction_at = Some(now.clone()),
                    TimestampKind::Check => worker.last_checked_at = Some(now.clone()),
                }
                worker.updated_at = now.clone();
            }
            parent.updated_at = now;
        })
    }

    async fn note_watched_session_timestamp(
        &self,
        orchestrator_thread_id: ThreadId,
        target_thread_id: ThreadId,
        kind: TimestampKind,
    ) -> std::io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let target_thread_id = target_thread_id.to_string();
        self.mutate_ledger_sync(move |ledger| {
            let now = now_rfc3339();
            let parent = ensure_thread_entry(ledger, orchestrator_thread_id, &now);
            if let Some(session) = parent
                .watched_sessions
                .iter_mut()
                .find(|session| session.thread_id == target_thread_id)
            {
                match kind {
                    TimestampKind::Instruction => session.last_instruction_at = Some(now.clone()),
                    TimestampKind::Check => session.last_checked_at = Some(now.clone()),
                }
                session.updated_at = now.clone();
            }
            parent.updated_at = now;
        })
    }

    pub(crate) async fn build_developer_instructions(
        &self,
        thread_id: ThreadId,
        escalation: &OrchestratorEscalationConfig,
    ) -> Option<String> {
        let ledger = self.read_ledger().await.ok()?;
        let thread = ledger
            .threads
            .iter()
            .find(|thread| thread.thread_id == thread_id.to_string())?;
        let worker_summary = render_worker_summary(thread);
        let escalation_mode = format!("{:?}", escalation.mode).to_lowercase();
        let escalation_channel = escalation.channel.clone().unwrap_or_default();
        let escalation_tool = escalation.tool.clone().unwrap_or_default();
        ORCHESTRATOR_SUPERVISION_TEMPLATE
            .render([
                (
                    "supervision_root",
                    self.root().display().to_string().as_str(),
                ),
                ("worker_summary", worker_summary.as_str()),
                ("escalation_mode", escalation_mode.as_str()),
                ("escalation_channel", escalation_channel.as_str()),
                ("escalation_tool", escalation_tool.as_str()),
            ])
            .ok()
    }

    pub async fn poll_state(
        &self,
        thread_id: ThreadId,
    ) -> std::io::Result<OrchestratorSupervisionPollState> {
        let ledger = self.read_ledger().await?;
        let Some(thread) = ledger
            .threads
            .iter()
            .find(|thread| thread.thread_id == thread_id.to_string())
        else {
            return Ok(OrchestratorSupervisionPollState {
                last_updated_at: None,
                has_supervised_workers: false,
                has_nonterminal_workers: false,
            });
        };

        Ok(OrchestratorSupervisionPollState {
            last_updated_at: Some(thread.updated_at.clone()),
            has_supervised_workers: !thread.workers.is_empty()
                || !thread.watched_sessions.is_empty(),
            has_nonterminal_workers: thread
                .workers
                .iter()
                .any(|worker| !worker.status.is_terminal())
                || thread
                    .watched_sessions
                    .iter()
                    .any(|session| !session.status.is_terminal()),
        })
    }

    pub(crate) fn spawn_status_watcher(
        &self,
        parent_thread_id: ThreadId,
        worker_thread_id: ThreadId,
        mut status_rx: Receiver<AgentStatus>,
    ) {
        let store = self.clone();
        tokio::spawn(async move {
            let mut current_status = status_rx.borrow_and_update().clone();
            if let Err(err) = store
                .note_status(parent_thread_id, worker_thread_id, &current_status)
                .await
            {
                warn!("failed recording initial orchestrator supervision status: {err}");
                return;
            }
            while status_rx.changed().await.is_ok() {
                current_status = status_rx.borrow_and_update().clone();
                if let Err(err) = store
                    .note_status(parent_thread_id, worker_thread_id, &current_status)
                    .await
                {
                    warn!("failed recording orchestrator supervision status update: {err}");
                    return;
                }
            }
        });
    }

    async fn read_ledger(&self) -> std::io::Result<SupervisionLedger> {
        match fs::read_to_string(self.state_path()).await {
            Ok(raw) => match serde_json::from_str::<SupervisionLedger>(&raw) {
                Ok(ledger) => Ok(ledger),
                Err(err) => {
                    warn!("failed parsing orchestrator supervision ledger: {err}");
                    Ok(SupervisionLedger::default())
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(SupervisionLedger::default())
            }
            Err(err) => Err(err),
        }
    }

    fn mutate_ledger_sync(
        &self,
        update: impl FnOnce(&mut SupervisionLedger),
    ) -> std::io::Result<()> {
        self.ensure_layout_sync()?;
        let mut ledger = self.read_ledger_sync()?;
        update(&mut ledger);
        self.write_ledger_sync(&ledger)
    }

    fn ensure_layout_sync(&self) -> std::io::Result<()> {
        std_fs::create_dir_all(self.root())
    }

    fn read_ledger_sync(&self) -> std::io::Result<SupervisionLedger> {
        match std_fs::read_to_string(self.state_path()) {
            Ok(raw) => match serde_json::from_str::<SupervisionLedger>(&raw) {
                Ok(ledger) => Ok(ledger),
                Err(err) => {
                    warn!("failed parsing orchestrator supervision ledger: {err}");
                    Ok(SupervisionLedger::default())
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(SupervisionLedger::default())
            }
            Err(err) => Err(err),
        }
    }

    fn write_ledger_sync(&self, ledger: &SupervisionLedger) -> std::io::Result<()> {
        let raw = serde_json::to_string_pretty(ledger)
            .map_err(|err| std::io::Error::other(format!("serialize supervision ledger: {err}")))?;
        std_fs::write(self.state_path(), raw)
    }
}

fn ensure_thread_entry<'a>(
    ledger: &'a mut SupervisionLedger,
    parent_thread_id: ThreadId,
    now: &str,
) -> &'a mut SupervisedThreadState {
    let thread_id = parent_thread_id.to_string();
    if let Some(index) = ledger
        .threads
        .iter()
        .position(|thread| thread.thread_id == thread_id)
    {
        return &mut ledger.threads[index];
    }
    ledger.threads.push(SupervisedThreadState {
        thread_id,
        updated_at: now.to_string(),
        workers: Vec::new(),
        watched_sessions: Vec::new(),
    });
    let index = ledger.threads.len() - 1;
    &mut ledger.threads[index]
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn status_summary(status: &AgentStatus) -> Option<String> {
    match status {
        AgentStatus::Completed(Some(message)) if !message.trim().is_empty() => {
            Some(message.trim().to_string())
        }
        AgentStatus::Errored(message) if !message.trim().is_empty() => {
            Some(message.trim().to_string())
        }
        _ => None,
    }
}

fn render_worker_summary(thread: &SupervisedThreadState) -> String {
    let mut lines = Vec::new();
    if thread.workers.is_empty() && thread.watched_sessions.is_empty() {
        return "- No supervised workers or watched sessions are currently recorded for this thread.\n"
                .to_string();
    }

    if thread.workers.is_empty() {
        lines.push("- No supervised workers are currently recorded for this thread.".to_string());
    }

    for worker in &thread.workers {
        let header = format!(
            "- {} ({}) status={} updated_at={}",
            worker
                .nickname
                .as_deref()
                .unwrap_or(worker.worker_thread_id.as_str()),
            worker.role.as_deref().unwrap_or("worker"),
            worker.status.as_str(),
            worker.updated_at
        );
        lines.push(header);
        lines.push(format!("  prompt: {}", worker.prompt_preview));
        if let Some(summary) = worker.last_status_summary.as_deref() {
            lines.push(format!("  latest_summary: {summary}"));
        }
        if let Some(last_instruction_at) = worker.last_instruction_at.as_deref() {
            lines.push(format!("  last_instruction_at: {last_instruction_at}"));
        }
        if let Some(last_checked_at) = worker.last_checked_at.as_deref() {
            lines.push(format!("  last_checked_at: {last_checked_at}"));
        }
    }
    if !thread.watched_sessions.is_empty() {
        lines.push("- Watched sessions:".to_string());
    }
    for session in &thread.watched_sessions {
        let header = format!(
            "  - {} status={} updated_at={}",
            session
                .title
                .as_deref()
                .unwrap_or(session.thread_id.as_str()),
            session.status.as_str(),
            session.updated_at
        );
        lines.push(header);
        lines.push(format!("    thread_id: {}", session.thread_id));
        if let Some(cwd) = session.cwd.as_deref() {
            lines.push(format!("    cwd: {cwd}"));
        }
        if let Some(summary) = session.last_event_summary.as_deref() {
            lines.push(format!("    latest_summary: {summary}"));
        }
        if let Some(last_instruction_at) = session.last_instruction_at.as_deref() {
            lines.push(format!("    last_instruction_at: {last_instruction_at}"));
        }
        if let Some(last_checked_at) = session.last_checked_at.as_deref() {
            lines.push(format!("    last_checked_at: {last_checked_at}"));
        }
    }
    format!("{}\n", lines.join("\n"))
}

impl SupervisedWorkerStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::PendingInit => "pending_init",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Errored => "errored",
            Self::Shutdown => "shutdown",
            Self::NotFound => "not_found",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Interrupted | Self::Errored | Self::Shutdown | Self::NotFound
        )
    }
}

impl WatchedSessionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Watched => "watched",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Errored => "errored",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted | Self::Errored)
    }
}

impl From<&AgentStatus> for WatchedSessionStatus {
    fn from(value: &AgentStatus) -> Self {
        match value {
            AgentStatus::Completed(_) => Self::Completed,
            AgentStatus::Interrupted | AgentStatus::Shutdown | AgentStatus::NotFound => {
                Self::Interrupted
            }
            AgentStatus::Errored(_) => Self::Errored,
            AgentStatus::PendingInit | AgentStatus::Running => Self::Watched,
        }
    }
}

impl From<&AgentStatus> for SupervisedWorkerStatus {
    fn from(value: &AgentStatus) -> Self {
        match value {
            AgentStatus::PendingInit => Self::PendingInit,
            AgentStatus::Running => Self::Running,
            AgentStatus::Completed(_) => Self::Completed,
            AgentStatus::Interrupted => Self::Interrupted,
            AgentStatus::Errored(_) => Self::Errored,
            AgentStatus::Shutdown => Self::Shutdown,
            AgentStatus::NotFound => Self::NotFound,
        }
    }
}

enum TimestampKind {
    Instruction,
    Check,
}

#[cfg(test)]
#[path = "orchestrator_supervision_tests.rs"]
mod tests;
