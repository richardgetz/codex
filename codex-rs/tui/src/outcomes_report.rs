use codex_protocol::ThreadId;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const SPARKLINE_BLOCKS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

#[derive(Debug, Clone)]
struct OutcomePoint {
    value: f64,
    label: String,
    summary: Option<String>,
    commit: Option<String>,
    pr: Option<String>,
}

#[derive(Debug)]
struct OutcomeSeries {
    scope: String,
    metric: String,
    unit: Option<String>,
    direction: Option<String>,
    points: Vec<OutcomePoint>,
}

type OutcomeSeriesKey = (String, String, Option<String>, Option<String>);

pub(crate) fn markdown(thread_id: ThreadId, objective: &str, outcomes: &[Value]) -> String {
    let mut lines = vec![
        format!("# Outcomes for {objective}"),
        String::new(),
        format!("Scratchpad: `{thread_id}`"),
        String::new(),
    ];
    if outcomes.is_empty() {
        lines.push("No outcomes recorded.".to_string());
        return lines.join("\n");
    }

    let series = outcome_series(outcomes);
    if series.is_empty() {
        append_raw_outcomes(&mut lines, outcomes);
        return lines.join("\n");
    }

    for item in &series {
        let values: Vec<f64> = item.points.iter().map(|point| point.value).collect();
        let first = values.first().copied().unwrap_or_default();
        let last = values.last().copied().unwrap_or(first);
        let unit = item.unit.as_deref().unwrap_or("");
        lines.push(format!("## {} - {}", item.scope, item.metric));
        lines.push(format!(
            "{} -> {}{}   {}",
            format_number(first),
            format_number(last),
            unit_suffix(unit).as_str(),
            percent_change(first, last)
        ));
        if let Some(direction) = item.direction.as_deref() {
            lines.push(format!("Direction: {direction}"));
        }
        lines.push(sparkline(&values));
        lines.push(String::new());
    }

    let mut proof = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_object())
        .filter_map(|object| {
            let summary = object.get("summary").and_then(Value::as_str)?;
            let scope = object
                .get("scope")
                .map(format_value)
                .unwrap_or_else(|| "general".to_string());
            let commit = object.get("commit").and_then(Value::as_str);
            let suffix = commit
                .map(|commit| format!(" at {commit}"))
                .unwrap_or_default();
            Some(format!("- {scope}: {summary}{suffix}"))
        })
        .take(5)
        .collect::<Vec<_>>();
    if !proof.is_empty() {
        lines.push("## Recent proof".to_string());
        lines.append(&mut proof);
        lines.push(String::new());
    }

    lines.join("\n")
}

pub(crate) fn write_html_report(
    codex_home: &Path,
    thread_id: ThreadId,
    objective: &str,
    outcomes: &[Value],
) -> io::Result<PathBuf> {
    let report_dir = codex_home.join("scratchpad").join("reports");
    std::fs::create_dir_all(&report_dir)?;
    let path = report_dir.join(format!("{thread_id}-outcomes.html"));
    std::fs::write(&path, html_report(thread_id, objective, outcomes))?;
    Ok(path)
}

fn outcome_series(outcomes: &[Value]) -> Vec<OutcomeSeries> {
    let mut grouped: BTreeMap<OutcomeSeriesKey, Vec<OutcomePoint>> = BTreeMap::new();
    for outcome in outcomes {
        let Some(object) = outcome.as_object() else {
            continue;
        };
        let Some(metric) = object.get("metric").map(format_value) else {
            continue;
        };
        let scope = object
            .get("scope")
            .map(format_value)
            .unwrap_or_else(|| "general".to_string());
        let unit = object
            .get("unit")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let direction = object
            .get("direction")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        if let Some(baseline) = object.get("baseline").and_then(number_value) {
            grouped
                .entry((
                    scope.clone(),
                    metric.clone(),
                    unit.clone(),
                    direction.clone(),
                ))
                .or_default()
                .push(OutcomePoint {
                    value: baseline,
                    label: "baseline".to_string(),
                    summary: None,
                    commit: None,
                    pr: None,
                });
        }
        let value = object
            .get("current")
            .and_then(number_value)
            .or_else(|| object.get("value").and_then(number_value));
        if let Some(value) = value {
            grouped
                .entry((scope, metric, unit, direction))
                .or_default()
                .push(OutcomePoint {
                    value,
                    label: object
                        .get("recorded_at")
                        .and_then(Value::as_str)
                        .unwrap_or("recorded")
                        .to_string(),
                    summary: object
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    commit: object
                        .get("commit")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    pr: object
                        .get("pr")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                });
        }
    }
    grouped
        .into_iter()
        .filter_map(|((scope, metric, unit, direction), points)| {
            (!points.is_empty()).then_some(OutcomeSeries {
                scope,
                metric,
                unit,
                direction,
                points,
            })
        })
        .collect()
}

fn html_report(thread_id: ThreadId, objective: &str, outcomes: &[Value]) -> String {
    let series = outcome_series(outcomes);
    let mut html = String::new();
    html.push_str("<!doctype html><html><head><meta charset=\"utf-8\"><title>Outcomes</title>");
    html.push_str("<style>body{font:14px system-ui,sans-serif;margin:32px;color:#202124;background:#fbfbf8}h1{font-size:28px;margin:0 0 4px}h2{font-size:18px;margin:0 0 12px}.muted{color:#666}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(320px,1fr));gap:16px;margin-top:24px}.card{background:white;border:1px solid #ddd;border-radius:8px;padding:16px}.metric{font-size:24px;font-weight:700}.proof{margin-top:24px}svg{width:100%;height:160px;overflow:visible}.axis{stroke:#ddd}.line{fill:none;stroke:#2563eb;stroke-width:3}.dot{fill:#2563eb}</style></head><body>");
    write!(html, "<h1>{}</h1>", escape_html(objective)).ok();
    write!(
        html,
        "<div class=\"muted\">Scratchpad: <code>{}</code></div>",
        escape_html(&thread_id.to_string())
    )
    .ok();
    if series.is_empty() {
        html.push_str("<p>No numeric outcomes recorded.</p>");
    } else {
        html.push_str("<div class=\"grid\">");
        for item in &series {
            append_html_series(&mut html, item);
        }
        html.push_str("</div>");
    }
    append_html_proof(&mut html, outcomes);
    html.push_str("</body></html>");
    html
}

fn append_html_series(html: &mut String, series: &OutcomeSeries) {
    let values: Vec<f64> = series.points.iter().map(|point| point.value).collect();
    let first = values.first().copied().unwrap_or_default();
    let last = values.last().copied().unwrap_or(first);
    let unit = series.unit.as_deref().unwrap_or("");
    write!(
        html,
        "<section class=\"card\"><h2>{} - {}</h2><div class=\"metric\">{} -> {}{}</div><div class=\"muted\">{}</div>{}",
        escape_html(&series.scope),
        escape_html(&series.metric),
        format_number(first),
        format_number(last),
        escape_html(&unit_suffix(unit)),
        escape_html(&percent_change(first, last)),
        line_svg(&values)
    )
    .ok();
    html.push_str("<ol>");
    for point in &series.points {
        write!(
            html,
            "<li><span class=\"muted\">{}</span>: {}{}",
            escape_html(&point.label),
            format_number(point.value),
            escape_html(&unit_suffix(unit))
        )
        .ok();
        if let Some(summary) = &point.summary {
            write!(html, " - {}", escape_html(summary)).ok();
        }
        if let Some(commit) = &point.commit {
            write!(html, " <code>{}</code>", escape_html(commit)).ok();
        }
        if let Some(pr) = &point.pr {
            write!(html, " <span class=\"muted\">PR {}</span>", escape_html(pr)).ok();
        }
        html.push_str("</li>");
    }
    html.push_str("</ol></section>");
}

fn append_html_proof(html: &mut String, outcomes: &[Value]) {
    let proof = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_object())
        .filter_map(|object| {
            let summary = object.get("summary").and_then(Value::as_str)?;
            let scope = object
                .get("scope")
                .map(format_value)
                .unwrap_or_else(|| "general".to_string());
            Some((scope, summary.to_string()))
        })
        .take(8)
        .collect::<Vec<_>>();
    if proof.is_empty() {
        return;
    }
    html.push_str("<section class=\"proof\"><h2>Recent proof</h2><ul>");
    for (scope, summary) in proof {
        write!(
            html,
            "<li><strong>{}</strong>: {}</li>",
            escape_html(&scope),
            escape_html(&summary)
        )
        .ok();
    }
    html.push_str("</ul></section>");
}

fn append_raw_outcomes(lines: &mut Vec<String>, outcomes: &[Value]) {
    for outcome in outcomes {
        let object = outcome.as_object();
        let scope = object
            .and_then(|object| object.get("scope"))
            .map(format_value)
            .unwrap_or_else(|| "general".to_string());
        let metric = object
            .and_then(|object| object.get("metric"))
            .map(format_value)
            .unwrap_or_else(|| "metric".to_string());
        lines.push(format!("## {scope} - {metric}"));
        for key in [
            "baseline",
            "current",
            "value",
            "delta",
            "unit",
            "summary",
            "tradeoffs",
            "commit",
            "pr",
            "recorded_at",
        ] {
            if let Some(value) = object.and_then(|object| object.get(key)) {
                lines.push(format!("- {key}: {}", format_value(value)));
            }
        }
        lines.push(String::new());
    }
}

fn line_svg(values: &[f64]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let spread = (max - min).abs().max(1.0);
    let last_index = values.len().saturating_sub(1).max(1) as f64;
    let points = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let x = 20.0 + (index as f64 / last_index) * 260.0;
            let y = 130.0 - ((*value - min) / spread) * 100.0;
            (x, y)
        })
        .collect::<Vec<_>>();
    let path = points
        .iter()
        .enumerate()
        .map(|(index, (x, y))| {
            if index == 0 {
                format!("M {x:.1} {y:.1}")
            } else {
                format!("L {x:.1} {y:.1}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let dots = points
        .iter()
        .map(|(x, y)| format!("<circle class=\"dot\" cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"4\"/>"))
        .collect::<String>();
    format!(
        "<svg viewBox=\"0 0 300 150\" role=\"img\"><line class=\"axis\" x1=\"20\" y1=\"130\" x2=\"280\" y2=\"130\"/><path class=\"line\" d=\"{path}\"/>{dots}</svg>"
    )
}

fn sparkline(values: &[f64]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let spread = max - min;
    values
        .iter()
        .map(|value| {
            let index = if spread.abs() < f64::EPSILON {
                0
            } else {
                (((value - min) / spread) * (SPARKLINE_BLOCKS.len() - 1) as f64).round() as usize
            };
            SPARKLINE_BLOCKS[index.min(SPARKLINE_BLOCKS.len() - 1)]
        })
        .collect()
}

fn number_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse::<f64>().ok()))
}

fn format_value(value: &Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn format_number(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

fn percent_change(first: f64, last: f64) -> String {
    if first.abs() < f64::EPSILON {
        return "n/a".to_string();
    }
    let change = ((last - first) / first) * 100.0;
    format!("{change:+.0}%")
}

fn unit_suffix(unit: &str) -> String {
    if unit.is_empty() {
        String::new()
    } else if unit.chars().next().is_some_and(char::is_whitespace) {
        unit.to_string()
    } else {
        format!(" {unit}")
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_metric_trend_card() {
        let thread_id = ThreadId::new();
        let markdown = markdown(
            thread_id,
            "Improve throughput",
            &[
                serde_json::json!({
                    "scope": "vector-search",
                    "metric": "qps",
                    "unit": " req/s",
                    "baseline": 930,
                    "current": 1840,
                    "summary": "Sharded fanout improved hot-query throughput",
                    "commit": "abc123"
                }),
                serde_json::json!({
                    "scope": "vector-search",
                    "metric": "qps",
                    "unit": " req/s",
                    "current": 2200
                }),
            ],
        );
        assert!(markdown.contains("930 -> 2200 req/s"));
        assert!(markdown.contains("+137%"));
        assert!(markdown.contains("▁"));
        assert!(markdown.contains("█"));
        assert!(markdown.contains("Recent proof"));
    }
}
