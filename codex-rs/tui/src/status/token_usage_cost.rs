use crate::token_usage::TokenUsage;
use codex_config::types::TuiStatusTokenUsage;
use codex_config::types::TuiStatusTokenUsageRate;
use ratatui::prelude::*;
use ratatui::style::Stylize;

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

pub(crate) fn compose_status_token_usage_cost(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
    usage: &TokenUsage,
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
    let cost = rates_for_model(config, model_provider_id, model).map(|rate| {
        let input_usd = cost_for_tokens(billable_input_tokens, rate.input_usd_per_1m)
            + cost_for_tokens(cached_input_tokens, rate.cached_input_usd_per_1m);
        let output_usd = cost_for_tokens(output_tokens, rate.output_usd_per_1m);
        StatusTokenUsageCostBreakdown {
            total_usd: input_usd + output_usd,
            input_usd,
            output_usd,
        }
    });

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

fn rates_for_model(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
) -> Option<TuiStatusTokenUsageRate> {
    config
        .model_rates
        .get(model)
        .copied()
        .or_else(|| built_in_rate_for_model(model_provider_id, model))
}

fn built_in_rate_for_model(
    model_provider_id: &str,
    model: &str,
) -> Option<TuiStatusTokenUsageRate> {
    if model_provider_id != "openai" {
        return None;
    }

    let model = model.to_ascii_lowercase();
    match model.as_str() {
        // Keep this table aligned with public OpenAI API standard-processing rates.
        // Unknown or newly added models still render tokens without cost until rates
        // are added here or supplied through config overrides.
        "gpt-5.3-codex" => Some(rate(
            /*input*/ 1.75, /*cached_input*/ 0.175, /*output*/ 14.0,
        )),
        "gpt-5.4" => Some(rate(
            /*input*/ 2.5, /*cached_input*/ 0.25, /*output*/ 15.0,
        )),
        "gpt-5.4-mini" => {
            Some(rate(
                /*input*/ 0.75, /*cached_input*/ 0.075, /*output*/ 4.5,
            ))
        }
        "gpt-5.4-nano" => {
            Some(rate(
                /*input*/ 0.20, /*cached_input*/ 0.02, /*output*/ 1.25,
            ))
        }
        "gpt-5.5" => Some(rate(
            /*input*/ 5.0, /*cached_input*/ 0.50, /*output*/ 30.0,
        )),
        _ => None,
    }
}

const fn rate(
    input_usd_per_1m: f64,
    cached_input_usd_per_1m: f64,
    output_usd_per_1m: f64,
) -> TuiStatusTokenUsageRate {
    TuiStatusTokenUsageRate {
        input_usd_per_1m,
        cached_input_usd_per_1m,
        output_usd_per_1m,
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

        let data = compose_status_token_usage_cost(&config, "openai", "gpt-5.3-codex", &usage)
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
                rate(
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

        let data = compose_status_token_usage_cost(&config, "custom", "custom-model", &usage)
            .expect("usage should render");

        assert_eq!(
            span_text(data.summary_spans()),
            "1.5M API-equivalent tokens  ~$3.75"
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

        let data = compose_status_token_usage_cost(&config, "openai", "unknown-model", &usage)
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

        let data = compose_status_token_usage_cost(&config, "custom", "gpt-5.4", &usage)
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

        assert!(compose_status_token_usage_cost(&config, "openai", "gpt-5.4", &usage).is_none());
    }
}
