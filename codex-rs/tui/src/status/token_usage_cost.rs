use crate::token_usage::TokenUsage;
use codex_config::types::TuiStatusTokenUsage;
use codex_config::types::TuiStatusTokenUsageRate;
use codex_config::types::TuiStatusTokenUsageServiceTierRate;
use codex_protocol::protocol::TOKEN_USAGE_STANDARD_SERVICE_TIER;
use ratatui::prelude::*;
use ratatui::style::Stylize;
use std::collections::BTreeMap;

const TOKENS_PER_MILLION: f64 = 1_000_000.0;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StatusTokenUsageCostData {
    total_tokens: i64,
    input_tokens: i64,
    cached_input_tokens: i64,
    billable_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
    cost: Option<StatusTokenUsageCostBreakdown>,
}

#[derive(Debug, Clone, PartialEq)]
struct StatusTokenUsageCostBreakdown {
    total_usd: f64,
    input_usd: f64,
    output_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ResolvedRate {
    input_usd_per_1m: f64,
    cached_input_usd_per_1m: f64,
    output_usd_per_1m: f64,
}

pub(crate) fn compose_status_token_usage_cost(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
    usage: &TokenUsage,
    usage_by_service_tier: &BTreeMap<String, TokenUsage>,
) -> Option<StatusTokenUsageCostData> {
    if !config.enabled || usage.is_zero() {
        return None;
    }

    let input_tokens = usage.input_tokens.max(0);
    let cached_input_tokens = usage.cached_input().min(input_tokens);
    let billable_input_tokens = (input_tokens - cached_input_tokens).max(0);
    let output_tokens = usage.output_tokens.max(0);
    let reasoning_output_tokens = usage.reasoning_output_tokens.max(0).min(output_tokens);
    let total_tokens = input_tokens.saturating_add(output_tokens);
    let cost = cost_for_usage_by_service_tier(
        config,
        model_provider_id,
        model,
        usage,
        usage_by_service_tier,
    );

    Some(StatusTokenUsageCostData {
        total_tokens,
        input_tokens,
        cached_input_tokens,
        billable_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        cost,
    })
}

impl StatusTokenUsageCostData {
    pub(crate) fn summary_spans(&self) -> Vec<Span<'static>> {
        let mut spans = vec![
            Span::from(format_token_count(self.total_tokens)),
            Span::from(" API-equivalent tokens"),
        ];
        push_optional_cost(&mut spans, self.cost.as_ref().map(|cost| cost.total_usd));
        spans
    }

    pub(crate) fn input_spans(&self) -> Vec<Span<'static>> {
        let mut spans = vec![
            Span::from(format_token_count(self.input_tokens)),
            Span::from(" total, "),
            Span::from(format_token_count(self.cached_input_tokens)),
            Span::from(" cached, "),
            Span::from(format_token_count(self.billable_input_tokens)),
            Span::from(" billable"),
        ];
        push_optional_cost(&mut spans, self.cost.as_ref().map(|cost| cost.input_usd));
        spans
    }

    pub(crate) fn output_spans(&self) -> Vec<Span<'static>> {
        let mut spans = vec![
            Span::from(format_token_count(self.output_tokens)),
            Span::from(" total, "),
            Span::from(format_token_count(self.reasoning_output_tokens)),
            Span::from(" reasoning"),
        ];
        push_optional_cost(&mut spans, self.cost.as_ref().map(|cost| cost.output_usd));
        spans
    }
}

fn push_optional_cost(spans: &mut Vec<Span<'static>>, cost: Option<f64>) {
    if let Some(cost) = cost {
        spans.push(Span::from("  ").dim());
        spans.push(Span::from(format!("~{}", format_usd(cost))).dim());
    } else {
        spans.push(Span::from("  ").dim());
        spans.push(Span::from("cost unavailable").dim());
    }
}

fn format_usd(value: f64) -> String {
    if value < 0.01 {
        format!("${value:.4}")
    } else {
        format!("${value:.2}")
    }
}

fn format_token_count(tokens: i64) -> String {
    let tokens = tokens.max(0);
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn cost_for_tokens(tokens: i64, usd_per_1m: f64) -> f64 {
    tokens.max(0) as f64 * usd_per_1m / TOKENS_PER_MILLION
}

fn cost_for_usage_by_service_tier(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
    usage: &TokenUsage,
    usage_by_service_tier: &BTreeMap<String, TokenUsage>,
) -> Option<StatusTokenUsageCostBreakdown> {
    if usage_by_service_tier.is_empty() {
        let rate = rates_for_model(
            config,
            model_provider_id,
            model,
            TOKEN_USAGE_STANDARD_SERVICE_TIER,
        )?;
        return Some(cost_for_usage(usage, rate));
    }

    let mut total_cost = StatusTokenUsageCostBreakdown::default();
    let mut covered_usage = TokenUsage::default();
    for (service_tier, tier_usage) in usage_by_service_tier {
        let rate = rates_for_model(config, model_provider_id, model, service_tier)?;
        total_cost.add_assign(cost_for_usage(tier_usage, rate));
        add_usage(&mut covered_usage, tier_usage);
    }

    let unbucketed_usage = unbucketed_usage(usage, &covered_usage);
    if !unbucketed_usage.is_zero() {
        let rate = rates_for_model(
            config,
            model_provider_id,
            model,
            TOKEN_USAGE_STANDARD_SERVICE_TIER,
        )?;
        total_cost.add_assign(cost_for_usage(&unbucketed_usage, rate));
    }

    Some(total_cost)
}

impl Default for StatusTokenUsageCostBreakdown {
    fn default() -> Self {
        Self {
            total_usd: 0.0,
            input_usd: 0.0,
            output_usd: 0.0,
        }
    }
}

impl StatusTokenUsageCostBreakdown {
    fn add_assign(&mut self, other: Self) {
        self.total_usd += other.total_usd;
        self.input_usd += other.input_usd;
        self.output_usd += other.output_usd;
    }
}

fn cost_for_usage(usage: &TokenUsage, rate: ResolvedRate) -> StatusTokenUsageCostBreakdown {
    let input_tokens = usage.input_tokens.max(0);
    let cached_input_tokens = usage.cached_input().min(input_tokens);
    let billable_input_tokens = (input_tokens - cached_input_tokens).max(0);
    let output_tokens = usage.output_tokens.max(0);
    let input_usd = cost_for_tokens(billable_input_tokens, rate.input_usd_per_1m)
        + cost_for_tokens(cached_input_tokens, rate.cached_input_usd_per_1m);
    let output_usd = cost_for_tokens(output_tokens, rate.output_usd_per_1m);
    StatusTokenUsageCostBreakdown {
        total_usd: input_usd + output_usd,
        input_usd,
        output_usd,
    }
}

fn add_usage(total: &mut TokenUsage, usage: &TokenUsage) {
    total.input_tokens += usage.input_tokens;
    total.cached_input_tokens += usage.cached_input_tokens;
    total.output_tokens += usage.output_tokens;
    total.reasoning_output_tokens += usage.reasoning_output_tokens;
    total.total_tokens += usage.total_tokens;
}

fn unbucketed_usage(total: &TokenUsage, covered: &TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: (total.input_tokens - covered.input_tokens).max(0),
        cached_input_tokens: (total.cached_input_tokens - covered.cached_input_tokens).max(0),
        output_tokens: (total.output_tokens - covered.output_tokens).max(0),
        reasoning_output_tokens: (total.reasoning_output_tokens - covered.reasoning_output_tokens)
            .max(0),
        total_tokens: (total.total_tokens - covered.total_tokens).max(0),
    }
}

fn rates_for_model(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
    service_tier: &str,
) -> Option<ResolvedRate> {
    if let Some(rate) = config.model_rates.get(model) {
        if is_standard_service_tier(service_tier) {
            return Some(rate.into());
        }
        return rate.service_tiers.get(service_tier).map(Into::into);
    }

    built_in_rate_for_model(model_provider_id, model, service_tier)
}

fn built_in_rate_for_model(
    model_provider_id: &str,
    model: &str,
    service_tier: &str,
) -> Option<ResolvedRate> {
    if model_provider_id != "openai" {
        return None;
    }

    let model = model.to_ascii_lowercase();
    match (model.as_str(), service_tier) {
        // Keep this table aligned with public OpenAI API processing rates.
        // Unknown or newly added models still render tokens without cost until rates
        // are added here or supplied through config overrides.
        ("gpt-5.3-codex", TOKEN_USAGE_STANDARD_SERVICE_TIER) => Some(rate(
            /*input_usd_per_1m*/ 1.75, /*cached_input_usd_per_1m*/ 0.175,
            /*output_usd_per_1m*/ 14.0,
        )),
        ("gpt-5.4", TOKEN_USAGE_STANDARD_SERVICE_TIER) => Some(rate(
            /*input_usd_per_1m*/ 2.5, /*cached_input_usd_per_1m*/ 0.25,
            /*output_usd_per_1m*/ 15.0,
        )),
        ("gpt-5.4-mini", TOKEN_USAGE_STANDARD_SERVICE_TIER) => {
            Some(rate(
                /*input_usd_per_1m*/ 0.75, /*cached_input_usd_per_1m*/ 0.075,
                /*output_usd_per_1m*/ 4.5,
            ))
        }
        ("gpt-5.4-nano", TOKEN_USAGE_STANDARD_SERVICE_TIER) => {
            Some(rate(
                /*input_usd_per_1m*/ 0.20, /*cached_input_usd_per_1m*/ 0.02,
                /*output_usd_per_1m*/ 1.25,
            ))
        }
        ("gpt-5.5", TOKEN_USAGE_STANDARD_SERVICE_TIER) => Some(rate(
            /*input_usd_per_1m*/ 5.0, /*cached_input_usd_per_1m*/ 0.50,
            /*output_usd_per_1m*/ 30.0,
        )),
        ("gpt-5.4", "priority") => Some(rate(
            /*input_usd_per_1m*/ 5.0, /*cached_input_usd_per_1m*/ 0.50,
            /*output_usd_per_1m*/ 30.0,
        )),
        ("gpt-5.4-mini", "priority") => Some(rate(
            /*input_usd_per_1m*/ 1.50, /*cached_input_usd_per_1m*/ 0.15,
            /*output_usd_per_1m*/ 9.0,
        )),
        ("gpt-5.5", "priority") => Some(rate(
            /*input_usd_per_1m*/ 12.50, /*cached_input_usd_per_1m*/ 1.25,
            /*output_usd_per_1m*/ 75.0,
        )),
        ("gpt-5.4", "flex") => Some(rate(
            /*input_usd_per_1m*/ 1.25, /*cached_input_usd_per_1m*/ 0.13,
            /*output_usd_per_1m*/ 7.50,
        )),
        ("gpt-5.4-mini", "flex") => Some(rate(
            /*input_usd_per_1m*/ 0.375, /*cached_input_usd_per_1m*/ 0.0375,
            /*output_usd_per_1m*/ 2.25,
        )),
        ("gpt-5.4-nano", "flex") => Some(rate(
            /*input_usd_per_1m*/ 0.10, /*cached_input_usd_per_1m*/ 0.01,
            /*output_usd_per_1m*/ 0.625,
        )),
        ("gpt-5.5", "flex") => Some(rate(
            /*input_usd_per_1m*/ 2.50, /*cached_input_usd_per_1m*/ 0.25,
            /*output_usd_per_1m*/ 15.0,
        )),
        _ => None,
    }
}

fn is_standard_service_tier(service_tier: &str) -> bool {
    service_tier == TOKEN_USAGE_STANDARD_SERVICE_TIER || service_tier == "default"
}

fn rate(
    input_usd_per_1m: f64,
    cached_input_usd_per_1m: f64,
    output_usd_per_1m: f64,
) -> ResolvedRate {
    ResolvedRate {
        input_usd_per_1m,
        cached_input_usd_per_1m,
        output_usd_per_1m,
    }
}

impl From<&TuiStatusTokenUsageRate> for ResolvedRate {
    fn from(rate: &TuiStatusTokenUsageRate) -> Self {
        Self {
            input_usd_per_1m: rate.input_usd_per_1m,
            cached_input_usd_per_1m: rate.cached_input_usd_per_1m,
            output_usd_per_1m: rate.output_usd_per_1m,
        }
    }
}

impl From<&TuiStatusTokenUsageServiceTierRate> for ResolvedRate {
    fn from(rate: &TuiStatusTokenUsageServiceTierRate) -> Self {
        Self {
            input_usd_per_1m: rate.input_usd_per_1m,
            cached_input_usd_per_1m: rate.cached_input_usd_per_1m,
            output_usd_per_1m: rate.output_usd_per_1m,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeMap;

    fn span_text(spans: Vec<Span<'static>>) -> String {
        spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn model_rate(
        input_usd_per_1m: f64,
        cached_input_usd_per_1m: f64,
        output_usd_per_1m: f64,
    ) -> TuiStatusTokenUsageRate {
        TuiStatusTokenUsageRate {
            input_usd_per_1m,
            cached_input_usd_per_1m,
            output_usd_per_1m,
            service_tiers: BTreeMap::new(),
        }
    }

    fn tier_rate(
        input_usd_per_1m: f64,
        cached_input_usd_per_1m: f64,
        output_usd_per_1m: f64,
    ) -> TuiStatusTokenUsageServiceTierRate {
        TuiStatusTokenUsageServiceTierRate {
            input_usd_per_1m,
            cached_input_usd_per_1m,
            output_usd_per_1m,
        }
    }

    #[test]
    fn cost_uses_built_in_model_rates() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            model_rates: BTreeMap::new(),
        };
        let usage = TokenUsage {
            input_tokens: 151_800,
            cached_input_tokens: 119_400,
            output_tokens: 32_400,
            reasoning_output_tokens: 8_700,
            total_tokens: 184_200,
        };

        let data = compose_status_token_usage_cost(
            &config,
            "openai",
            "gpt-5.3-codex",
            &usage,
            &BTreeMap::new(),
        )
        .expect("usage should render");

        assert_eq!(
            span_text(data.summary_spans()),
            "184.2K API-equivalent tokens  ~$0.53"
        );
        assert_eq!(
            span_text(data.input_spans()),
            "151.8K total, 119.4K cached, 32.4K billable  ~$0.08"
        );
        assert_eq!(
            span_text(data.output_spans()),
            "32.4K total, 8.7K reasoning  ~$0.45"
        );
    }

    #[test]
    fn cost_uses_configured_model_rate_overrides() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            model_rates: BTreeMap::from([(
                "custom-model".to_string(),
                model_rate(
                    /*input*/ 2.0, /*cached_input*/ 1.0, /*output*/ 4.0,
                ),
            )]),
        };
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 250_000,
            output_tokens: 500_000,
            reasoning_output_tokens: 100_000,
            total_tokens: 1_500_000,
        };

        let data = compose_status_token_usage_cost(
            &config,
            "custom",
            "custom-model",
            &usage,
            &BTreeMap::new(),
        )
        .expect("usage should render");

        assert_eq!(
            span_text(data.summary_spans()),
            "1.5M API-equivalent tokens  ~$3.75"
        );
    }

    #[test]
    fn cost_uses_priority_bucket_rates_when_present() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            model_rates: BTreeMap::new(),
        };
        let usage = TokenUsage {
            input_tokens: 100_000,
            cached_input_tokens: 20_000,
            output_tokens: 10_000,
            reasoning_output_tokens: 5_000,
            total_tokens: 110_000,
        };
        let usage_by_service_tier = BTreeMap::from([("priority".to_string(), usage.clone())]);

        let data = compose_status_token_usage_cost(
            &config,
            "openai",
            "gpt-5.5",
            &usage,
            &usage_by_service_tier,
        )
        .expect("usage should render");

        assert_eq!(
            span_text(data.summary_spans()),
            "110.0K API-equivalent tokens  ~$1.77"
        );
        assert_eq!(
            span_text(data.input_spans()),
            "100.0K total, 20.0K cached, 80.0K billable  ~$1.02"
        );
        assert_eq!(
            span_text(data.output_spans()),
            "10.0K total, 5.0K reasoning  ~$0.75"
        );
    }

    #[test]
    fn configured_service_tier_rates_override_model_rates() {
        let mut custom_rate = model_rate(
            /*input*/ 2.0, /*cached_input*/ 1.0, /*output*/ 4.0,
        );
        custom_rate.service_tiers.insert(
            "priority".to_string(),
            tier_rate(
                /*input*/ 20.0, /*cached_input*/ 10.0, /*output*/ 40.0,
            ),
        );
        let config = TuiStatusTokenUsage {
            enabled: true,
            model_rates: BTreeMap::from([("custom-model".to_string(), custom_rate)]),
        };
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 250_000,
            output_tokens: 500_000,
            reasoning_output_tokens: 100_000,
            total_tokens: 1_500_000,
        };
        let usage_by_service_tier = BTreeMap::from([("priority".to_string(), usage.clone())]);

        let data = compose_status_token_usage_cost(
            &config,
            "custom",
            "custom-model",
            &usage,
            &usage_by_service_tier,
        )
        .expect("usage should render");

        assert_eq!(
            span_text(data.summary_spans()),
            "1.5M API-equivalent tokens  ~$37.50"
        );
    }

    #[test]
    fn unknown_model_still_renders_tokens_without_cost() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            model_rates: BTreeMap::new(),
        };
        let usage = TokenUsage {
            input_tokens: 1_000,
            cached_input_tokens: 100,
            output_tokens: 200,
            reasoning_output_tokens: 50,
            total_tokens: 1_200,
        };

        let data = compose_status_token_usage_cost(
            &config,
            "openai",
            "unknown-model",
            &usage,
            &BTreeMap::new(),
        )
        .expect("usage should render");

        assert_eq!(
            span_text(data.summary_spans()),
            "1.2K API-equivalent tokens  cost unavailable"
        );
    }

    #[test]
    fn non_openai_provider_does_not_use_openai_built_in_rates() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            model_rates: BTreeMap::new(),
        };
        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 200,
            total_tokens: 1_200,
            ..TokenUsage::default()
        };

        let data =
            compose_status_token_usage_cost(&config, "custom", "gpt-5.4", &usage, &BTreeMap::new())
                .expect("usage should render");

        assert_eq!(
            span_text(data.summary_spans()),
            "1.2K API-equivalent tokens  cost unavailable"
        );
    }

    #[test]
    fn disabled_config_does_not_render() {
        let config = TuiStatusTokenUsage::default();
        let usage = TokenUsage {
            input_tokens: 1_000,
            total_tokens: 1_000,
            ..TokenUsage::default()
        };

        assert!(
            compose_status_token_usage_cost(&config, "openai", "gpt-5.4", &usage, &BTreeMap::new())
                .is_none()
        );
    }
}
