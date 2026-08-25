//! Rendering and period selection for the daily spend report.

use super::DailySpendDay;
use super::add_optional_amount;
use super::read_history;
use crate::status::format_tokens_compact;
use chrono::Duration;
use chrono::Months;
use chrono::NaiveDate;
use codex_config::types::MAX_DAILY_SPEND_RETENTION_DAYS;
use ratatui::style::Stylize;
use ratatui::text::Line;
use std::collections::BTreeMap;
use std::path::Path;

const SPARKLINE_LEVELS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub(super) fn render_report_at(
    path: &Path,
    args: &str,
    today: NaiveDate,
) -> anyhow::Result<Vec<Line<'static>>> {
    let period = SpendPeriod::parse(args, today)?;
    let history = read_history(path)?;
    let dates = period.dates();
    let days = dates
        .iter()
        .map(|date| {
            history
                .days
                .get(&date.to_string())
                .cloned()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let total_tokens = days.iter().map(|day| day.tokens).sum::<i64>();
    let total_usd = days.iter().try_fold(0.0, |total, day| {
        if day.tokens == 0 {
            Some(total)
        } else {
            Some(total + day.estimated_usd?)
        }
    });
    let model_totals = model_totals(&days);
    let mut lines = vec![format!("Spend {}", period.label()).bold().into()];
    if days.iter().all(|day| day.tokens == 0) {
        lines.push("  No recorded spend in this period.".dim().into());
        return Ok(lines);
    }

    lines.push(
        format!(
            "  Total: {}  |  {} tokens",
            format_spend(total_usd),
            format_tokens_compact(total_tokens),
        )
        .into(),
    );
    lines.push(format!("  Daily USD: {}", sparkline(&days)).into());
    if let Some((minimum, maximum)) = daily_price_range(&days) {
        lines.push(
            format!(
                "  Blended effective price / 1M tokens: {}  ({}–{})",
                price_sparkline(&days),
                format_price(minimum),
                format_price(maximum),
            )
            .into(),
        );
    }
    if let (Some(first), Some(last)) = (dates.first(), dates.last()) {
        lines.push(format!("  Range: {first} through {last}").dim().into());
    }
    if days
        .iter()
        .any(|day| day.estimated_usd.is_none() && day.tokens > 0)
    {
        lines.push(
            "  Some tokens used models without a configured price; totals are partial."
                .dim()
                .into(),
        );
    }
    if model_totals.len() > 1 {
        lines.push("  By model:".bold().into());
        for (model, (tokens, usd)) in model_totals {
            lines.push(
                format!(
                    "    {model}: {}  |  {} tokens",
                    format_spend(usd),
                    format_tokens_compact(tokens)
                )
                .into(),
            );
        }
    }
    Ok(lines)
}

fn model_totals(days: &[DailySpendDay]) -> BTreeMap<String, (i64, Option<f64>)> {
    let mut totals = BTreeMap::new();
    for day in days {
        for (model, amount) in &day.models {
            let entry = totals.entry(model.clone()).or_insert((0, Some(0.0)));
            entry.0 += amount.tokens;
            entry.1 = add_optional_amount(entry.1, amount.estimated_usd);
        }
    }
    totals
}

fn sparkline(days: &[DailySpendDay]) -> String {
    let max = days
        .iter()
        .filter_map(|day| day.estimated_usd)
        .fold(0.0, f64::max);
    days.iter()
        .map(|day| {
            if day.tokens == 0 {
                return SPARKLINE_LEVELS[0];
            }
            let Some(value) = day.estimated_usd else {
                return '?';
            };
            if max <= 0.0 {
                return SPARKLINE_LEVELS[0];
            }
            let index =
                ((value / max) * f64::from(SPARKLINE_LEVELS.len() as u32 - 1)).round() as usize;
            SPARKLINE_LEVELS[index.min(SPARKLINE_LEVELS.len() - 1)]
        })
        .collect()
}

fn format_spend(value: Option<f64>) -> String {
    value.map_or_else(|| "unpriced".to_string(), |value| format!("~${value:.2}"))
}

fn daily_price(day: &DailySpendDay) -> Option<f64> {
    (day.tokens > 0)
        .then_some(day.estimated_usd?)
        .map(|usd| usd * 1_000_000.0 / day.tokens as f64)
}

fn daily_price_range(days: &[DailySpendDay]) -> Option<(f64, f64)> {
    let mut prices = days.iter().filter_map(daily_price);
    let first = prices.next()?;
    Some(prices.fold((first, first), |(minimum, maximum), price| {
        (minimum.min(price), maximum.max(price))
    }))
}

fn price_sparkline(days: &[DailySpendDay]) -> String {
    let Some((minimum, maximum)) = daily_price_range(days) else {
        return String::new();
    };
    days.iter()
        .map(|day| {
            let Some(price) = daily_price(day) else {
                return if day.tokens == 0 { '·' } else { '?' };
            };
            if (maximum - minimum).abs() < f64::EPSILON {
                return SPARKLINE_LEVELS[0];
            }
            let index = (((price - minimum) / (maximum - minimum))
                * f64::from(SPARKLINE_LEVELS.len() as u32 - 1))
            .round() as usize;
            SPARKLINE_LEVELS[index.min(SPARKLINE_LEVELS.len() - 1)]
        })
        .collect()
}

fn format_price(value: f64) -> String {
    format!("${value:.2}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SpendPeriod {
    Days {
        start: NaiveDate,
        end: NaiveDate,
        count: u32,
    },
    Month {
        start: NaiveDate,
        end: NaiveDate,
    },
    Range {
        start: NaiveDate,
        end: NaiveDate,
    },
}

impl SpendPeriod {
    pub(super) fn parse(args: &str, today: NaiveDate) -> anyhow::Result<Self> {
        let args = args.trim();
        if args.is_empty() {
            return Ok(Self::Days {
                start: today - Duration::days(29),
                end: today,
                count: 30,
            });
        }
        if let Ok(count) = args.parse::<u32>() {
            if count == 0 || count > MAX_DAILY_SPEND_RETENTION_DAYS {
                anyhow::bail!("days must be between 1 and {MAX_DAILY_SPEND_RETENTION_DAYS}");
            }
            return Ok(Self::Days {
                start: today - Duration::days(i64::from(count - 1)),
                end: today,
                count,
            });
        }
        if let Ok(start) = NaiveDate::parse_from_str(&format!("{args}-01"), "%Y-%m-%d") {
            let end = start
                .checked_add_months(Months::new(1))
                .and_then(|date| date.checked_sub_signed(Duration::days(1)))
                .ok_or_else(|| anyhow::anyhow!("invalid month: {args}"))?;
            return Ok(Self::Month { start, end });
        }
        if let Some((start, end)) = args.split_once("..") {
            let start = NaiveDate::parse_from_str(start.trim(), "%Y-%m-%d")?;
            let end = NaiveDate::parse_from_str(end.trim(), "%Y-%m-%d")?;
            if end < start {
                anyhow::bail!("spend range must end on or after its start");
            }
            let span = (end - start).num_days() + 1;
            if span > i64::from(MAX_DAILY_SPEND_RETENTION_DAYS) {
                anyhow::bail!("spend range cannot exceed {MAX_DAILY_SPEND_RETENTION_DAYS} days");
            }
            return Ok(Self::Range { start, end });
        }
        anyhow::bail!("usage: /spend [days|YYYY-MM|YYYY-MM-DD..YYYY-MM-DD]");
    }

    fn dates(self) -> Vec<NaiveDate> {
        let (start, end) = match self {
            Self::Days { start, end, .. }
            | Self::Month { start, end }
            | Self::Range { start, end } => (start, end),
        };
        let count = (end - start).num_days().saturating_add(1);
        (0..count)
            .filter_map(|offset| start.checked_add_signed(Duration::days(offset)))
            .collect()
    }

    fn label(self) -> String {
        match self {
            Self::Days { count, .. } => format!("(last {count} days)"),
            Self::Month { start, .. } => format!("({})", start.format("%Y-%m")),
            Self::Range { start, end } => format!("({start} through {end})"),
        }
    }
}
