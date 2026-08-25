use super::daily_spend_report::SpendPeriod;
use super::*;
use crate::token_usage::TokenUsage;
use chrono::NaiveDate;
use codex_config::types::TuiStatusTokenUsageRate;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use ratatui::text::Line;
use std::collections::BTreeMap;
use tempfile::TempDir;

fn usage(input_tokens: i64, output_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens + output_tokens,
        ..TokenUsage::default()
    }
}

fn model_info(
    total_token_usage: TokenUsage,
    usage_by_model: BTreeMap<String, TokenUsage>,
    usage_by_model_and_service_tier_and_context_length: BTreeMap<
        String,
        BTreeMap<String, BTreeMap<String, TokenUsage>>,
    >,
) -> TokenUsageInfo {
    TokenUsageInfo {
        total_token_usage,
        last_token_usage: TokenUsage::default(),
        usage_by_model,
        usage_by_model_and_service_tier_and_context_length,
        ..TokenUsageInfo::default()
    }
}

fn configured_usage() -> TuiStatusTokenUsage {
    TuiStatusTokenUsage {
        enabled: true,
        daily_spend_retention_days: 2,
        model_rates: BTreeMap::from([(
            "model-a".to_string(),
            TuiStatusTokenUsageRate {
                input_usd_per_1m: 2.0,
                cached_input_usd_per_1m: 1.0,
                cache_write_usd_per_1m: 0.0,
                output_usd_per_1m: 4.0,
                service_tiers: BTreeMap::new(),
            },
        )]),
    }
}

fn render_lines(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn usage_deltas_keep_swapped_models_separate() {
    let previous_model_a = usage(100, 20);
    let current_model_a = usage(300, 40);
    let current_model_b = usage(80, 10);
    let previous = model_info(
        previous_model_a.clone(),
        BTreeMap::from([("model-a".to_string(), previous_model_a.clone())]),
        BTreeMap::from([(
            "model-a".to_string(),
            BTreeMap::from([(
                "standard".to_string(),
                BTreeMap::from([("short".to_string(), previous_model_a)]),
            )]),
        )]),
    );
    let current = model_info(
        usage(380, 50),
        BTreeMap::from([
            ("model-a".to_string(), current_model_a.clone()),
            ("model-b".to_string(), current_model_b.clone()),
        ]),
        BTreeMap::from([
            (
                "model-a".to_string(),
                BTreeMap::from([(
                    "standard".to_string(),
                    BTreeMap::from([("short".to_string(), current_model_a)]),
                )]),
            ),
            (
                "model-b".to_string(),
                BTreeMap::from([(
                    "standard".to_string(),
                    BTreeMap::from([("short".to_string(), current_model_b.clone())]),
                )]),
            ),
        ]),
    );

    let deltas = usage_deltas(Some(&previous), &current, "fallback-model");

    assert_eq!(
        deltas,
        BTreeMap::from([
            (
                "model-a".to_string(),
                DailySpendDelta {
                    usage: usage(200, 20),
                    usage_by_service_tier: BTreeMap::from([(
                        "standard".to_string(),
                        usage(200, 20),
                    )]),
                    usage_by_service_tier_and_context_length: BTreeMap::from([(
                        "standard".to_string(),
                        BTreeMap::from([("short".to_string(), usage(200, 20))]),
                    )]),
                },
            ),
            (
                "model-b".to_string(),
                DailySpendDelta {
                    usage: current_model_b.clone(),
                    usage_by_service_tier: BTreeMap::from([(
                        "standard".to_string(),
                        current_model_b.clone(),
                    )]),
                    usage_by_service_tier_and_context_length: BTreeMap::from([(
                        "standard".to_string(),
                        BTreeMap::from([("short".to_string(), current_model_b)]),
                    )]),
                },
            ),
        ])
    );
}

#[test]
fn usage_deltas_replay_legacy_aggregate_before_new_model_usage() {
    let previous_usage = usage(1_000, 100);
    let new_usage = usage(200, 20);
    let previous = model_info(previous_usage, BTreeMap::new(), BTreeMap::new());
    let current = model_info(
        usage(1_200, 120),
        BTreeMap::from([("model-a".to_string(), new_usage.clone())]),
        BTreeMap::from([(
            "model-a".to_string(),
            BTreeMap::from([(
                "standard".to_string(),
                BTreeMap::from([("short".to_string(), new_usage.clone())]),
            )]),
        )]),
    );

    let deltas = usage_deltas(Some(&previous), &current, "fallback-model");

    assert_eq!(
        deltas,
        BTreeMap::from([(
            "model-a".to_string(),
            DailySpendDelta {
                usage: new_usage.clone(),
                usage_by_service_tier: BTreeMap::from([(
                    "standard".to_string(),
                    new_usage.clone(),
                )]),
                usage_by_service_tier_and_context_length: BTreeMap::from([(
                    "standard".to_string(),
                    BTreeMap::from([("short".to_string(), new_usage)]),
                )]),
            },
        )])
    );
}

#[test]
fn daily_history_is_retained_within_configured_window() {
    let temp_dir = TempDir::new().expect("temporary directory");
    let path = temp_dir.path().join("daily_spend.json");
    let config = configured_usage();
    let deltas = BTreeMap::from([(
        "model-a".to_string(),
        DailySpendDelta {
            usage: usage(1_000, 500),
            usage_by_service_tier: BTreeMap::new(),
            usage_by_service_tier_and_context_length: BTreeMap::new(),
        },
    )]);
    assert_eq!(
        estimate_cost_usd_for_usage(
            &config,
            "openai",
            "model-a",
            &deltas["model-a"].usage,
            &deltas["model-a"].usage_by_service_tier,
            &deltas["model-a"].usage_by_service_tier_and_context_length,
        ),
        Some(0.004)
    );

    record_usage_deltas(
        &path,
        config.daily_spend_retention_days,
        NaiveDate::from_ymd_opt(2024, 1, 1).expect("date"),
        &deltas,
        &config,
        "openai",
    )
    .expect("first rollup should persist");
    record_usage_deltas(
        &path,
        config.daily_spend_retention_days,
        NaiveDate::from_ymd_opt(2024, 1, 3).expect("date"),
        &deltas,
        &config,
        "openai",
    )
    .expect("second rollup should persist");

    let history = read_history(&path).expect("history should deserialize");
    assert_eq!(
        history.days.keys().cloned().collect::<Vec<_>>(),
        vec!["2024-01-03".to_string()]
    );
    let amount = &history.days["2024-01-03"].models["model-a"];
    assert_eq!(amount.tokens, 1_500);
    assert!((amount.estimated_usd.expect("configured price") - 0.004).abs() < f64::EPSILON);
}

#[test]
fn spend_report_shows_daily_fluctuation_and_model_totals() {
    let temp_dir = TempDir::new().expect("temporary directory");
    let path = temp_dir.path().join("daily_spend.json");
    let history = DailySpendHistory {
        version: HISTORY_VERSION,
        days: BTreeMap::from([
            (
                "2024-01-01".to_string(),
                DailySpendDay {
                    tokens: 1_000,
                    estimated_usd: Some(1.25),
                    models: BTreeMap::from([(
                        "model-a".to_string(),
                        DailySpendAmount {
                            tokens: 1_000,
                            estimated_usd: Some(1.25),
                        },
                    )]),
                },
            ),
            (
                "2024-01-03".to_string(),
                DailySpendDay {
                    tokens: 2_000,
                    estimated_usd: Some(4.5),
                    models: BTreeMap::from([(
                        "model-b".to_string(),
                        DailySpendAmount {
                            tokens: 2_000,
                            estimated_usd: Some(4.5),
                        },
                    )]),
                },
            ),
        ]),
    };
    write_history(&path, &history).expect("history should persist");

    let lines = render_report_at(
        &path,
        "2024-01",
        NaiveDate::from_ymd_opt(2024, 2, 1).expect("date"),
    )
    .expect("report should render");

    assert_snapshot!(render_lines(&lines));
}

#[test]
fn spend_period_supports_default_days_numeric_month_and_range() {
    let today = NaiveDate::from_ymd_opt(2024, 2, 15).expect("date");
    assert_eq!(
        SpendPeriod::parse("", today).expect("default period"),
        SpendPeriod::Days {
            start: NaiveDate::from_ymd_opt(2024, 1, 17).expect("date"),
            end: today,
            count: 30,
        }
    );
    assert_eq!(
        SpendPeriod::parse("7", today).expect("numeric period"),
        SpendPeriod::Days {
            start: NaiveDate::from_ymd_opt(2024, 2, 9).expect("date"),
            end: today,
            count: 7,
        }
    );
    assert_eq!(
        SpendPeriod::parse("2024-01", today).expect("month period"),
        SpendPeriod::Month {
            start: NaiveDate::from_ymd_opt(2024, 1, 1).expect("date"),
            end: NaiveDate::from_ymd_opt(2024, 1, 31).expect("date"),
        }
    );
    assert_eq!(
        SpendPeriod::parse("2024-02-01..2024-02-15", today).expect("range period"),
        SpendPeriod::Range {
            start: NaiveDate::from_ymd_opt(2024, 2, 1).expect("date"),
            end: today,
        }
    );
}
