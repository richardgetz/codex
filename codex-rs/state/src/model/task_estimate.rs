use super::epoch_seconds_to_datetime;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Lifecycle state for a user-visible ETA task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEstimateStatus {
    Pending,
    Active,
    Blocked,
    Completed,
    Cancelled,
}

impl TaskEstimateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

impl TryFrom<&str> for TaskEstimateStatus {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "active" => Ok(Self::Active),
            "blocked" => Ok(Self::Blocked),
            "completed" => Ok(Self::Completed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(anyhow::anyhow!("unknown task estimate status `{other}`")),
        }
    }
}

/// A lower/upper wall-clock duration estimate in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEstimateRange {
    pub lower_seconds: Option<i64>,
    pub upper_seconds: Option<i64>,
}

// Keep aggregate finish timestamps inside chrono's supported range even when a caller supplies
// the largest value representable by an integer. With at most 256 tasks per root, this also keeps
// a serial dependency path well below the DateTime horizon.
pub(crate) const MAX_ESTIMATE_SECONDS: i64 = 100_i64 * 365 * 24 * 60 * 60;

impl TaskEstimateRange {
    pub fn is_known(self) -> bool {
        self.lower_seconds.is_some() && self.upper_seconds.is_some()
    }

    pub fn validate(self) -> Result<()> {
        if self.lower_seconds.is_some_and(|value| value < 0)
            || self.upper_seconds.is_some_and(|value| value < 0)
        {
            return Err(anyhow::anyhow!(
                "task duration estimates must not be negative"
            ));
        }
        if self
            .lower_seconds
            .is_some_and(|value| value > MAX_ESTIMATE_SECONDS)
            || self
                .upper_seconds
                .is_some_and(|value| value > MAX_ESTIMATE_SECONDS)
        {
            return Err(anyhow::anyhow!(
                "task duration estimates must not exceed {MAX_ESTIMATE_SECONDS} seconds"
            ));
        }
        if let (Some(lower), Some(upper)) = (self.lower_seconds, self.upper_seconds)
            && lower > upper
        {
            return Err(anyhow::anyhow!(
                "task duration estimate lower bound must not exceed upper bound"
            ));
        }
        Ok(())
    }
}

/// A bounded estimate revision retained for task history and accuracy display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEstimateRevision {
    pub lower_seconds: Option<i64>,
    pub upper_seconds: Option<i64>,
    pub reason: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub actor_thread_id: ThreadId,
}

/// A persisted task estimate owned by one root agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEstimate {
    pub task_id: String,
    pub root_thread_id: ThreadId,
    pub owner_thread_id: ThreadId,
    pub parent_task_id: Option<String>,
    pub depends_on_task_ids: Vec<String>,
    pub title: String,
    pub status: TaskEstimateStatus,
    pub current_lower_seconds: Option<i64>,
    pub current_upper_seconds: Option<i64>,
    pub original_lower_seconds: Option<i64>,
    pub original_upper_seconds: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub terminal_at: Option<DateTime<Utc>>,
    pub actual_elapsed_seconds: Option<i64>,
    pub updated_at: DateTime<Utc>,
    pub revisions: Vec<TaskEstimateRevision>,
}

impl TaskEstimate {
    pub fn current_range(&self) -> TaskEstimateRange {
        TaskEstimateRange {
            lower_seconds: self.current_lower_seconds,
            upper_seconds: self.current_upper_seconds,
        }
    }

    pub fn original_range(&self) -> TaskEstimateRange {
        TaskEstimateRange {
            lower_seconds: self.original_lower_seconds,
            upper_seconds: self.original_upper_seconds,
        }
    }

    /// Return the one-shot freshness delay for this task, measured from its latest durable
    /// update. The configured minimum is always honored; a known upper estimate extends the
    /// delay to one quarter of that estimate, rounded up so fractional seconds never wake early.
    pub fn freshness_delay_seconds(&self, freshness_minimum_seconds: i64) -> i64 {
        let minimum = freshness_minimum_seconds.max(0);
        let Some(upper) = self.current_upper_seconds else {
            return minimum;
        };
        let upper = upper.max(0);
        let quarter = upper / 4;
        let quarter = quarter.saturating_add(if upper % 4 == 0 { 0 } else { 1 });
        minimum.max(quarter)
    }

    /// Return the duration remaining from `now` without changing task state.
    pub fn remaining_range(&self, now: DateTime<Utc>) -> TaskEstimateRange {
        let range = self.current_range();
        if self.started_at.is_none() {
            return range;
        }
        // A revision is explicitly a remaining range measured from the update time. Using the
        // row's update timestamp as the baseline keeps a reminder response from restarting the
        // original duration while still allowing reads to age the saved range deterministically.
        let elapsed = (now - self.updated_at).num_seconds().max(0);
        TaskEstimateRange {
            lower_seconds: range
                .lower_seconds
                .map(|value| value.saturating_sub(elapsed).max(0)),
            upper_seconds: range
                .upper_seconds
                .map(|value| value.saturating_sub(elapsed).max(0)),
        }
    }
}

/// The deterministic aggregate ETA for one root session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEstimateOverall {
    pub finish_at: Option<DateTime<Utc>>,
    pub remaining_lower_seconds: Option<i64>,
    pub remaining_upper_seconds: Option<i64>,
    pub unknown_reason: Option<String>,
}

impl TaskEstimateOverall {
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self {
            finish_at: None,
            remaining_lower_seconds: None,
            remaining_upper_seconds: None,
            unknown_reason: Some(reason.into()),
        }
    }
}

/// A bounded snapshot used by app-server reads and update notifications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEstimateSnapshot {
    pub root_thread_id: ThreadId,
    pub generated_at: DateTime<Utc>,
    pub sequence: i64,
    pub active: Vec<TaskEstimate>,
    pub history: Vec<TaskEstimate>,
    pub next_cursor: Option<String>,
    pub overall: TaskEstimateOverall,
}

/// A model or app-server operation against one task. Actor and root ownership are supplied by
/// the caller and never taken from these fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEstimateMutation {
    pub action: TaskEstimateAction,
    pub task_id: Option<String>,
    pub title: Option<String>,
    pub parent_task_id: Option<String>,
    pub depends_on_task_ids: Option<Vec<String>>,
    pub estimate: Option<TaskEstimateRange>,
    pub reason: Option<String>,
    /// Optional owner override. Only the root Lead may assign a persisted descendant.
    pub owner_thread_id: Option<ThreadId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEstimateAction {
    Create,
    Start,
    Revise,
    Block,
    Complete,
    Cancel,
}

/// The changed tasks and aggregate result of one atomic update batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEstimateUpdateResult {
    pub root_thread_id: ThreadId,
    pub generated_at: DateTime<Utc>,
    pub sequence: i64,
    pub changed_tasks: Vec<TaskEstimate>,
    pub overall: TaskEstimateOverall,
}

/// A page of terminal tasks for the History view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEstimateHistoryPage {
    pub tasks: Vec<TaskEstimate>,
    pub next_cursor: Option<String>,
}

pub(crate) struct TaskEstimateRow {
    pub task_id: String,
    pub root_thread_id: String,
    pub owner_thread_id: String,
    pub parent_task_id: Option<String>,
    pub depends_on_task_ids: String,
    pub title: String,
    pub status: String,
    pub current_lower_seconds: Option<i64>,
    pub current_upper_seconds: Option<i64>,
    pub original_lower_seconds: Option<i64>,
    pub original_upper_seconds: Option<i64>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub terminal_at: Option<i64>,
    pub actual_elapsed_seconds: Option<i64>,
    pub updated_at: i64,
    pub updated_sequence: i64,
}

impl TaskEstimateRow {
    pub(crate) fn try_from_row(row: &SqliteRow) -> Result<Self> {
        Ok(Self {
            task_id: row.try_get("task_id")?,
            root_thread_id: row.try_get("root_thread_id")?,
            owner_thread_id: row.try_get("owner_thread_id")?,
            parent_task_id: row.try_get("parent_task_id")?,
            depends_on_task_ids: row.try_get("depends_on_task_ids")?,
            title: row.try_get("title")?,
            status: row.try_get("status")?,
            current_lower_seconds: row.try_get("current_lower_seconds")?,
            current_upper_seconds: row.try_get("current_upper_seconds")?,
            original_lower_seconds: row.try_get("original_lower_seconds")?,
            original_upper_seconds: row.try_get("original_upper_seconds")?,
            created_at: row.try_get("created_at")?,
            started_at: row.try_get("started_at")?,
            terminal_at: row.try_get("terminal_at")?,
            actual_elapsed_seconds: row.try_get("actual_elapsed_seconds")?,
            updated_at: row.try_get("updated_at")?,
            updated_sequence: row.try_get("updated_sequence")?,
        })
    }
}

pub(crate) fn task_estimate_from_row(row: TaskEstimateRow) -> Result<TaskEstimate> {
    let depends_on_task_ids = serde_json::from_str(&row.depends_on_task_ids)?;
    Ok(TaskEstimate {
        task_id: row.task_id,
        root_thread_id: ThreadId::from_string(&row.root_thread_id)?,
        owner_thread_id: ThreadId::from_string(&row.owner_thread_id)?,
        parent_task_id: row.parent_task_id,
        depends_on_task_ids,
        title: row.title,
        status: TaskEstimateStatus::try_from(row.status.as_str())?,
        current_lower_seconds: row.current_lower_seconds,
        current_upper_seconds: row.current_upper_seconds,
        original_lower_seconds: row.original_lower_seconds,
        original_upper_seconds: row.original_upper_seconds,
        created_at: epoch_seconds_to_datetime(row.created_at)?,
        started_at: row.started_at.map(epoch_seconds_to_datetime).transpose()?,
        terminal_at: row.terminal_at.map(epoch_seconds_to_datetime).transpose()?,
        actual_elapsed_seconds: row.actual_elapsed_seconds,
        updated_at: epoch_seconds_to_datetime(row.updated_at)?,
        revisions: Vec::new(),
    })
}

pub(crate) fn task_estimate_revision_from_row(row: &SqliteRow) -> Result<TaskEstimateRevision> {
    Ok(TaskEstimateRevision {
        lower_seconds: row.try_get("lower_seconds")?,
        upper_seconds: row.try_get("upper_seconds")?,
        reason: row.try_get("reason")?,
        updated_at: epoch_seconds_to_datetime(row.try_get("updated_at")?)?,
        actor_thread_id: ThreadId::from_string(
            row.try_get::<String, _>("actor_thread_id")?.as_str(),
        )?,
    })
}

pub(crate) fn validate_task_id(task_id: &str) -> Result<()> {
    if task_id.is_empty() || task_id.len() > 128 {
        return Err(anyhow::anyhow!("task id must contain 1 to 128 characters"));
    }
    if task_id.chars().any(char::is_whitespace) {
        return Err(anyhow::anyhow!("task id must not contain whitespace"));
    }
    Ok(())
}

pub(crate) fn validate_task_title(title: &str) -> Result<()> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 512 {
        return Err(anyhow::anyhow!(
            "task title must contain 1 to 512 characters"
        ));
    }
    Ok(())
}

pub(crate) fn validate_task_reason(reason: Option<&str>) -> Result<()> {
    if reason.is_some_and(|reason| reason.chars().count() > 512) {
        return Err(anyhow::anyhow!(
            "task reason must be at most 512 characters"
        ));
    }
    Ok(())
}

pub(crate) fn validate_dependencies(
    task_id: &str,
    parent_task_id: Option<&str>,
    depends_on_task_ids: &[String],
) -> Result<()> {
    validate_task_id(task_id)?;
    if let Some(parent_task_id) = parent_task_id {
        validate_task_id(parent_task_id)?;
        if parent_task_id == task_id {
            return Err(anyhow::anyhow!("a task cannot parent itself"));
        }
    }
    if depends_on_task_ids.len() > 32 {
        return Err(anyhow::anyhow!(
            "a task may depend on at most 32 other tasks"
        ));
    }
    let mut seen = BTreeSet::new();
    for dependency in depends_on_task_ids {
        validate_task_id(dependency)?;
        if dependency == task_id || !seen.insert(dependency) {
            return Err(anyhow::anyhow!(
                "task dependencies must be unique and acyclic"
            ));
        }
    }
    Ok(())
}

/// Detect a dependency cycle in an in-memory root snapshot.
pub(crate) fn validate_dependency_graph(tasks: &BTreeMap<String, TaskEstimate>) -> Result<()> {
    fn visit(
        task_id: &str,
        tasks: &BTreeMap<String, TaskEstimate>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> Result<()> {
        if visited.contains(task_id) {
            return Ok(());
        }
        if !visiting.insert(task_id.to_string()) {
            return Err(anyhow::anyhow!("task dependency graph contains a cycle"));
        }
        let task = tasks.get(task_id).ok_or_else(|| {
            anyhow::anyhow!("task dependency references unknown task `{task_id}`")
        })?;
        for dependency in &task.depends_on_task_ids {
            visit(dependency, tasks, visiting, visited)?;
        }
        visiting.remove(task_id);
        visited.insert(task_id.to_string());
        Ok(())
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for task_id in tasks.keys() {
        visit(task_id, tasks, &mut visiting, &mut visited)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "task_estimate_tests.rs"]
mod tests;
