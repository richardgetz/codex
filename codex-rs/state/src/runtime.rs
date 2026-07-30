use crate::LogEntry;
use crate::LogQuery;
use crate::LogRow;
use crate::SortKey;
use crate::SqliteConfig;
use crate::ThreadMetadata;
use crate::ThreadMetadataBuilder;
use crate::ThreadsPage;
use crate::apply_rollout_item;
use crate::migrations::runtime_goals_migrator;
use crate::migrations::runtime_logs_migrator;
use crate::migrations::runtime_memories_migrator;
use crate::migrations::runtime_state_migrator;
use crate::migrations::runtime_thread_history_migrator;
use crate::model::ThreadRow;
use crate::model::anchor_from_item;
use crate::model::datetime_to_epoch_millis;
use crate::model::datetime_to_epoch_seconds;
use crate::model::epoch_millis_to_datetime;
use crate::paths::file_modified_time_utc;
use crate::telemetry::DbKind;
use crate::telemetry::DbTelemetry;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use serde_json::Value;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::Instant;
use tracing::warn;

mod backfill;
mod external_agent_config_imports;
mod goals;
mod logs;
mod memories;
mod recovery;
mod remote_control;
#[cfg(test)]
pub(crate) mod test_support;
mod thread_control;
mod thread_inbound_messages;
mod threads;

pub use external_agent_config_imports::ExternalAgentConfigImportDetailsRecord;
pub use external_agent_config_imports::ExternalAgentConfigImportFailureRecord;
pub use external_agent_config_imports::ExternalAgentConfigImportHistoryRecord;
pub use external_agent_config_imports::ExternalAgentConfigImportSuccessRecord;
pub use goals::GoalAccountingMode;
pub use goals::GoalAccountingOutcome;
pub use goals::GoalStore;
pub use goals::GoalUpdate;
pub use memories::MemoryStore;
pub use recovery::RuntimeDbBackup;
pub(super) use recovery::RuntimeDbInitError;
pub use recovery::backup_runtime_db_for_fresh_start;
pub use recovery::is_sqlite_corruption_error;
pub use recovery::runtime_db_path_for_corruption_error;
pub use recovery::sqlite_error_detail_is_corruption;
pub use recovery::sqlite_error_detail_is_lock;
pub use remote_control::RemoteControlEnrollmentRecord;
pub use threads::ThreadFilterOptions;

// "Partition" is the retained-log-content bucket we cap at 10 MiB:
// - one bucket per non-null thread_id
// - one bucket per threadless (thread_id IS NULL) non-null process_uuid
// - one bucket for threadless rows with process_uuid IS NULL
// This budget tracks each row's persisted rendered log body plus non-body
// metadata, rather than the exact sum of all persisted SQLite column bytes.
const LOG_PARTITION_SIZE_LIMIT_BYTES: i64 = 10 * 1024 * 1024;
const LOG_PARTITION_ROW_LIMIT: i64 = 1_000;

#[derive(Clone)]
pub struct StateRuntime {
    sqlite: SqliteConfig,
    default_provider: String,
    pool: Arc<sqlx::SqlitePool>,
    logs_pool: Arc<sqlx::SqlitePool>,
    thread_goals: GoalStore,
    memories: MemoryStore,
    thread_updated_at_millis: Arc<AtomicI64>,
    thread_recency_at_millis: Arc<AtomicI64>,
}

impl StateRuntime {
    /// Initialize the state runtime using the provided SQLite configuration and default provider.
    ///
    /// This opens (and migrates) the SQLite databases under the configured
    /// `sqlite_home`.
    /// Logs and paginated thread history live in dedicated files to reduce
    /// lock contention with the rest of the state store.
    pub async fn init(sqlite: SqliteConfig, default_provider: String) -> anyhow::Result<Arc<Self>> {
        Self::init_inner(sqlite, default_provider, /*telemetry_override*/ None).await
    }

    #[cfg(test)]
    pub(crate) async fn init_with_telemetry_for_tests(
        sqlite: SqliteConfig,
        default_provider: String,
        telemetry_override: &dyn DbTelemetry,
    ) -> anyhow::Result<Arc<Self>> {
        Self::init_inner(sqlite, default_provider, Some(telemetry_override)).await
    }

    async fn init_inner(
        sqlite: SqliteConfig,
        default_provider: String,
        telemetry_override: Option<&dyn DbTelemetry>,
    ) -> anyhow::Result<Arc<Self>> {
        tokio::fs::create_dir_all(sqlite.home()).await?;
        let state_migrator = runtime_state_migrator();
        let logs_migrator = runtime_logs_migrator();
        let goals_migrator = runtime_goals_migrator();
        let memories_migrator = runtime_memories_migrator();
        let state_path = sqlite.state_db_path();
        let logs_path = sqlite.logs_db_path();
        let goals_path = sqlite.goals_db_path();
        let memories_path = sqlite.memories_db_path();
        let pool = match sqlite
            .open_state_db(&state_migrator, telemetry_override)
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open state db at {}: {err}", state_path.display());
                return Err(err);
            }
        };
        let logs_pool = match sqlite
            .open_logs_db(&logs_migrator, telemetry_override)
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open logs db at {}: {err}", logs_path.display());
                close_sqlite_pools(&[pool.as_ref()]).await;
                return Err(err);
            }
        };
        let goals_pool = match sqlite
            .open_goals_db(&goals_migrator, telemetry_override)
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open goals db at {}: {err}", goals_path.display());
                close_sqlite_pools(&[pool.as_ref(), logs_pool.as_ref()]).await;
                return Err(err);
            }
        };
        if let Err(err) = backfill_legacy_thread_goals(&sqlite, &state_path, &goals_path).await {
            warn!(
                "failed to backfill thread goals from {} to {}: {err}",
                state_path.display(),
                goals_path.display(),
            );
            close_sqlite_pools(&[pool.as_ref(), logs_pool.as_ref(), goals_pool.as_ref()]).await;
            return Err(err);
        }
        let memories_pool = match sqlite
            .open_memories_db(&memories_migrator, telemetry_override)
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!(
                    "failed to open memories db at {}: {err}",
                    memories_path.display()
                );
                close_sqlite_pools(&[pool.as_ref(), logs_pool.as_ref(), goals_pool.as_ref()]).await;
                return Err(err);
            }
        };
        if let Err(err) = backfill_legacy_memory_tables(&sqlite, &state_path, &memories_path).await
        {
            warn!(
                "failed to backfill memory data from {} to {}: {err}",
                state_path.display(),
                memories_path.display(),
            );
            close_sqlite_pools(&[
                pool.as_ref(),
                logs_pool.as_ref(),
                goals_pool.as_ref(),
                memories_pool.as_ref(),
            ])
            .await;
            return Err(err);
        }
        let started = Instant::now();
        let backfill_state_result = ensure_backfill_state_row_in_pool(pool.as_ref()).await;
        crate::telemetry::record_init_result(
            telemetry_override,
            DbKind::State,
            "ensure_backfill_state",
            started.elapsed(),
            &backfill_state_result,
        );
        if let Err(err) = backfill_state_result {
            close_sqlite_pools(&[
                pool.as_ref(),
                logs_pool.as_ref(),
                goals_pool.as_ref(),
                memories_pool.as_ref(),
            ])
            .await;
            return Err(err);
        }
        let started = Instant::now();
        let thread_timestamp_millis_result: anyhow::Result<(Option<i64>, Option<i64>)> =
            sqlx::query_as(
                "SELECT MAX(threads.updated_at_ms), MAX(threads.recency_at_ms) FROM threads",
            )
            .fetch_one(pool.as_ref())
            .await
            .map_err(anyhow::Error::from);
        crate::telemetry::record_init_result(
            telemetry_override,
            DbKind::State,
            "post_init_query",
            started.elapsed(),
            &thread_timestamp_millis_result,
        );
        let (thread_updated_at_millis, thread_recency_at_millis) =
            match thread_timestamp_millis_result {
                Ok(value) => value,
                Err(err) => {
                    close_sqlite_pools(&[
                        pool.as_ref(),
                        logs_pool.as_ref(),
                        goals_pool.as_ref(),
                        memories_pool.as_ref(),
                    ])
                    .await;
                    return Err(err);
                }
            };
        let thread_updated_at_millis = thread_updated_at_millis.unwrap_or(0);
        let thread_recency_at_millis = thread_recency_at_millis.unwrap_or(0);
        let runtime = Arc::new(Self {
            thread_goals: GoalStore::new(Arc::clone(&goals_pool)),
            memories: MemoryStore::new(Arc::clone(&memories_pool), Arc::clone(&pool)),
            pool,
            logs_pool,
            sqlite,
            default_provider,
            thread_updated_at_millis: Arc::new(AtomicI64::new(thread_updated_at_millis)),
            thread_recency_at_millis: Arc::new(AtomicI64::new(thread_recency_at_millis)),
        });
        if let Err(err) = runtime.run_logs_startup_maintenance().await {
            warn!(
                "failed to run startup maintenance for logs db at {}: {err}",
                logs_path.display(),
            );
        }
        Ok(runtime)
    }

    /// Return the SQLite configuration for this runtime.
    pub fn sqlite(&self) -> &SqliteConfig {
        &self.sqlite
    }

    pub fn thread_goals(&self) -> &GoalStore {
        &self.thread_goals
    }

    pub fn memories(&self) -> &MemoryStore {
        &self.memories
    }

    pub async fn clear_memory_data(&self) -> anyhow::Result<()> {
        self.memories.clear_memory_data().await?;
        clear_legacy_memory_data_in_state_db(&self.sqlite, &self.sqlite.state_db_path()).await?;
        Ok(())
    }

    /// Close all SQLite pools and wait for outstanding pool workers to exit.
    pub async fn close(&self) {
        self.memories.close().await;
        self.thread_goals.close().await;
        self.logs_pool.close().await;
        self.pool.close().await;
    }

    pub async fn clear_memory_data_in_sqlite_home(sqlite: &SqliteConfig) -> anyhow::Result<bool> {
        let memories_path = sqlite.memories_db_path();
        let state_path = sqlite.state_db_path();
        if !tokio::fs::try_exists(&memories_path).await? {
            return clear_legacy_memory_data_in_state_db(sqlite, &state_path).await;
        }

        let memories_migrator = runtime_memories_migrator();
        let pool = sqlite
            .open_memories_db(&memories_migrator, /*telemetry_override*/ None)
            .await?;
        memories::clear_memory_data_in_pool(&pool).await?;
        pool.close().await;
        clear_legacy_memory_data_in_state_db(sqlite, &state_path).await?;
        Ok(true)
    }
}

async fn close_sqlite_pools(pools: &[&SqlitePool]) {
    for pool in pools {
        pool.close().await;
    }
}

async fn open_state_sqlite(
    sqlite: &SqliteConfig,
    path: &Path,
    migrator: &Migrator,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    debug_assert_eq!(path, sqlite.state_db_path());
    sqlite.open_state_db(migrator, telemetry_override).await
}

async fn open_logs_sqlite(
    sqlite: &SqliteConfig,
    path: &Path,
    migrator: &Migrator,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    debug_assert_eq!(path, sqlite.logs_db_path());
    sqlite.open_logs_db(migrator, telemetry_override).await
}

async fn open_goals_sqlite(
    sqlite: &SqliteConfig,
    path: &Path,
    migrator: &Migrator,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    debug_assert_eq!(path, sqlite.goals_db_path());
    sqlite.open_goals_db(migrator, telemetry_override).await
}

async fn backfill_legacy_thread_goals(
    sqlite: &SqliteConfig,
    state_path: &Path,
    goals_path: &Path,
) -> anyhow::Result<()> {
    if !state_path.exists() {
        return Ok(());
    }

    let mut goals_conn = sqlite.open_read_write_connection(goals_path).await?;
    let state_path = state_path.to_string_lossy().replace('\'', "''");
    let attach_sql = format!("ATTACH DATABASE '{state_path}' AS legacy_state");
    sqlx::query(sqlx::AssertSqlSafe(attach_sql))
        .execute(&mut goals_conn)
        .await?;

    let backfill_result = async {
        let legacy_table_exists = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM legacy_state.sqlite_master WHERE type = 'table' AND name = 'thread_goals'",
        )
        .fetch_one(&mut goals_conn)
        .await?
            > 0;
        if !legacy_table_exists {
            return Ok(());
        }

        let existing_goal_rows =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM thread_goals")
                .fetch_one(&mut goals_conn)
                .await?;
        if existing_goal_rows > 0 {
            return Ok(());
        }

        sqlx::query(
            r#"
INSERT OR IGNORE INTO thread_goals (
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
)
SELECT
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
FROM legacy_state.thread_goals
            "#,
        )
        .execute(&mut goals_conn)
        .await?;

        Ok::<(), anyhow::Error>(())
    }
    .await;

    let detach_result = sqlx::query("DETACH DATABASE legacy_state")
        .execute(&mut goals_conn)
        .await;
    backfill_result?;
    detach_result?;
    Ok(())
}

async fn backfill_legacy_memory_tables(
    sqlite: &SqliteConfig,
    state_path: &Path,
    memories_path: &Path,
) -> anyhow::Result<()> {
    if !state_path.exists() {
        return Ok(());
    }

    let mut memories_conn = sqlite.open_read_write_connection(memories_path).await?;
    let state_path = state_path.to_string_lossy().replace('\'', "''");
    let attach_sql = format!("ATTACH DATABASE '{state_path}' AS legacy_state");
    sqlx::query(sqlx::AssertSqlSafe(attach_sql))
        .execute(&mut memories_conn)
        .await?;

    let backfill_result = async {
        let legacy_stage1_exists = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM legacy_state.sqlite_master WHERE type = 'table' AND name = 'stage1_outputs'",
        )
        .fetch_one(&mut memories_conn)
        .await?
            > 0;
        if legacy_stage1_exists {
            let stage1_columns =
                legacy_table_columns(&mut memories_conn, "stage1_outputs").await?;
            let rollout_slug = optional_legacy_column(&stage1_columns, "rollout_slug", "NULL");
            let usage_count = optional_legacy_column(&stage1_columns, "usage_count", "NULL");
            let last_usage = optional_legacy_column(&stage1_columns, "last_usage", "NULL");
            let selected_for_phase2 =
                optional_legacy_column(&stage1_columns, "selected_for_phase2", "0");
            let selected_for_phase2_source_updated_at = optional_legacy_column(
                &stage1_columns,
                "selected_for_phase2_source_updated_at",
                "NULL",
            );
            let backfill_stage1_sql = format!(
                r#"
INSERT OR IGNORE INTO stage1_outputs (
    thread_id,
    source_updated_at,
    raw_memory,
    rollout_summary,
    rollout_slug,
    generated_at,
    usage_count,
    last_usage,
    selected_for_phase2,
    selected_for_phase2_source_updated_at
)
SELECT
    thread_id,
    source_updated_at,
    raw_memory,
    rollout_summary,
    {rollout_slug},
    generated_at,
    {usage_count},
    {last_usage},
    {selected_for_phase2},
    {selected_for_phase2_source_updated_at}
FROM legacy_state.stage1_outputs
                "#
            );
            sqlx::query(sqlx::AssertSqlSafe(backfill_stage1_sql))
            .execute(&mut memories_conn)
            .await?;
        }

        let legacy_jobs_exists = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM legacy_state.sqlite_master WHERE type = 'table' AND name = 'jobs'",
        )
        .fetch_one(&mut memories_conn)
        .await?
            > 0;
        if legacy_jobs_exists {
            let job_columns = legacy_table_columns(&mut memories_conn, "jobs").await?;
            let worker_id = optional_legacy_column(&job_columns, "worker_id", "NULL");
            let ownership_token = optional_legacy_column(&job_columns, "ownership_token", "NULL");
            let started_at = optional_legacy_column(&job_columns, "started_at", "NULL");
            let finished_at = optional_legacy_column(&job_columns, "finished_at", "NULL");
            let lease_until = optional_legacy_column(&job_columns, "lease_until", "NULL");
            let retry_at = optional_legacy_column(&job_columns, "retry_at", "NULL");
            let retry_remaining = optional_legacy_column(&job_columns, "retry_remaining", "0");
            let last_error = optional_legacy_column(&job_columns, "last_error", "NULL");
            let input_watermark = optional_legacy_column(&job_columns, "input_watermark", "NULL");
            let last_success_watermark =
                optional_legacy_column(&job_columns, "last_success_watermark", "NULL");
            let backfill_jobs_sql = format!(
                r#"
INSERT OR IGNORE INTO jobs (
    kind,
    job_key,
    status,
    worker_id,
    ownership_token,
    started_at,
    finished_at,
    lease_until,
    retry_at,
    retry_remaining,
    last_error,
    input_watermark,
    last_success_watermark
)
SELECT
    kind,
    job_key,
    status,
    {worker_id},
    {ownership_token},
    {started_at},
    {finished_at},
    {lease_until},
    {retry_at},
    {retry_remaining},
    {last_error},
    {input_watermark},
    {last_success_watermark}
FROM legacy_state.jobs
                "#
            );
            sqlx::query(sqlx::AssertSqlSafe(backfill_jobs_sql))
            .execute(&mut memories_conn)
            .await?;
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    let detach_result = sqlx::query("DETACH DATABASE legacy_state")
        .execute(&mut memories_conn)
        .await;
    backfill_result?;
    detach_result?;
    Ok(())
}

async fn clear_legacy_memory_data_in_state_db(
    sqlite: &SqliteConfig,
    state_path: &Path,
) -> anyhow::Result<bool> {
    if !state_path.exists() {
        return Ok(false);
    }

    let mut state_conn = sqlite.open_read_write_connection(state_path).await?;
    let mut found_legacy_memory_tables = false;

    if sqlite_table_exists(&mut state_conn, "stage1_outputs").await? {
        found_legacy_memory_tables = true;
        sqlx::query("DELETE FROM stage1_outputs")
            .execute(&mut state_conn)
            .await?;
    }

    if sqlite_table_exists(&mut state_conn, "jobs").await? {
        found_legacy_memory_tables = true;
        sqlx::query(
            r#"
DELETE FROM jobs
WHERE kind = ? OR kind = ?
            "#,
        )
        .bind("memory_stage1")
        .bind("memory_consolidate_global")
        .execute(&mut state_conn)
        .await?;
    }

    Ok(found_legacy_memory_tables)
}

async fn legacy_table_columns(
    conn: &mut SqliteConnection,
    table_name: &str,
) -> anyhow::Result<BTreeSet<String>> {
    let table_name = table_name.replace('\'', "''");
    let pragma_sql = format!("PRAGMA legacy_state.table_info('{table_name}')");
    let rows = sqlx::query(sqlx::AssertSqlSafe(pragma_sql))
        .fetch_all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| row.get::<String, _>("name"))
        .collect())
}

async fn sqlite_table_exists(
    conn: &mut SqliteConnection,
    table_name: &str,
) -> anyhow::Result<bool> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
    )
    .bind(table_name)
    .fetch_one(conn)
    .await?;
    Ok(count > 0)
}

fn optional_legacy_column<'a>(
    columns: &'a BTreeSet<String>,
    column: &'a str,
    fallback: &'a str,
) -> &'a str {
    if columns.contains(column) {
        column
    } else {
        fallback
    }
}

async fn open_memories_sqlite(
    sqlite: &SqliteConfig,
    path: &Path,
    migrator: &Migrator,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    debug_assert_eq!(path, sqlite.memories_db_path());
    sqlite.open_memories_db(migrator, telemetry_override).await
}

/// Open and migrate the rebuildable paginated thread-history database.
pub async fn open_thread_history_db(sqlite: &SqliteConfig) -> anyhow::Result<SqlitePool> {
    let migrator = runtime_thread_history_migrator();
    sqlite
        .open_thread_history_db(&migrator, /*telemetry_override*/ None)
        .await
}

pub(super) async fn ensure_backfill_state_row_in_pool(
    pool: &sqlx::SqlitePool,
) -> anyhow::Result<()> {
    // Eagerly check if the operation would have no effect to avoid blocking waiting for a SQLite
    // writer for no reason in the hot startup path.
    if sqlx::query_scalar::<_, i64>("SELECT 1 FROM backfill_state WHERE id = 1")
        .fetch_optional(pool)
        .await?
        .is_some()
    {
        return Ok(());
    }

    sqlx::query(
        r#"
INSERT INTO backfill_state (id, status, last_watermark, last_success_at, updated_at)
VALUES (?, ?, NULL, NULL, ?)
ON CONFLICT(id) DO NOTHING
            "#,
    )
    .bind(1_i64)
    .bind(crate::BackfillStatus::Pending.as_str())
    .bind(Utc::now().timestamp())
    .execute(pool)
    .await?;
    Ok(())
}

/// Run SQLite's built-in integrity check against an existing database file.
pub async fn sqlite_integrity_check(
    sqlite: &SqliteConfig,
    path: &Path,
) -> anyhow::Result<Vec<String>> {
    let pool = sqlite.open_read_only_pool(path).await?;
    let rows = sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
        .fetch_all(&pool)
        .await?;
    pool.close().await;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::StateRuntime;
    use super::backfill_legacy_memory_tables;
    use super::backfill_legacy_thread_goals;
    use super::open_goals_sqlite;
    use super::open_memories_sqlite;
    use super::runtime_goals_migrator;
    use super::runtime_memories_migrator;
    use super::runtime_state_migrator;
    use super::sqlite_integrity_check;
    use super::test_support::unique_temp_dir;
    use crate::DB_INIT_METRIC;
    use crate::DbTelemetry;
    use crate::migrations::STATE_MIGRATOR;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;
    use sqlx::Row;
    use sqlx::SqlitePool;
    use sqlx::migrate::MigrateError;
    use sqlx::sqlite::SqliteConnectOptions;
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::Mutex;

    #[derive(Debug, PartialEq, Eq)]
    struct JobSnapshot {
        kind: String,
        job_key: String,
        status: String,
        worker_id: Option<String>,
        ownership_token: Option<String>,
        started_at: Option<i64>,
        finished_at: Option<i64>,
        lease_until: Option<i64>,
        retry_at: Option<i64>,
        retry_remaining: i64,
        last_error: Option<String>,
        input_watermark: Option<i64>,
        last_success_watermark: Option<i64>,
    }

    #[derive(Default)]
    struct TestTelemetry {
        counters: Mutex<Vec<MetricEvent>>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct MetricEvent {
        name: String,
        tags: BTreeMap<String, String>,
    }

    impl TestTelemetry {
        fn counters(&self) -> Vec<MetricEvent> {
            self.counters
                .lock()
                .expect("telemetry lock")
                .iter()
                .map(|event| MetricEvent {
                    name: event.name.clone(),
                    tags: event.tags.clone(),
                })
                .collect()
        }
    }

    impl DbTelemetry for TestTelemetry {
        fn counter(&self, name: &str, _inc: i64, tags: &[(&str, &str)]) {
            self.counters
                .lock()
                .expect("telemetry lock")
                .push(MetricEvent {
                    name: name.to_string(),
                    tags: tags_to_map(tags),
                });
        }

        fn record_duration(
            &self,
            _name: &str,
            _duration: std::time::Duration,
            _tags: &[(&str, &str)],
        ) {
        }
    }

    fn tags_to_map(tags: &[(&str, &str)]) -> BTreeMap<String, String> {
        tags.iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    async fn open_db_pool(path: &Path) -> SqlitePool {
        crate::SqliteConfig::new_for_testing(path.parent().unwrap_or(path).abs())
            .open_read_write_pool(path)
            .await
            .expect("open sqlite pool")
    }

    #[tokio::test]
    async fn sqlite_integrity_check_reports_ok_for_valid_db() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex home");
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let path = sqlite.state_db_path();
        let pool = sqlite
            .open_read_write_pool(&path)
            .await
            .expect("open sqlite db");
        sqlx::query("CREATE TABLE sample (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .expect("create sample table");
        pool.close().await;

        let result = sqlite_integrity_check(&sqlite, &path)
            .await
            .expect("integrity check should run");

        assert_eq!(result, vec!["ok".to_string()]);
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn open_state_sqlite_tolerates_newer_applied_migrations() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex home");
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let state_path = sqlite.state_db_path();
        let pool = sqlite
            .open_read_write_pool(&state_path)
            .await
            .expect("open state db");
        STATE_MIGRATOR
            .run(&pool)
            .await
            .expect("apply current state schema");
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(9_999_i64)
        .bind("future migration")
        .bind(true)
        .bind(vec![1_u8, 2, 3, 4])
        .bind(1_i64)
        .execute(&pool)
        .await
        .expect("insert future migration record");
        pool.close().await;

        let strict_pool = open_db_pool(state_path.as_path()).await;
        let strict_err = STATE_MIGRATOR
            .run(&strict_pool)
            .await
            .expect_err("strict migrator should reject newer applied migrations");
        assert!(matches!(strict_err, MigrateError::VersionMissing(9_999)));
        strict_pool.close().await;

        let tolerant_migrator = runtime_state_migrator();
        let tolerant_pool = sqlite
            .open_state_db(&tolerant_migrator, /*telemetry_override*/ None)
            .await
            .expect("runtime migrator should tolerate newer applied migrations");
        tolerant_pool.close().await;

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn backfills_legacy_thread_goals_into_split_goals_db() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex home");
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let state_path = sqlite.state_db_path();
        let goals_path = sqlite.goals_db_path();

        let state_pool = SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .filename(&state_path)
                .create_if_missing(true),
        )
        .await
        .expect("open legacy state db");
        sqlx::query(
            r#"
CREATE TABLE thread_goals (
    thread_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    objective TEXT NOT NULL,
    status TEXT NOT NULL,
    token_budget INTEGER,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    time_used_seconds INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
)
            "#,
        )
        .execute(&state_pool)
        .await
        .expect("create legacy thread_goals");
        sqlx::query(
            r#"
INSERT INTO thread_goals (
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind("thread-1")
        .bind("goal-1")
        .bind("ship stable refresh")
        .bind("active")
        .bind(123_i64)
        .bind(45_i64)
        .bind(6_i64)
        .bind(1_700_000_000_000_i64)
        .bind(1_700_000_001_000_i64)
        .execute(&state_pool)
        .await
        .expect("insert legacy goal");
        state_pool.close().await;

        let goals_pool = open_goals_sqlite(
            &sqlite,
            goals_path.as_path(),
            &runtime_goals_migrator(),
            /*telemetry_override*/ None,
        )
        .await
        .expect("migrate goals db");
        goals_pool.close().await;

        backfill_legacy_thread_goals(&sqlite, state_path.as_path(), goals_path.as_path())
            .await
            .expect("backfill legacy goals");

        let goals_pool = open_db_pool(goals_path.as_path()).await;
        let row = sqlx::query(
            r#"
SELECT
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
FROM thread_goals
            "#,
        )
        .fetch_one(&goals_pool)
        .await
        .expect("fetch copied goal");
        assert_eq!(
            (
                row.get::<String, _>("thread_id"),
                row.get::<String, _>("goal_id"),
                row.get::<String, _>("objective"),
                row.get::<String, _>("status"),
                row.get::<Option<i64>, _>("token_budget"),
                row.get::<i64, _>("tokens_used"),
                row.get::<i64, _>("time_used_seconds"),
                row.get::<i64, _>("created_at_ms"),
                row.get::<i64, _>("updated_at_ms"),
            ),
            (
                "thread-1".to_string(),
                "goal-1".to_string(),
                "ship stable refresh".to_string(),
                "active".to_string(),
                Some(123),
                45,
                6,
                1_700_000_000_000,
                1_700_000_001_000,
            )
        );
        goals_pool.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn backfills_legacy_memory_tables_into_split_memories_db() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex home");
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let state_path = sqlite.state_db_path();
        let memories_path = sqlite.memories_db_path();

        let state_pool = SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .filename(&state_path)
                .create_if_missing(true),
        )
        .await
        .expect("open legacy state db");
        sqlx::query(
            r#"
CREATE TABLE stage1_outputs (
    thread_id TEXT PRIMARY KEY,
    source_updated_at INTEGER NOT NULL,
    raw_memory TEXT NOT NULL,
    rollout_summary TEXT NOT NULL,
    generated_at INTEGER NOT NULL
);
            "#,
        )
        .execute(&state_pool)
        .await
        .expect("create legacy stage1_outputs");
        sqlx::query(
            r#"
CREATE TABLE jobs (
    kind TEXT NOT NULL,
    job_key TEXT NOT NULL,
    status TEXT NOT NULL,
    retry_remaining INTEGER NOT NULL,
    PRIMARY KEY (kind, job_key)
);
            "#,
        )
        .execute(&state_pool)
        .await
        .expect("create legacy jobs");
        sqlx::query(
            r#"
INSERT INTO stage1_outputs (
    thread_id,
    source_updated_at,
    raw_memory,
    rollout_summary,
    generated_at
) VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("thread-memory")
        .bind(10_i64)
        .bind("raw memory")
        .bind("rollout summary")
        .bind(11_i64)
        .execute(&state_pool)
        .await
        .expect("insert legacy stage1 output");
        sqlx::query(
            r#"
INSERT INTO jobs (
    kind,
    job_key,
    status,
    retry_remaining
) VALUES (?, ?, ?, ?)
            "#,
        )
        .bind("memory")
        .bind("thread-memory")
        .bind("done")
        .bind(3_i64)
        .execute(&state_pool)
        .await
        .expect("insert legacy job");
        state_pool.close().await;

        let memories_pool = open_memories_sqlite(
            &sqlite,
            memories_path.as_path(),
            &runtime_memories_migrator(),
            /*telemetry_override*/ None,
        )
        .await
        .expect("migrate memories db");
        memories_pool.close().await;

        backfill_legacy_memory_tables(&sqlite, state_path.as_path(), memories_path.as_path())
            .await
            .expect("backfill legacy memories");

        let memories_pool = open_db_pool(memories_path.as_path()).await;
        let stage1 = sqlx::query(
            r#"
SELECT
    thread_id,
    source_updated_at,
    raw_memory,
    rollout_summary,
    rollout_slug,
    generated_at,
    usage_count,
    last_usage,
    selected_for_phase2,
    selected_for_phase2_source_updated_at
FROM stage1_outputs
            "#,
        )
        .fetch_one(&memories_pool)
        .await
        .expect("fetch copied stage1 output");
        assert_eq!(
            (
                stage1.get::<String, _>("thread_id"),
                stage1.get::<i64, _>("source_updated_at"),
                stage1.get::<String, _>("raw_memory"),
                stage1.get::<String, _>("rollout_summary"),
                stage1.get::<Option<String>, _>("rollout_slug"),
                stage1.get::<i64, _>("generated_at"),
                stage1.get::<Option<i64>, _>("usage_count"),
                stage1.get::<Option<i64>, _>("last_usage"),
                stage1.get::<i64, _>("selected_for_phase2"),
                stage1.get::<Option<i64>, _>("selected_for_phase2_source_updated_at"),
            ),
            (
                "thread-memory".to_string(),
                10,
                "raw memory".to_string(),
                "rollout summary".to_string(),
                None,
                11,
                None,
                None,
                0,
                None,
            )
        );

        let job = sqlx::query(
            r#"
SELECT
    kind,
    job_key,
    status,
    worker_id,
    ownership_token,
    started_at,
    finished_at,
    lease_until,
    retry_at,
    retry_remaining,
    last_error,
    input_watermark,
    last_success_watermark
FROM jobs
            "#,
        )
        .fetch_one(&memories_pool)
        .await
        .expect("fetch copied job");
        assert_eq!(
            JobSnapshot {
                kind: job.get("kind"),
                job_key: job.get("job_key"),
                status: job.get("status"),
                worker_id: job.get("worker_id"),
                ownership_token: job.get("ownership_token"),
                started_at: job.get("started_at"),
                finished_at: job.get("finished_at"),
                lease_until: job.get("lease_until"),
                retry_at: job.get("retry_at"),
                retry_remaining: job.get("retry_remaining"),
                last_error: job.get("last_error"),
                input_watermark: job.get("input_watermark"),
                last_success_watermark: job.get("last_success_watermark"),
            },
            JobSnapshot {
                kind: "memory".to_string(),
                job_key: "thread-memory".to_string(),
                status: "done".to_string(),
                worker_id: None,
                ownership_token: None,
                started_at: None,
                finished_at: None,
                lease_until: None,
                retry_at: None,
                retry_remaining: 3,
                last_error: None,
                input_watermark: None,
                last_success_watermark: None,
            }
        );
        memories_pool.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn clear_memory_data_removes_legacy_state_memory_rows_before_split_db_exists() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex home");
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let state_path = sqlite.state_db_path();

        let state_pool = SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .filename(&state_path)
                .create_if_missing(true),
        )
        .await
        .expect("open legacy state db");
        sqlx::query(
            r#"
CREATE TABLE stage1_outputs (
    thread_id TEXT PRIMARY KEY,
    source_updated_at INTEGER NOT NULL,
    raw_memory TEXT NOT NULL,
    rollout_summary TEXT NOT NULL,
    generated_at INTEGER NOT NULL
);
            "#,
        )
        .execute(&state_pool)
        .await
        .expect("create legacy stage1_outputs");
        sqlx::query(
            r#"
CREATE TABLE jobs (
    kind TEXT NOT NULL,
    job_key TEXT NOT NULL,
    status TEXT NOT NULL,
    retry_remaining INTEGER NOT NULL,
    PRIMARY KEY (kind, job_key)
);
            "#,
        )
        .execute(&state_pool)
        .await
        .expect("create legacy jobs");
        sqlx::query(
            "INSERT INTO stage1_outputs (thread_id, source_updated_at, raw_memory, rollout_summary, generated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("thread-memory")
        .bind(10_i64)
        .bind("raw memory")
        .bind("rollout summary")
        .bind(11_i64)
        .execute(&state_pool)
        .await
        .expect("insert legacy stage1 output");
        sqlx::query(
            "INSERT INTO jobs (kind, job_key, status, retry_remaining) VALUES (?, ?, ?, ?), (?, ?, ?, ?)",
        )
        .bind("memory_stage1")
        .bind("memory-job")
        .bind("done")
        .bind(0_i64)
        .bind("other")
        .bind("other-job")
        .bind("done")
        .bind(0_i64)
        .execute(&state_pool)
        .await
        .expect("insert legacy jobs");
        state_pool.close().await;

        let cleared = StateRuntime::clear_memory_data_in_sqlite_home(&sqlite)
            .await
            .expect("clear legacy memory data");
        assert!(cleared);

        let state_pool = open_db_pool(state_path.as_path()).await;
        let counts = sqlx::query(
            r#"
SELECT
    (SELECT COUNT(*) FROM stage1_outputs) AS stage1_count,
    (SELECT COUNT(*) FROM jobs WHERE kind = 'memory_stage1') AS memory_job_count,
    (SELECT COUNT(*) FROM jobs WHERE kind = 'other') AS other_job_count
            "#,
        )
        .fetch_one(&state_pool)
        .await
        .expect("fetch legacy memory counts");
        assert_eq!(
            (
                counts.get::<i64, _>("stage1_count"),
                counts.get::<i64, _>("memory_job_count"),
                counts.get::<i64, _>("other_job_count"),
            ),
            (0, 0, 1)
        );
        state_pool.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn init_records_successful_sqlite_init_phases_to_explicit_telemetry() {
        let codex_home = unique_temp_dir();
        let telemetry = TestTelemetry::default();

        let runtime = StateRuntime::init_with_telemetry_for_tests(
            crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
            "test-provider".to_string(),
            &telemetry,
        )
        .await
        .expect("state runtime should initialize");

        let phases = telemetry
            .counters()
            .into_iter()
            .filter(|event| event.name == DB_INIT_METRIC)
            .filter(|event| event.tags.get("status").map(String::as_str) == Some("success"))
            .filter_map(|event| event.tags.get("phase").cloned())
            .collect::<BTreeSet<_>>();
        let expected = [
            "open_state",
            "migrate_state",
            "open_logs",
            "migrate_logs",
            "open_goals",
            "migrate_goals",
            "open_memories",
            "migrate_memories",
            "ensure_backfill_state",
            "post_init_query",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
        assert_eq!(phases, expected);

        runtime.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }
}
