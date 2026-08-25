use crate::token_usage::TokenUsage;
use codex_config::types::TuiStatusTokenUsage;
use codex_config::types::TuiStatusTokenUsageRate;
use codex_config::types::TuiStatusTokenUsageServiceTierRate;
use codex_protocol::protocol::TOKEN_USAGE_LONG_CONTEXT;
use codex_protocol::protocol::TOKEN_USAGE_SHORT_CONTEXT;
use codex_protocol::protocol::TOKEN_USAGE_STANDARD_SERVICE_TIER;
use ratatui::prelude::*;
use ratatui::style::Stylize;
use std::collections::BTreeMap;

const TOKENS_PER_MILLION: f64 = 1_000_000.0;
const UNATTRIBUTED_MODEL_LABEL: &str = "unattributed";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StatusTokenUsageCostData {
    total_tokens: i64,
    input_tokens: i64,
    cached_input_tokens: i64,
    cache_write_tokens: i64,
    billable_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
    cost: Option<StatusTokenUsageCostBreakdown>,
    model_breakdowns: Vec<StatusTokenUsageCostModelData>,
}

#[derive(Debug, Clone, PartialEq)]
struct StatusTokenUsageCostBreakdown {
    total_usd: f64,
    input_usd: f64,
    output_usd: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct StatusTokenUsageCostModelData {
    model: String,
    total_tokens: i64,
    cost: Option<StatusTokenUsageCostBreakdown>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ResolvedRate {
    input_usd_per_1m: f64,
    cached_input_usd_per_1m: f64,
    cache_write_usd_per_1m: f64,
    output_usd_per_1m: f64,
}

#[cfg(test)]
pub(crate) fn compose_status_token_usage_cost(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
    usage: &TokenUsage,
    usage_by_service_tier: &BTreeMap<String, TokenUsage>,
) -> Option<StatusTokenUsageCostData> {
    compose_status_token_usage_cost_with_context_length(
        config,
        model_provider_id,
        model,
        usage,
        usage_by_service_tier,
        &BTreeMap::new(),
    )
}

pub(crate) fn compose_status_token_usage_cost_with_context_length(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
    usage: &TokenUsage,
    usage_by_service_tier: &BTreeMap<String, TokenUsage>,
    usage_by_service_tier_and_context_length: &BTreeMap<String, BTreeMap<String, TokenUsage>>,
) -> Option<StatusTokenUsageCostData> {
    compose_status_token_usage_cost_with_models(
        config,
        model_provider_id,
        model,
        usage,
        usage_by_service_tier,
        usage_by_service_tier_and_context_length,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
}

pub(crate) fn compose_status_token_usage_cost_with_models(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
    usage: &TokenUsage,
    usage_by_service_tier: &BTreeMap<String, TokenUsage>,
    usage_by_service_tier_and_context_length: &BTreeMap<String, BTreeMap<String, TokenUsage>>,
    usage_by_model: &BTreeMap<String, TokenUsage>,
    usage_by_model_and_service_tier_and_context_length: &BTreeMap<
        String,
        BTreeMap<String, BTreeMap<String, TokenUsage>>,
    >,
) -> Option<StatusTokenUsageCostData> {
    if !config.enabled || usage.is_zero() {
        return None;
    }

    if !usage_by_model.is_empty() {
        let mut attributed_usage = TokenUsage::default();
        let mut model_breakdowns = Vec::new();
        let mut aggregate_cost = StatusTokenUsageCostBreakdown::default();
        let mut cost_available = true;

        for (model, model_usage) in usage_by_model {
            if model_usage.is_zero() {
                continue;
            }
            let model_context_usages = usage_by_model_and_service_tier_and_context_length
                .get(model)
                .cloned()
                .unwrap_or_default();
            let model_tier_usage = model_context_usages
                .iter()
                .map(|(service_tier, context_usages)| {
                    let mut total = TokenUsage::default();
                    for context_usage in context_usages.values() {
                        add_usage(&mut total, context_usage);
                    }
                    (service_tier.clone(), total)
                })
                .collect::<BTreeMap<_, _>>();
            let model_data = compose_single_model_cost(
                config,
                model_provider_id,
                model,
                model_usage,
                &model_tier_usage,
                &model_context_usages,
            )?;
            add_usage(&mut attributed_usage, model_usage);
            if let Some(cost) = model_data.cost.as_ref() {
                aggregate_cost.add_assign(cost.clone());
            } else {
                cost_available = false;
            }
            model_breakdowns.push(StatusTokenUsageCostModelData {
                model: model.clone(),
                total_tokens: model_data.total_tokens,
                cost: model_data.cost,
            });
        }

        let unattributed_usage = unbucketed_usage(usage, &attributed_usage);
        if !unattributed_usage.is_zero() {
            model_breakdowns.push(StatusTokenUsageCostModelData {
                model: UNATTRIBUTED_MODEL_LABEL.to_string(),
                total_tokens: input_token_breakdown(&unattributed_usage)
                    .0
                    .saturating_add(unattributed_usage.output_tokens.max(0)),
                cost: None,
            });
            cost_available = false;
        }

        if model_breakdowns.is_empty() {
            return None;
        }
        return Some(status_token_usage_cost_data(
            usage,
            if cost_available {
                Some(aggregate_cost)
            } else {
                None
            },
            model_breakdowns,
        ));
    }

    compose_single_model_cost(
        config,
        model_provider_id,
        model,
        usage,
        usage_by_service_tier,
        usage_by_service_tier_and_context_length,
    )
}

pub(crate) fn estimate_cost_usd_for_usage(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
    usage: &TokenUsage,
    usage_by_service_tier: &BTreeMap<String, TokenUsage>,
    usage_by_service_tier_and_context_length: &BTreeMap<String, BTreeMap<String, TokenUsage>>,
) -> Option<f64> {
    if usage.is_zero() {
        return Some(0.0);
    }
    cost_for_usage_by_service_tier(
        config,
        model_provider_id,
        model,
        usage,
        usage_by_service_tier,
        usage_by_service_tier_and_context_length,
    )
    .map(|cost| cost.total_usd)
}

fn compose_single_model_cost(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
    usage: &TokenUsage,
    usage_by_service_tier: &BTreeMap<String, TokenUsage>,
    usage_by_service_tier_and_context_length: &BTreeMap<String, BTreeMap<String, TokenUsage>>,
) -> Option<StatusTokenUsageCostData> {
    if usage.is_zero() {
        return None;
    }

    let (input_tokens, cached_input_tokens, cache_write_tokens, billable_input_tokens) =
        input_token_breakdown(usage);
    let output_tokens = usage.output_tokens.max(0);
    let reasoning_output_tokens = usage.reasoning_output_tokens.max(0).min(output_tokens);
    let total_tokens = input_tokens.saturating_add(output_tokens);
    let cost = cost_for_usage_by_service_tier(
        config,
        model_provider_id,
        model,
        usage,
        usage_by_service_tier,
        usage_by_service_tier_and_context_length,
    );

    Some(StatusTokenUsageCostData {
        total_tokens,
        input_tokens,
        cached_input_tokens,
        cache_write_tokens,
        billable_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        cost,
        model_breakdowns: Vec::new(),
    })
}

fn status_token_usage_cost_data(
    usage: &TokenUsage,
    cost: Option<StatusTokenUsageCostBreakdown>,
    model_breakdowns: Vec<StatusTokenUsageCostModelData>,
) -> StatusTokenUsageCostData {
    let (input_tokens, cached_input_tokens, cache_write_tokens, billable_input_tokens) =
        input_token_breakdown(usage);
    let output_tokens = usage.output_tokens.max(0);
    let reasoning_output_tokens = usage.reasoning_output_tokens.max(0).min(output_tokens);
    StatusTokenUsageCostData {
        total_tokens: input_tokens.saturating_add(output_tokens),
        input_tokens,
        cached_input_tokens,
        cache_write_tokens,
        billable_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        cost,
        model_breakdowns,
    }
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
        ];
        if self.cache_write_tokens > 0 {
            spans.push(Span::from(format_token_count(self.cache_write_tokens)));
            spans.push(Span::from(" cache writes, "));
        }
        spans.push(Span::from(format_token_count(self.billable_input_tokens)));
        spans.push(Span::from(" billable"));
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

    pub(crate) fn has_multiple_models(&self) -> bool {
        self.model_breakdowns.len() > 1
    }

    pub(crate) fn model_spans(&self) -> Vec<Vec<Span<'static>>> {
        self.model_breakdowns
            .iter()
            .map(|breakdown| {
                let mut spans = vec![
                    Span::from(breakdown.model.clone()),
                    Span::from(": "),
                    Span::from(format_token_count(breakdown.total_tokens)),
                    Span::from(" API-equivalent tokens"),
                ];
                push_optional_cost(
                    &mut spans,
                    breakdown.cost.as_ref().map(|cost| cost.total_usd),
                );
                spans
            })
            .collect()
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
    super::helpers::format_tokens_compact(tokens)
}

fn cost_for_tokens(tokens: i64, usd_per_1m: f64) -> f64 {
    tokens.max(0) as f64 * usd_per_1m / TOKENS_PER_MILLION
}

fn input_token_breakdown(usage: &TokenUsage) -> (i64, i64, i64, i64) {
    let input_tokens = usage.input_tokens.max(0);
    let cached_input_tokens = usage.cached_input().max(0).min(input_tokens);
    let non_cached_input_tokens = input_tokens - cached_input_tokens;
    let cache_write_tokens = usage.cache_write_tokens.max(0).min(non_cached_input_tokens);
    let billable_input_tokens = non_cached_input_tokens - cache_write_tokens;

    (
        input_tokens,
        cached_input_tokens,
        cache_write_tokens,
        billable_input_tokens,
    )
}

fn cost_for_usage_by_service_tier(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
    usage: &TokenUsage,
    usage_by_service_tier: &BTreeMap<String, TokenUsage>,
    usage_by_service_tier_and_context_length: &BTreeMap<String, BTreeMap<String, TokenUsage>>,
) -> Option<StatusTokenUsageCostBreakdown> {
    if usage_by_service_tier_and_context_length.is_empty() {
        return cost_for_usage_by_service_tier_short(
            config,
            model_provider_id,
            model,
            usage,
            usage_by_service_tier,
        );
    }

    let mut total_cost = StatusTokenUsageCostBreakdown::default();
    let mut covered_usage = TokenUsage::default();
    let mut covered_usage_by_service_tier = BTreeMap::new();
    for (service_tier, context_usages) in usage_by_service_tier_and_context_length {
        for (context_length, context_usage) in context_usages {
            let context_length = ContextLength::from_key(context_length)?;
            let rate = rates_for_model(
                config,
                model_provider_id,
                model,
                service_tier,
                context_length,
            )?;
            total_cost.add_assign(cost_for_usage(context_usage, rate));
            add_usage(&mut covered_usage, context_usage);
            add_usage(
                covered_usage_by_service_tier
                    .entry(service_tier.clone())
                    .or_default(),
                context_usage,
            );
        }
    }

    for (service_tier, tier_usage) in usage_by_service_tier {
        let covered = covered_usage_by_service_tier
            .get(service_tier)
            .cloned()
            .unwrap_or_default();
        let unbucketed_usage = unbucketed_usage(tier_usage, &covered);
        if !unbucketed_usage.is_zero() {
            let rate = rates_for_model(
                config,
                model_provider_id,
                model,
                service_tier,
                ContextLength::Short,
            )?;
            total_cost.add_assign(cost_for_usage(&unbucketed_usage, rate));
            add_usage(&mut covered_usage, &unbucketed_usage);
        }
    }

    let unbucketed_usage = unbucketed_usage(usage, &covered_usage);
    if !unbucketed_usage.is_zero() {
        let rate = rates_for_model(
            config,
            model_provider_id,
            model,
            TOKEN_USAGE_STANDARD_SERVICE_TIER,
            ContextLength::Short,
        )?;
        total_cost.add_assign(cost_for_usage(&unbucketed_usage, rate));
    }

    Some(total_cost)
}

fn cost_for_usage_by_service_tier_short(
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
            ContextLength::Short,
        )?;
        return Some(cost_for_usage(usage, rate));
    }

    let mut total_cost = StatusTokenUsageCostBreakdown::default();
    let mut covered_usage = TokenUsage::default();
    for (service_tier, tier_usage) in usage_by_service_tier {
        let rate = rates_for_model(
            config,
            model_provider_id,
            model,
            service_tier,
            ContextLength::Short,
        )?;
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
            ContextLength::Short,
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
    let (_, cached_input_tokens, cache_write_tokens, billable_input_tokens) =
        input_token_breakdown(usage);
    let output_tokens = usage.output_tokens.max(0);
    let input_usd = cost_for_tokens(billable_input_tokens, rate.input_usd_per_1m)
        + cost_for_tokens(cached_input_tokens, rate.cached_input_usd_per_1m)
        + cost_for_tokens(cache_write_tokens, rate.cache_write_usd_per_1m);
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
    total.cache_write_tokens += usage.cache_write_tokens;
    total.output_tokens += usage.output_tokens;
    total.reasoning_output_tokens += usage.reasoning_output_tokens;
    total.total_tokens += usage.total_tokens;
}

fn unbucketed_usage(total: &TokenUsage, covered: &TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: (total.input_tokens - covered.input_tokens).max(0),
        cached_input_tokens: (total.cached_input_tokens - covered.cached_input_tokens).max(0),
        cache_write_tokens: (total.cache_write_tokens - covered.cache_write_tokens).max(0),
        output_tokens: (total.output_tokens - covered.output_tokens).max(0),
        reasoning_output_tokens: (total.reasoning_output_tokens - covered.reasoning_output_tokens)
            .max(0),
        total_tokens: (total.total_tokens - covered.total_tokens).max(0),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextLength {
    Short,
    Long,
}

impl ContextLength {
    fn from_key(key: &str) -> Option<Self> {
        match key {
            TOKEN_USAGE_SHORT_CONTEXT => Some(Self::Short),
            TOKEN_USAGE_LONG_CONTEXT => Some(Self::Long),
            _ => None,
        }
    }
}

fn rates_for_model(
    config: &TuiStatusTokenUsage,
    model_provider_id: &str,
    model: &str,
    service_tier: &str,
    context_length: ContextLength,
) -> Option<ResolvedRate> {
    let service_tier = canonical_service_tier(service_tier);
    if let Some(rate) = config.model_rates.get(model) {
        if is_standard_service_tier(service_tier) {
            return Some(rate.into());
        }
        return rate.service_tiers.get(service_tier).map(Into::into);
    }

    built_in_rate_for_model(model_provider_id, model, service_tier, context_length)
}

fn canonical_service_tier(service_tier: &str) -> &str {
    match service_tier {
        "fast" => "priority",
        "batch" => "flex",
        _ => service_tier,
    }
}

fn built_in_rate_for_model(
    model_provider_id: &str,
    model: &str,
    service_tier: &str,
    context_length: ContextLength,
) -> Option<ResolvedRate> {
    if model_provider_id != "openai" {
        return None;
    }

    let model = model.to_ascii_lowercase();
    let service_tier = if is_standard_service_tier(service_tier) {
        TOKEN_USAGE_STANDARD_SERVICE_TIER
    } else {
        service_tier
    };
    match (model.as_str(), service_tier, context_length) {
        // Keep this table aligned with public OpenAI API processing rates.
        // Unknown or newly added models still render tokens without cost until rates
        // are added here or supplied through config overrides.
        (
            "gpt-5.3-codex",
            TOKEN_USAGE_STANDARD_SERVICE_TIER,
            ContextLength::Short | ContextLength::Long,
        ) => Some(rate(
            /*input_usd_per_1m*/ 1.75, /*cached_input_usd_per_1m*/ 0.175,
            /*output_usd_per_1m*/ 14.0,
        )),
        ("gpt-5.4", TOKEN_USAGE_STANDARD_SERVICE_TIER, ContextLength::Short) => Some(rate(
            /*input_usd_per_1m*/ 2.5, /*cached_input_usd_per_1m*/ 0.25,
            /*output_usd_per_1m*/ 15.0,
        )),
        (
            "gpt-5.4-mini",
            TOKEN_USAGE_STANDARD_SERVICE_TIER,
            ContextLength::Short | ContextLength::Long,
        ) => {
            Some(rate(
                /*input_usd_per_1m*/ 0.75, /*cached_input_usd_per_1m*/ 0.075,
                /*output_usd_per_1m*/ 4.5,
            ))
        }
        (
            "gpt-5.4-nano",
            TOKEN_USAGE_STANDARD_SERVICE_TIER,
            ContextLength::Short | ContextLength::Long,
        ) => {
            Some(rate(
                /*input_usd_per_1m*/ 0.20, /*cached_input_usd_per_1m*/ 0.02,
                /*output_usd_per_1m*/ 1.25,
            ))
        }
        ("gpt-5.5", TOKEN_USAGE_STANDARD_SERVICE_TIER, ContextLength::Short) => Some(rate(
            /*input_usd_per_1m*/ 5.0, /*cached_input_usd_per_1m*/ 0.50,
            /*output_usd_per_1m*/ 30.0,
        )),
        ("gpt-5.6" | "gpt-5.6-sol", TOKEN_USAGE_STANDARD_SERVICE_TIER, ContextLength::Short) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 5.0, /*cached_input_usd_per_1m*/ 0.50,
                /*cache_write_usd_per_1m*/ 6.25, /*output_usd_per_1m*/ 30.0,
            ))
        }
        ("gpt-5.6-terra", TOKEN_USAGE_STANDARD_SERVICE_TIER, ContextLength::Short) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 2.0, /*cached_input_usd_per_1m*/ 0.20,
                /*cache_write_usd_per_1m*/ 2.50, /*output_usd_per_1m*/ 12.0,
            ))
        }
        ("gpt-5.6-luna", TOKEN_USAGE_STANDARD_SERVICE_TIER, ContextLength::Short) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 0.20, /*cached_input_usd_per_1m*/ 0.02,
                /*cache_write_usd_per_1m*/ 0.25, /*output_usd_per_1m*/ 1.20,
            ))
        }
        ("gpt-5.4", TOKEN_USAGE_STANDARD_SERVICE_TIER, ContextLength::Long) => Some(rate(
            /*input_usd_per_1m*/ 5.0, /*cached_input_usd_per_1m*/ 0.50,
            /*output_usd_per_1m*/ 22.5,
        )),
        ("gpt-5.5", TOKEN_USAGE_STANDARD_SERVICE_TIER, ContextLength::Long) => Some(rate(
            /*input_usd_per_1m*/ 10.0, /*cached_input_usd_per_1m*/ 1.0,
            /*output_usd_per_1m*/ 45.0,
        )),
        ("gpt-5.6" | "gpt-5.6-sol", TOKEN_USAGE_STANDARD_SERVICE_TIER, ContextLength::Long) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 10.0, /*cached_input_usd_per_1m*/ 1.0,
                /*cache_write_usd_per_1m*/ 12.50, /*output_usd_per_1m*/ 45.0,
            ))
        }
        ("gpt-5.6-terra", TOKEN_USAGE_STANDARD_SERVICE_TIER, ContextLength::Long) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 4.0, /*cached_input_usd_per_1m*/ 0.40,
                /*cache_write_usd_per_1m*/ 5.0, /*output_usd_per_1m*/ 18.0,
            ))
        }
        ("gpt-5.6-luna", TOKEN_USAGE_STANDARD_SERVICE_TIER, ContextLength::Long) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 0.40, /*cached_input_usd_per_1m*/ 0.04,
                /*cache_write_usd_per_1m*/ 0.50, /*output_usd_per_1m*/ 1.80,
            ))
        }
        ("gpt-5.4", "priority" | "fast", ContextLength::Short) => Some(rate(
            /*input_usd_per_1m*/ 5.0, /*cached_input_usd_per_1m*/ 0.50,
            /*output_usd_per_1m*/ 30.0,
        )),
        ("gpt-5.3-codex", "priority" | "fast", ContextLength::Short) => Some(rate(
            /*input_usd_per_1m*/ 3.5, /*cached_input_usd_per_1m*/ 0.35,
            /*output_usd_per_1m*/ 28.0,
        )),
        ("gpt-5.4-mini", "priority" | "fast", ContextLength::Short) => Some(rate(
            /*input_usd_per_1m*/ 1.50, /*cached_input_usd_per_1m*/ 0.15,
            /*output_usd_per_1m*/ 9.0,
        )),
        ("gpt-5.5", "priority" | "fast", ContextLength::Short) => Some(rate(
            /*input_usd_per_1m*/ 12.50, /*cached_input_usd_per_1m*/ 1.25,
            /*output_usd_per_1m*/ 75.0,
        )),
        ("gpt-5.6" | "gpt-5.6-sol", "priority" | "fast", ContextLength::Short) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 10.0, /*cached_input_usd_per_1m*/ 1.0,
                /*cache_write_usd_per_1m*/ 12.50, /*output_usd_per_1m*/ 60.0,
            ))
        }
        ("gpt-5.6-terra", "priority" | "fast", ContextLength::Short) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 4.0, /*cached_input_usd_per_1m*/ 0.40,
                /*cache_write_usd_per_1m*/ 5.0, /*output_usd_per_1m*/ 24.0,
            ))
        }
        ("gpt-5.6-luna", "priority" | "fast", ContextLength::Short) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 0.40, /*cached_input_usd_per_1m*/ 0.04,
                /*cache_write_usd_per_1m*/ 0.50, /*output_usd_per_1m*/ 2.40,
            ))
        }
        ("gpt-5.4", "priority" | "fast", ContextLength::Long)
        | ("gpt-5.4-mini", "priority" | "fast", ContextLength::Long)
        | ("gpt-5.5", "priority" | "fast", ContextLength::Long)
        | ("gpt-5.6" | "gpt-5.6-sol", "priority" | "fast", ContextLength::Long)
        | ("gpt-5.6-terra", "priority" | "fast", ContextLength::Long)
        | ("gpt-5.6-luna", "priority" | "fast", ContextLength::Long) => None,
        ("gpt-5.4", "flex" | "batch", ContextLength::Short) => Some(rate(
            /*input_usd_per_1m*/ 1.25, /*cached_input_usd_per_1m*/ 0.13,
            /*output_usd_per_1m*/ 7.50,
        )),
        ("gpt-5.4-mini", "flex" | "batch", ContextLength::Short | ContextLength::Long) => {
            Some(rate(
                /*input_usd_per_1m*/ 0.375, /*cached_input_usd_per_1m*/ 0.0375,
                /*output_usd_per_1m*/ 2.25,
            ))
        }
        ("gpt-5.4-nano", "flex" | "batch", ContextLength::Short | ContextLength::Long) => {
            Some(rate(
                /*input_usd_per_1m*/ 0.10, /*cached_input_usd_per_1m*/ 0.01,
                /*output_usd_per_1m*/ 0.625,
            ))
        }
        ("gpt-5.5", "flex" | "batch", ContextLength::Short) => Some(rate(
            /*input_usd_per_1m*/ 2.50, /*cached_input_usd_per_1m*/ 0.25,
            /*output_usd_per_1m*/ 15.0,
        )),
        ("gpt-5.6" | "gpt-5.6-sol", "flex" | "batch", ContextLength::Short) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 2.50, /*cached_input_usd_per_1m*/ 0.25,
                /*cache_write_usd_per_1m*/ 3.125, /*output_usd_per_1m*/ 15.0,
            ))
        }
        ("gpt-5.6-terra", "flex" | "batch", ContextLength::Short) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 1.0, /*cached_input_usd_per_1m*/ 0.10,
                /*cache_write_usd_per_1m*/ 1.25, /*output_usd_per_1m*/ 6.0,
            ))
        }
        ("gpt-5.6-luna", "flex" | "batch", ContextLength::Short) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 0.10, /*cached_input_usd_per_1m*/ 0.01,
                /*cache_write_usd_per_1m*/ 0.125, /*output_usd_per_1m*/ 0.60,
            ))
        }
        ("gpt-5.4", "flex" | "batch", ContextLength::Long) => Some(rate(
            /*input_usd_per_1m*/ 2.50, /*cached_input_usd_per_1m*/ 0.25,
            /*output_usd_per_1m*/ 11.25,
        )),
        ("gpt-5.5", "flex" | "batch", ContextLength::Long) => Some(rate(
            /*input_usd_per_1m*/ 5.0, /*cached_input_usd_per_1m*/ 0.50,
            /*output_usd_per_1m*/ 22.50,
        )),
        ("gpt-5.6" | "gpt-5.6-sol", "flex" | "batch", ContextLength::Long) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 5.0, /*cached_input_usd_per_1m*/ 0.50,
                /*cache_write_usd_per_1m*/ 6.25, /*output_usd_per_1m*/ 22.50,
            ))
        }
        ("gpt-5.6-terra", "flex" | "batch", ContextLength::Long) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 2.0, /*cached_input_usd_per_1m*/ 0.20,
                /*cache_write_usd_per_1m*/ 2.50, /*output_usd_per_1m*/ 9.0,
            ))
        }
        ("gpt-5.6-luna", "flex" | "batch", ContextLength::Long) => {
            Some(rate_with_cache_write(
                /*input_usd_per_1m*/ 0.20, /*cached_input_usd_per_1m*/ 0.02,
                /*cache_write_usd_per_1m*/ 0.25, /*output_usd_per_1m*/ 0.90,
            ))
        }
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
    rate_with_cache_write(
        input_usd_per_1m,
        cached_input_usd_per_1m,
        /*cache_write_usd_per_1m*/ 0.0,
        output_usd_per_1m,
    )
}

fn rate_with_cache_write(
    input_usd_per_1m: f64,
    cached_input_usd_per_1m: f64,
    cache_write_usd_per_1m: f64,
    output_usd_per_1m: f64,
) -> ResolvedRate {
    ResolvedRate {
        input_usd_per_1m,
        cached_input_usd_per_1m,
        cache_write_usd_per_1m,
        output_usd_per_1m,
    }
}

impl From<&TuiStatusTokenUsageRate> for ResolvedRate {
    fn from(rate: &TuiStatusTokenUsageRate) -> Self {
        Self {
            input_usd_per_1m: rate.input_usd_per_1m,
            cached_input_usd_per_1m: rate.cached_input_usd_per_1m,
            cache_write_usd_per_1m: rate.cache_write_usd_per_1m,
            output_usd_per_1m: rate.output_usd_per_1m,
        }
    }
}

impl From<&TuiStatusTokenUsageServiceTierRate> for ResolvedRate {
    fn from(rate: &TuiStatusTokenUsageServiceTierRate) -> Self {
        Self {
            input_usd_per_1m: rate.input_usd_per_1m,
            cached_input_usd_per_1m: rate.cached_input_usd_per_1m,
            cache_write_usd_per_1m: rate.cache_write_usd_per_1m,
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
            cache_write_usd_per_1m: 0.0,
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
            cache_write_usd_per_1m: 0.0,
            output_usd_per_1m,
        }
    }

    #[test]
    fn cost_uses_built_in_model_rates() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            daily_spend_retention_days: 30,
            model_rates: BTreeMap::new(),
        };
        let usage = TokenUsage {
            input_tokens: 151_800,
            cached_input_tokens: 119_400,
            cache_write_tokens: 0,
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
            "184K API-equivalent tokens  ~$0.53"
        );
        assert_eq!(
            span_text(data.input_spans()),
            "152K total, 119K cached, 32.4K billable  ~$0.08"
        );
        assert_eq!(
            span_text(data.output_spans()),
            "32.4K total, 8.7K reasoning  ~$0.45"
        );
    }

    #[test]
    fn token_counts_roll_up_to_billions_and_trillions() {
        assert_eq!(format_token_count(1_000_000_000), "1B");
        assert_eq!(format_token_count(1_234_000_000), "1.23B");
        assert_eq!(format_token_count(1_000_000_000_000), "1T");
    }

    #[test]
    fn cost_keeps_multiple_models_separate() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            daily_spend_retention_days: 30,
            model_rates: BTreeMap::from([
                ("model-a".to_string(), model_rate(2.0, 1.0, 4.0)),
                ("model-b".to_string(), model_rate(10.0, 5.0, 20.0)),
            ]),
        };
        let model_a_usage = TokenUsage {
            input_tokens: 1_000_000,
            total_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let model_b_usage = TokenUsage {
            input_tokens: 2_000_000,
            total_tokens: 2_000_000,
            ..TokenUsage::default()
        };
        let usage = TokenUsage {
            input_tokens: 3_000_000,
            total_tokens: 3_000_000,
            ..TokenUsage::default()
        };
        let data = compose_status_token_usage_cost_with_models(
            &config,
            "custom",
            "model-a",
            &usage,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::from([
                ("model-a".to_string(), model_a_usage),
                ("model-b".to_string(), model_b_usage),
            ]),
            &BTreeMap::new(),
        )
        .expect("usage should render");

        assert!(data.has_multiple_models());
        assert_eq!(
            data.model_spans()
                .into_iter()
                .map(span_text)
                .collect::<Vec<_>>(),
            vec![
                "model-a: 1M API-equivalent tokens  ~$2.00",
                "model-b: 2M API-equivalent tokens  ~$20.00",
            ]
        );
        assert_eq!(
            span_text(data.summary_spans()),
            "3M API-equivalent tokens  ~$22.00"
        );
    }

    #[test]
    fn cost_keeps_legacy_usage_visible_when_new_model_buckets_appear() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            daily_spend_retention_days: 30,
            model_rates: BTreeMap::from([("model-a".to_string(), model_rate(2.0, 1.0, 4.0))]),
        };
        let usage = TokenUsage {
            input_tokens: 3_000_000,
            total_tokens: 3_000_000,
            ..TokenUsage::default()
        };
        let data = compose_status_token_usage_cost_with_models(
            &config,
            "custom",
            "model-a",
            &usage,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::from([(
                "model-a".to_string(),
                TokenUsage {
                    input_tokens: 1_000_000,
                    total_tokens: 1_000_000,
                    ..TokenUsage::default()
                },
            )]),
            &BTreeMap::new(),
        )
        .expect("usage should render");

        assert_eq!(
            data.model_spans()
                .into_iter()
                .map(span_text)
                .collect::<Vec<_>>(),
            vec![
                "model-a: 1M API-equivalent tokens  ~$2.00",
                "unattributed: 2M API-equivalent tokens  cost unavailable",
            ]
        );
        assert_eq!(
            span_text(data.summary_spans()),
            "3M API-equivalent tokens  cost unavailable"
        );
    }

    #[test]
    fn cost_uses_configured_model_rate_overrides() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            daily_spend_retention_days: 30,
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
            cache_write_tokens: 0,
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
            daily_spend_retention_days: 30,
            model_rates: BTreeMap::new(),
        };
        let usage = TokenUsage {
            input_tokens: 100_000,
            cached_input_tokens: 20_000,
            cache_write_tokens: 0,
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
            "110K API-equivalent tokens  ~$1.77"
        );
        assert_eq!(
            span_text(data.input_spans()),
            "100K total, 20K cached, 80K billable  ~$1.02"
        );
        assert_eq!(
            span_text(data.output_spans()),
            "10K total, 5K reasoning  ~$0.75"
        );
    }

    #[test]
    fn cost_uses_gpt_5_6_cache_write_rates() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            daily_spend_retention_days: 30,
            model_rates: BTreeMap::new(),
        };
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 200_000,
            cache_write_tokens: 100_000,
            output_tokens: 500_000,
            reasoning_output_tokens: 100_000,
            total_tokens: 1_500_000,
        };
        let usage_by_service_tier = BTreeMap::from([("flex".to_string(), usage.clone())]);

        let data = compose_status_token_usage_cost(
            &config,
            "openai",
            "gpt-5.6-terra",
            &usage,
            &usage_by_service_tier,
        )
        .expect("usage should render");

        assert_eq!(
            span_text(data.summary_spans()),
            "1.5M API-equivalent tokens  ~$3.84"
        );
        assert_eq!(
            span_text(data.input_spans()),
            "1M total, 200K cached, 100K cache writes, 700K billable  ~$0.84"
        );
        assert_eq!(
            span_text(data.output_spans()),
            "500K total, 100K reasoning  ~$3.00"
        );
    }

    #[test]
    fn cost_uses_long_context_rates_from_per_response_usage_buckets() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            daily_spend_retention_days: 30,
            model_rates: BTreeMap::new(),
        };
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 200_000,
            cache_write_tokens: 100_000,
            output_tokens: 500_000,
            reasoning_output_tokens: 100_000,
            total_tokens: 1_500_000,
        };
        let usage_by_service_tier =
            BTreeMap::from([(TOKEN_USAGE_STANDARD_SERVICE_TIER.to_string(), usage.clone())]);
        let usage_by_service_tier_and_context_length = BTreeMap::from([(
            TOKEN_USAGE_STANDARD_SERVICE_TIER.to_string(),
            BTreeMap::from([(TOKEN_USAGE_LONG_CONTEXT.to_string(), usage.clone())]),
        )]);

        let data = compose_status_token_usage_cost_with_context_length(
            &config,
            "openai",
            "gpt-5.6-terra",
            &usage,
            &usage_by_service_tier,
            &usage_by_service_tier_and_context_length,
        )
        .expect("usage should render");

        assert_eq!(
            span_text(data.summary_spans()),
            "1.5M API-equivalent tokens  ~$12.38"
        );
        assert_eq!(
            span_text(data.input_spans()),
            "1M total, 200K cached, 100K cache writes, 700K billable  ~$3.38"
        );
        assert_eq!(
            span_text(data.output_spans()),
            "500K total, 100K reasoning  ~$9.00"
        );
    }

    #[test]
    fn cost_does_not_double_count_tier_aggregate_after_context_bucketing() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            daily_spend_retention_days: 30,
            model_rates: BTreeMap::new(),
        };
        let long_usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            total_tokens: 1_100_000,
            ..TokenUsage::default()
        };
        let short_usage = TokenUsage {
            input_tokens: 500_000,
            output_tokens: 50_000,
            total_tokens: 550_000,
            ..TokenUsage::default()
        };
        let usage = TokenUsage {
            input_tokens: long_usage.input_tokens + short_usage.input_tokens,
            cached_input_tokens: long_usage.cached_input_tokens + short_usage.cached_input_tokens,
            cache_write_tokens: long_usage.cache_write_tokens + short_usage.cache_write_tokens,
            output_tokens: long_usage.output_tokens + short_usage.output_tokens,
            reasoning_output_tokens: long_usage.reasoning_output_tokens
                + short_usage.reasoning_output_tokens,
            total_tokens: long_usage.total_tokens + short_usage.total_tokens,
        };
        let usage_by_service_tier =
            BTreeMap::from([(TOKEN_USAGE_STANDARD_SERVICE_TIER.to_string(), usage.clone())]);
        let usage_by_service_tier_and_context_length = BTreeMap::from([(
            TOKEN_USAGE_STANDARD_SERVICE_TIER.to_string(),
            BTreeMap::from([(TOKEN_USAGE_LONG_CONTEXT.to_string(), long_usage)]),
        )]);

        let data = compose_status_token_usage_cost_with_context_length(
            &config,
            "openai",
            "gpt-5.4",
            &usage,
            &usage_by_service_tier,
            &usage_by_service_tier_and_context_length,
        )
        .expect("usage should render");

        assert_eq!(
            span_text(data.summary_spans()),
            "1.65M API-equivalent tokens  ~$9.25"
        );
    }

    #[test]
    fn models_without_long_context_surcharge_reuse_short_rates() {
        let config = TuiStatusTokenUsage {
            enabled: true,
            daily_spend_retention_days: 30,
            model_rates: BTreeMap::new(),
        };
        let usage = TokenUsage {
            input_tokens: 100_000,
            output_tokens: 10_000,
            total_tokens: 110_000,
            ..TokenUsage::default()
        };
        let usage_by_service_tier =
            BTreeMap::from([(TOKEN_USAGE_STANDARD_SERVICE_TIER.to_string(), usage.clone())]);
        let short_buckets = BTreeMap::from([(
            TOKEN_USAGE_STANDARD_SERVICE_TIER.to_string(),
            BTreeMap::from([(TOKEN_USAGE_SHORT_CONTEXT.to_string(), usage.clone())]),
        )]);
        let long_buckets = BTreeMap::from([(
            TOKEN_USAGE_STANDARD_SERVICE_TIER.to_string(),
            BTreeMap::from([(TOKEN_USAGE_LONG_CONTEXT.to_string(), usage.clone())]),
        )]);

        let short = compose_status_token_usage_cost_with_context_length(
            &config,
            "openai",
            "gpt-5.4-mini",
            &usage,
            &usage_by_service_tier,
            &short_buckets,
        )
        .expect("short-context usage should render");
        let long = compose_status_token_usage_cost_with_context_length(
            &config,
            "openai",
            "gpt-5.4-mini",
            &usage,
            &usage_by_service_tier,
            &long_buckets,
        )
        .expect("long-context usage should render");

        assert_eq!(short.cost, long.cost);
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
            daily_spend_retention_days: 30,
            model_rates: BTreeMap::from([("custom-model".to_string(), custom_rate)]),
        };
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 250_000,
            cache_write_tokens: 0,
            output_tokens: 500_000,
            reasoning_output_tokens: 100_000,
            total_tokens: 1_500_000,
        };
        let usage_by_service_tier = BTreeMap::from([("fast".to_string(), usage.clone())]);

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
            daily_spend_retention_days: 30,
            model_rates: BTreeMap::new(),
        };
        let usage = TokenUsage {
            input_tokens: 1_000,
            cached_input_tokens: 100,
            cache_write_tokens: 0,
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
            daily_spend_retention_days: 30,
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
