//! `report.json` and `report.md`: per-probe results, totals and the acceptance criteria.

use serde::Serialize;
use serde_json::Value;

use super::model_calls::TokenUsage;
use super::probe_answering::ProbeResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    Pass,
    Fail,
    NotEvaluated,
}

impl Check {
    fn word(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::NotEvaluated => "not evaluated",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Criterion {
    pub name: String,
    pub check: Check,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AnswerTotals {
    pub correct: usize,
    pub partial: usize,
    pub wrong: usize,
    pub correct_rate: f64,
    pub average_input_tokens: f64,
    pub average_cached_tokens: f64,
    pub average_output_tokens: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FailureClasses {
    pub missing: usize,
    pub not_zoomed: usize,
    pub other: usize,
    pub unclassified: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Totals {
    pub probes: usize,
    pub view: AnswerTotals,
    pub baseline: AnswerTotals,
    pub zoom_calls_per_probe: f64,
    pub probes_reached_source: usize,
    pub probes_opened_source: usize,
    pub failure_classes: FailureClasses,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub totals: Totals,
    pub acceptance: Vec<Criterion>,
    pub probes: Vec<ProbeResult>,
    pub cost: Option<Value>,
}

fn answer_totals<'a>(entries: impl Iterator<Item = (&'a str, &'a TokenUsage)>) -> AnswerTotals {
    let mut totals = AnswerTotals::default();
    let (mut input, mut cached, mut output, mut count) = (0u64, 0u64, 0u64, 0usize);
    for (verdict, usage) in entries {
        match verdict {
            "correct" => totals.correct += 1,
            "partial" => totals.partial += 1,
            _ => totals.wrong += 1,
        }
        input += usage.input_tokens;
        cached += usage.cached_tokens;
        output += usage.output_tokens;
        count += 1;
    }
    if count > 0 {
        let divisor = count as f64;
        totals.correct_rate = totals.correct as f64 / divisor;
        totals.average_input_tokens = input as f64 / divisor;
        totals.average_cached_tokens = cached as f64 / divisor;
        totals.average_output_tokens = output as f64 / divisor;
    }
    totals
}

fn cost_per_turn(cost: &Value, case: &str) -> Option<f64> {
    cost.pointer(&format!("/report/{case}/per_turn"))?.as_f64()
}

fn cost_criterion(cost: Option<&Value>) -> Criterion {
    let name = "The computed per-turn input cost of the view layout is lower than the current layout's in both cache cases".to_string();
    let Some(cost) = cost else {
        return Criterion {
            name,
            check: Check::NotEvaluated,
            detail: "cost.json is missing; run build first".into(),
        };
    };
    let pairs = [
        ("warm", "current_warm", "view_warm"),
        ("expired", "current_expired", "view_expired"),
    ];
    let mut detail = Vec::new();
    let mut all_lower = true;
    for (label, current, view) in pairs {
        match (cost_per_turn(cost, current), cost_per_turn(cost, view)) {
            (Some(current_cost), Some(view_cost)) => {
                all_lower &= view_cost < current_cost;
                detail.push(format!(
                    "{label}: view {view_cost:.1} vs current {current_cost:.1}"
                ));
            }
            _ => {
                all_lower = false;
                detail.push(format!("{label}: cost missing"));
            }
        }
    }
    Criterion {
        name,
        check: if all_lower { Check::Pass } else { Check::Fail },
        detail: detail.join("; "),
    }
}

pub fn build_report(results: &[ProbeResult], cost: Option<Value>) -> Report {
    let view = answer_totals(results.iter().map(|result| {
        (
            result.view_verdict.verdict.as_str(),
            &result.view_answer.usage,
        )
    }));
    let baseline = answer_totals(results.iter().map(|result| {
        (
            result.baseline_verdict.verdict.as_str(),
            &result.baseline_answer.usage,
        )
    }));
    let mut classes = FailureClasses::default();
    for result in results
        .iter()
        .filter(|result| result.view_verdict.is_wrong())
    {
        match result.failure_class.as_str() {
            "missing" => classes.missing += 1,
            "not_zoomed" => classes.not_zoomed += 1,
            "other" => classes.other += 1,
            _ => classes.unclassified += 1,
        }
    }
    let zoom_calls: usize = results.iter().map(|result| result.zoom_calls.len()).sum();
    let totals = Totals {
        probes: results.len(),
        zoom_calls_per_probe: if results.is_empty() {
            0.0
        } else {
            zoom_calls as f64 / results.len() as f64
        },
        probes_reached_source: results.iter().filter(|result| result.reached).count(),
        probes_opened_source: results.iter().filter(|result| result.opened_source).count(),
        failure_classes: classes,
        view,
        baseline,
    };
    let rates_check = if results.is_empty() {
        Check::NotEvaluated
    } else if totals.view.correct_rate >= totals.baseline.correct_rate {
        Check::Pass
    } else {
        Check::Fail
    };
    let wrong_view = totals.view.wrong;
    let classified = wrong_view - totals.failure_classes.unclassified;
    let acceptance = vec![
        Criterion {
            name: "The view answers' correct rate is not lower than the baseline's".into(),
            check: rates_check,
            detail: format!(
                "view {:.3}, baseline {:.3}",
                totals.view.correct_rate, totals.baseline.correct_rate
            ),
        },
        Criterion {
            name: "Every wrong view answer is classified as missing, not_zoomed or other".into(),
            check: if results.is_empty() {
                Check::NotEvaluated
            } else if totals.failure_classes.unclassified == 0 {
                Check::Pass
            } else {
                Check::Fail
            },
            detail: format!(
                "{classified} of {wrong_view} classified: missing {}, not_zoomed {}, other {}",
                totals.failure_classes.missing,
                totals.failure_classes.not_zoomed,
                totals.failure_classes.other
            ),
        },
        cost_criterion(cost.as_ref()),
    ];
    Report {
        totals,
        acceptance,
        probes: results.to_vec(),
        cost,
    }
}

/// The cost section of `report.md`.
pub fn render_cost_section(cost: &Value) -> String {
    let mut text = String::from("## Computed input cost\n\n");
    text.push_str("These figures are computed from byte sizes and price multipliers. They are not measured from a provider.\n\n");
    text.push_str("| Layout | Cache | Per turn | Total | Main | Compressor | Cached tokens | Written tokens | Compactions |\n");
    text.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for (label, layout, cache) in [
        ("current", "current_warm", "warm"),
        ("current", "current_expired", "expired"),
        ("view", "view_warm", "warm"),
        ("view", "view_expired", "expired"),
    ] {
        let field = |name: &str| {
            cost.pointer(&format!("/report/{layout}/{name}"))
                .and_then(Value::as_f64)
                .unwrap_or(f64::NAN)
        };
        text.push_str(&format!(
            "| {label} | {cache} | {:.1} | {:.1} | {:.1} | {:.1} | {:.0} | {:.0} | {:.0} |\n",
            field("per_turn"),
            field("total"),
            field("main"),
            field("compressor"),
            field("cached_input_tokens"),
            field("written_input_tokens"),
            field("compactions"),
        ));
    }
    text.push_str("\nCosts are in units of one uncached input token.\n");
    if let Some(note) = cost.get("assumptions").and_then(Value::as_array) {
        text.push_str("\nAssumptions:\n\n");
        for item in note.iter().filter_map(Value::as_str) {
            text.push_str(&format!("- {item}\n"));
        }
    }
    text
}

fn answer_table_row(label: &str, totals: &AnswerTotals) -> String {
    format!(
        "| {label} | {} | {} | {} | {:.3} | {:.0} | {:.0} | {:.0} |\n",
        totals.correct,
        totals.partial,
        totals.wrong,
        totals.correct_rate,
        totals.average_input_tokens,
        totals.average_cached_tokens,
        totals.average_output_tokens
    )
}

pub fn render_markdown(report: &Report) -> String {
    let totals = &report.totals;
    let mut text = String::from("# View-tree evaluation\n\n## Acceptance criteria\n\n");
    for (index, criterion) in report.acceptance.iter().enumerate() {
        text.push_str(&format!(
            "{}. {}: **{}** ({})\n",
            index + 1,
            criterion.name,
            criterion.check.word(),
            criterion.detail
        ));
    }
    text.push_str(&format!("\n## Totals over {} probes\n\n", totals.probes));
    text.push_str("| Answer | Correct | Partial | Wrong | Correct rate | Input tokens | Cached tokens | Output tokens |\n");
    text.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    text.push_str(&answer_table_row("view", &totals.view));
    text.push_str(&answer_table_row("baseline", &totals.baseline));
    text.push_str(&format!(
        "\nToken columns are averages per answer. Zoom calls per probe: {:.2}. Probes where a zoom reached a source: {}. \
         Probes where zoom(source, 1) was called: {}.\n",
        totals.zoom_calls_per_probe, totals.probes_reached_source, totals.probes_opened_source
    ));
    text.push_str(&format!(
        "\nWrong view answers: missing {}, not_zoomed {}, other {}, unclassified {}.\n",
        totals.failure_classes.missing,
        totals.failure_classes.not_zoomed,
        totals.failure_classes.other,
        totals.failure_classes.unclassified
    ));
    if let Some(cost) = &report.cost {
        text.push('\n');
        text.push_str(&render_cost_section(cost));
    }
    text.push_str("\n## Probes\n");
    for result in &report.probes {
        text.push_str(&render_probe(result));
    }
    text
}

fn render_probe(result: &ProbeResult) -> String {
    let zoom: Vec<String> = result
        .zoom_calls
        .iter()
        .map(|call| format!("{} {}", call.tool, call.arguments))
        .collect();
    format!(
        "\n### {} (sources {:?})\n\nQuestion: {}\n\nExpected: {}\n\nView answer ({}): {}\n\nBaseline answer ({}): {}\n\n\
         Zoom calls: {}. Reached source: {}. Opened source: {}.{}\n",
        result.probe.id,
        result.probe.sources,
        result.probe.question,
        result.probe.expected,
        result.view_verdict.verdict,
        result.view_answer.text,
        result.baseline_verdict.verdict,
        result.baseline_answer.text,
        if zoom.is_empty() {
            "none".to_string()
        } else {
            zoom.join("; ")
        },
        result.reached,
        result.opened_source,
        if result.failure_class.is_empty() {
            String::new()
        } else {
            format!(" Failure class: {}.", result.failure_class)
        },
    ) + &format!(
        "\nView judge: {}\n\nBaseline judge: {}\n",
        result.view_verdict.reason, result.baseline_verdict.reason
    )
}
