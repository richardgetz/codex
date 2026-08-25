//! Small local rollups of estimated API spend for the `/spend` command.

use crate::status::estimate_cost_usd_for_usage;
use crate::token_usage::TokenUsage;
use crate::token_usage::TokenUsageInfo;
use chrono::Duration;
use chrono::Local;
use chrono::NaiveDate;
use codex_config::types::DEFAULT_DAILY_SPEND_RETENTION_DAYS;
use codex_config::types::MAX_DAILY_SPEND_RETENTION_DAYS;
use codex_config::types::TuiStatusTokenUsage;
use ratatui::text::Line;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tempfile::NamedTempFile;

const HISTORY_VERSION: u32 = 1;
const SPEND_DIRECTORY: &str = "usage";
const SPEND_FILENAME: &str = "daily_spend.json";
const LOCK_STALE_AFTER: Duration = Duration::minutes(1);

#[path = "daily_spend_report.rs"]
mod daily_spend_report;
use daily_spend_report::render_report_at;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct DailySpendAmount {
    #[serde(default)]
    tokens: i64,
    #[serde(default)]
    estimated_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct DailySpendDay {
    #[serde(default)]
    tokens: i64,
    #[serde(default)]
    estimated_usd: Option<f64>,
    #[serde(default)]
    models: BTreeMap<String, DailySpendAmount>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct DailySpendHistory {
    #[serde(default = "default_history_version")]
    version: u32,
    #[serde(default)]
    days: BTreeMap<String, DailySpendDay>,
}

impl Default for DailySpendHistory {
    fn default() -> Self {
        Self {
            version: HISTORY_VERSION,
            days: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct DailySpendDelta {
    usage: TokenUsage,
    usage_by_service_tier: BTreeMap<String, TokenUsage>,
    usage_by_service_tier_and_context_length: BTreeMap<String, BTreeMap<String, TokenUsage>>,
}

/// Tracks the last cumulative usage event so live notifications can be reduced to one delta.
pub(crate) struct DailySpendTracker {
    path: PathBuf,
    previous_usage: Option<TokenUsageInfo>,
}

impl DailySpendTracker {
    pub(crate) fn new(codex_home: &Path) -> Self {
        Self {
            path: spend_path(codex_home),
            previous_usage: None,
        }
    }

    pub(crate) fn observe(
        &mut self,
        info: &TokenUsageInfo,
        replay: bool,
        config: &TuiStatusTokenUsage,
        model_provider_id: &str,
        current_model: &str,
    ) -> anyhow::Result<()> {
        if replay {
            self.previous_usage = Some(info.clone());
            return Ok(());
        }

        let previous = self.previous_usage.as_ref();
        let deltas = usage_deltas(previous, info, current_model);
        if !deltas.is_empty() {
            record_usage_deltas(
                &self.path,
                config.daily_spend_retention_days,
                Local::now().date_naive(),
                &deltas,
                config,
                model_provider_id,
            )?;
        }
        self.previous_usage = Some(info.clone());
        Ok(())
    }

    pub(crate) fn render_report(&self, args: &str) -> anyhow::Result<Vec<Line<'static>>> {
        render_report_at(&self.path, args, Local::now().date_naive())
    }
}

pub(crate) fn spend_path(codex_home: &Path) -> PathBuf {
    codex_home.join(SPEND_DIRECTORY).join(SPEND_FILENAME)
}

fn usage_deltas(
    previous: Option<&TokenUsageInfo>,
    current: &TokenUsageInfo,
    current_model: &str,
) -> BTreeMap<String, DailySpendDelta> {
    if current.usage_by_model.is_empty() {
        let previous_total = previous.map(|info| &info.total_token_usage);
        let usage = subtract_usage(&current.total_token_usage, previous_total);
        let previous_tiers = previous
            .map(|info| &info.usage_by_service_tier_and_context_length)
            .cloned()
            .unwrap_or_default();
        let nested = subtract_nested_usage(
            &current.usage_by_service_tier_and_context_length,
            &previous_tiers,
        );
        let tiers = aggregate_tier_usage(&nested);
        return if !usage.is_zero() {
            {
                BTreeMap::from([(
                    current_model.to_string(),
                    DailySpendDelta {
                        usage,
                        usage_by_service_tier: tiers,
                        usage_by_service_tier_and_context_length: nested,
                    },
                )])
            }
        } else {
            Default::default()
        };
    }

    let mut deltas = BTreeMap::new();
    for (model, current_usage) in &current.usage_by_model {
        let previous_usage = previous.and_then(|info| info.usage_by_model.get(model));
        let usage = subtract_usage(current_usage, previous_usage);
        if !usage.is_zero() {
            let current_nested = current
                .usage_by_model_and_service_tier_and_context_length
                .get(model)
                .cloned()
                .unwrap_or_default();
            let previous_nested = previous
                .and_then(|info| {
                    info.usage_by_model_and_service_tier_and_context_length
                        .get(model)
                })
                .cloned()
                .unwrap_or_default();
            let nested = subtract_nested_usage(&current_nested, &previous_nested);
            deltas.insert(
                model.clone(),
                DailySpendDelta {
                    usage,
                    usage_by_service_tier: aggregate_tier_usage(&nested),
                    usage_by_service_tier_and_context_length: nested,
                },
            );
        }
    }
    deltas
}

fn subtract_usage(current: &TokenUsage, previous: Option<&TokenUsage>) -> TokenUsage {
    let previous = previous.cloned().unwrap_or_default();
    TokenUsage {
        input_tokens: current.input_tokens.saturating_sub(previous.input_tokens),
        cached_input_tokens: current
            .cached_input_tokens
            .saturating_sub(previous.cached_input_tokens),
        cache_write_tokens: current
            .cache_write_tokens
            .saturating_sub(previous.cache_write_tokens),
        output_tokens: current.output_tokens.saturating_sub(previous.output_tokens),
        reasoning_output_tokens: current
            .reasoning_output_tokens
            .saturating_sub(previous.reasoning_output_tokens),
        total_tokens: current.total_tokens.saturating_sub(previous.total_tokens),
    }
}

fn subtract_nested_usage(
    current: &BTreeMap<String, BTreeMap<String, TokenUsage>>,
    previous: &BTreeMap<String, BTreeMap<String, TokenUsage>>,
) -> BTreeMap<String, BTreeMap<String, TokenUsage>> {
    let mut delta = BTreeMap::new();
    for (service_tier, context_usages) in current {
        let previous_context_usages = previous.get(service_tier);
        let mut context_delta = BTreeMap::new();
        for (context_length, usage) in context_usages {
            let previous_usage =
                previous_context_usages.and_then(|usages| usages.get(context_length));
            let usage = subtract_usage(usage, previous_usage);
            if !usage.is_zero() {
                context_delta.insert(context_length.clone(), usage);
            }
        }
        if !context_delta.is_empty() {
            delta.insert(service_tier.clone(), context_delta);
        }
    }
    delta
}

fn aggregate_tier_usage(
    nested: &BTreeMap<String, BTreeMap<String, TokenUsage>>,
) -> BTreeMap<String, TokenUsage> {
    nested
        .iter()
        .map(|(service_tier, context_usages)| {
            let mut total = TokenUsage::default();
            for usage in context_usages.values() {
                add_usage(&mut total, usage);
            }
            (service_tier.clone(), total)
        })
        .collect()
}

fn add_usage(total: &mut TokenUsage, usage: &TokenUsage) {
    total.input_tokens += usage.input_tokens;
    total.cached_input_tokens += usage.cached_input_tokens;
    total.cache_write_tokens += usage.cache_write_tokens;
    total.output_tokens += usage.output_tokens;
    total.reasoning_output_tokens += usage.reasoning_output_tokens;
    total.total_tokens += usage.total_tokens;
}

fn record_usage_deltas(
    path: &Path,
    retention_days: u32,
    date: NaiveDate,
    deltas: &BTreeMap<String, DailySpendDelta>,
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
) -> anyhow::Result<()> {
    with_history_lock(path, |history| {
        let date_key = date.to_string();
        let day = history.days.entry(date_key).or_default();
        for (model, delta) in deltas {
            let estimated_usd = estimate_cost_usd_for_usage(
                config,
                model_provider_id,
                model,
                &delta.usage,
                &delta.usage_by_service_tier,
                &delta.usage_by_service_tier_and_context_length,
            );
            add_amount(
                day,
                model,
                DailySpendAmount {
                    tokens: delta.usage.total_tokens,
                    estimated_usd,
                },
            );
        }
        prune_history(history, date, retention_days);
        write_history(path, history)
    })
}

fn add_amount(day: &mut DailySpendDay, model: &str, amount: DailySpendAmount) {
    let previous_day_tokens = day.tokens;
    day.tokens += amount.tokens;
    day.estimated_usd = if previous_day_tokens == 0 {
        amount.estimated_usd
    } else {
        add_optional_amount(day.estimated_usd, amount.estimated_usd)
    };
    let model_amount = day.models.entry(model.to_string()).or_default();
    let previous_model_tokens = model_amount.tokens;
    model_amount.tokens += amount.tokens;
    model_amount.estimated_usd = if previous_model_tokens == 0 {
        amount.estimated_usd
    } else {
        add_optional_amount(model_amount.estimated_usd, amount.estimated_usd)
    };
}

fn add_optional_amount(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        _ => None,
    }
}

fn prune_history(history: &mut DailySpendHistory, today: NaiveDate, retention_days: u32) {
    let retention_days = if retention_days == 0 {
        DEFAULT_DAILY_SPEND_RETENTION_DAYS
    } else {
        retention_days.clamp(1, MAX_DAILY_SPEND_RETENTION_DAYS)
    };
    let cutoff = today - Duration::days(i64::from(retention_days.saturating_sub(1)));
    history.days.retain(|date, _| {
        NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .is_ok_and(|date| date >= cutoff && date <= today)
    });
}

fn read_history(path: &Path) -> anyhow::Result<DailySpendHistory> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(err) => return Err(err.into()),
    };
    let history = serde_json::from_str(&text)?;
    Ok(history)
}

fn write_history(path: &Path, history: &DailySpendHistory) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("daily spend path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut file = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(file.as_file_mut(), history)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|err| err.error)?;
    Ok(())
}

fn with_history_lock(
    path: &Path,
    update: impl FnOnce(&mut DailySpendHistory) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("daily spend path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("lock");
    let mut stale_lock_removed = false;
    let lock = loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(lock) => break lock,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = lock_path.metadata().is_ok_and(|metadata| {
                    metadata
                        .modified()
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE_AFTER.to_std().unwrap_or_default())
                });
                if stale && !stale_lock_removed {
                    stale_lock_removed = true;
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }
                return Err(anyhow::anyhow!("daily spend history is busy; will retry"));
            }
            Err(err) => return Err(err.into()),
        }
    };
    let _guard = HistoryLock {
        file: lock,
        path: lock_path,
    };
    let mut history = read_history(path)?;
    update(&mut history)
}

struct HistoryLock {
    file: std::fs::File,
    path: PathBuf,
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        let _ = self.file.sync_all();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn default_history_version() -> u32 {
    HISTORY_VERSION
}

#[cfg(test)]
#[path = "daily_spend_tests.rs"]
mod tests;
