//! `report.json` and `report.md`: run metadata, layer-3 counts per condition and
//! traceability, D rebuild counts, layer-2 metrics and judge agreement. Counts with sample
//! sizes; no threshold.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use otto::memory::proposals::evaluation::Metrics;
use otto_core::provider::Provider;
use serde::{Deserialize, Serialize};

use super::argument_parsing::{Condition, Options, Variant};
use super::conditions::{AnswerRecord, Role};
use super::items::{Item, Retirement};
use super::layer_two::ProposalRecord;
use super::provider_capture::RequestLog;
use crate::eval_command::model_calls::{EvalEnvironment, TokenUsage};

pub const ITEMS_FILE: &str = "items.json";
pub const ANSWERS_FILE: &str = "answers.jsonl";
pub const REPORT_JSON_FILE: &str = "report.json";
pub const REPORT_MARKDOWN_FILE: &str = "report.md";

/// What identifies a run: versions, model, settings, usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMetadata {
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub otto_version: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub items_version: String,
    pub labels_version: String,
    pub proposal_prompt_version: String,
    pub judge_prompt_version: String,
    pub layer_two_judge_prompt_version: String,
    pub conditions: Vec<Condition>,
    pub variants: Vec<Variant>,
    pub repeat: u32,
    pub judge: bool,
    pub max_calls: u32,
    pub usage: TokenUsage,
}

impl RunMetadata {
    pub fn collect<P: Provider + Clone + Send + Sync>(
        env: &Arc<EvalEnvironment<P>>,
        options: &Options,
        run_id: &str,
        started_at: DateTime<Utc>,
        log: &RequestLog,
    ) -> Self {
        let mut usage = TokenUsage::default();
        for request in log.since(0) {
            usage.add(&request.usage);
        }
        Self {
            run_id: run_id.to_owned(),
            started_at,
            finished_at: Utc::now(),
            otto_version: env!("CARGO_PKG_VERSION").to_owned(),
            provider: env.provider_name.clone(),
            model: env.profile_model.clone(),
            thinking: env.thinking.clone(),
            items_version: super::items::ITEMS_VERSION.to_owned(),
            labels_version: otto::memory::proposals::evaluation::DATASET_VERSION.to_owned(),
            proposal_prompt_version: otto::memory::proposals::PROMPT_VERSION.to_owned(),
            judge_prompt_version: super::judging::JUDGE_PROMPT_VERSION.to_owned(),
            layer_two_judge_prompt_version: super::judging::LAYER_TWO_JUDGE_PROMPT_VERSION
                .to_owned(),
            conditions: options.conditions.clone(),
            variants: options.variants.clone(),
            repeat: options.repeat,
            judge: options.judge,
            max_calls: options.max_calls,
            usage,
        }
    }
}

/// A count over a sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Count {
    pub count: usize,
    pub of: usize,
}

/// The layer-3 counts of one condition over one traceability class, task answers only
/// unless named otherwise.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionCounts {
    pub answers: usize,
    pub errors: usize,
    pub continued_reliance_rule: Count,
    pub continued_reliance_judge: Count,
    pub uses_replacement_rule: Count,
    pub uses_replacement_judge: Count,
    pub admits_uncertainty_rule: Count,
    pub admits_uncertainty_judge: Count,
    pub request_leakage: Count,
    pub answer_leakage: Count,
    /// Over control answers.
    pub control_utility: Count,
    pub control_collateral: Count,
    pub control_abstention: Count,
    /// Task answers where the rule and the judge disagree on continued reliance.
    pub rule_judge_disagreement: Count,
}

/// The D rebuild counts over the summary variants.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildCounts {
    pub compactions: usize,
    pub noop: usize,
    pub summary_holds_obsolete_term: Count,
}

/// The layer-2 section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerTwoSection {
    pub proposals: usize,
    pub metrics_text: String,
    pub judged: usize,
    pub judge_action_reasonable: Count,
    /// Proposals where the judge's `action_reasonable` equals the rule's `action_correct`.
    pub judge_rule_agreement: Count,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub metadata: RunMetadata,
    pub items: Vec<String>,
    /// Keyed `"<condition>/<traceable|untraceable>"`.
    pub layer_three: BTreeMap<String, ConditionCounts>,
    pub rebuild: RebuildCounts,
    pub layer_two: Option<LayerTwoSection>,
}

impl Count {
    fn over<T>(rows: &[&T], applies: impl Fn(&T) -> Option<bool>) -> Count {
        let mut count = Count::default();
        for row in rows {
            if let Some(value) = applies(row) {
                count.of += 1;
                count.count += usize::from(value);
            }
        }
        count
    }

    fn render(self) -> String {
        format!("{}/{}", self.count, self.of)
    }
}

/// Aggregates the rows into the report. Rows are grouped by the condition and the
/// traceability stored in each row. A row whose item is not in `items` is counted but matches
/// no per-item rule (uncertainty expected, forgotten). A row with an error is excluded from
/// the rule and judge counts and counted in `errors`; `request_leakage` counts it as well.
pub fn build_report(
    metadata: RunMetadata,
    items: &[Item],
    answers: &[AnswerRecord],
    proposals: &[ProposalRecord],
) -> Report {
    let mut groups: BTreeMap<String, Vec<&AnswerRecord>> = BTreeMap::new();
    for answer in answers {
        let class = if answer.traceable {
            "traceable"
        } else {
            "untraceable"
        };
        groups
            .entry(format!("{}/{class}", answer.condition.as_str()))
            .or_default()
            .push(answer);
    }
    let layer_three = groups
        .into_iter()
        .map(|(key, rows)| (key, condition_counts(&rows, items)))
        .collect();
    Report {
        metadata,
        items: items.iter().map(|item| item.name.clone()).collect(),
        layer_three,
        rebuild: rebuild_counts(answers),
        layer_two: layer_two_section(proposals),
    }
}

fn condition_counts(rows: &[&AnswerRecord], items: &[Item]) -> ConditionCounts {
    let find = |name: &str| items.iter().find(|item| item.name == name);
    let task: Vec<&AnswerRecord> = rows
        .iter()
        .copied()
        .filter(|row| row.role == Role::Task)
        .collect();
    let task_ok: Vec<&AnswerRecord> = task
        .iter()
        .copied()
        .filter(|row| row.error.is_empty())
        .collect();
    let control_ok: Vec<&AnswerRecord> = rows
        .iter()
        .copied()
        .filter(|row| row.role == Role::Control && row.error.is_empty())
        .collect();
    let expects_uncertainty =
        |row: &AnswerRecord| find(&row.item).is_some_and(|item| item.checks.uncertainty_expected);
    let is_forgotten = |row: &AnswerRecord| {
        find(&row.item).is_some_and(|item| item.retirement == Retirement::Forgotten)
    };
    ConditionCounts {
        answers: task.len(),
        errors: rows.iter().filter(|row| !row.error.is_empty()).count(),
        continued_reliance_rule: Count::over(&task_ok, |row| Some(row.rules.continued_reliance)),
        continued_reliance_judge: Count::over(&task_ok, |row| {
            row.judge.as_ref().map(|verdict| verdict.relies_on_obsolete)
        }),
        uses_replacement_rule: Count::over(&task_ok, |row| row.rules.uses_replacement),
        uses_replacement_judge: Count::over(&task_ok, |row| {
            row.judge
                .as_ref()
                .and_then(|verdict| verdict.uses_replacement)
        }),
        admits_uncertainty_rule: Count::over(&task_ok, |row| {
            expects_uncertainty(row).then_some(row.rules.admits_uncertainty)
        }),
        admits_uncertainty_judge: Count::over(&task_ok, |row| {
            let verdict = row.judge.as_ref()?;
            expects_uncertainty(row).then_some(verdict.admits_uncertainty)
        }),
        request_leakage: Count::over(rows, |row| Some(row.requests.marker_in_request)),
        answer_leakage: Count::over(&task_ok, |row| {
            is_forgotten(row).then_some(row.rules.answer_leakage)
        }),
        control_utility: Count::over(&control_ok, |row| row.rules.control_utility),
        control_collateral: Count::over(&control_ok, |row| row.rules.control_collateral),
        control_abstention: Count::over(&control_ok, |row| row.rules.control_abstention),
        rule_judge_disagreement: Count::over(&task_ok, |row| {
            let verdict = row.judge.as_ref()?;
            Some(row.rules.continued_reliance != verdict.relies_on_obsolete)
        }),
    }
}

fn rebuild_counts(answers: &[AnswerRecord]) -> RebuildCounts {
    let mut counts = RebuildCounts::default();
    for compaction in answers
        .iter()
        .filter(|row| row.role == Role::Task)
        .filter_map(|row| row.compaction.as_ref())
    {
        counts.compactions += 1;
        if compaction.noop {
            counts.noop += 1;
        } else {
            counts.summary_holds_obsolete_term.of += 1;
            counts.summary_holds_obsolete_term.count +=
                usize::from(compaction.summary_holds_obsolete_term);
        }
    }
    counts
}

fn layer_two_section(proposals: &[ProposalRecord]) -> Option<LayerTwoSection> {
    if proposals.is_empty() {
        return None;
    }
    let scores: Vec<_> = proposals.iter().map(|row| row.score.clone()).collect();
    let judged: Vec<&ProposalRecord> = proposals.iter().filter(|row| row.judge.is_some()).collect();
    Some(LayerTwoSection {
        proposals: proposals.len(),
        metrics_text: Metrics::from_scores(&scores).render(),
        judged: judged.len(),
        judge_action_reasonable: Count::over(&judged, |row| {
            row.judge.as_ref().map(|verdict| verdict.action_reasonable)
        }),
        judge_rule_agreement: Count::over(&judged, |row| {
            row.judge
                .as_ref()
                .map(|verdict| verdict.action_reasonable == row.score.action_correct)
        }),
    })
}

/// `report.md`: the metadata, one table per layer-3 section, the rebuild table and the
/// layer-2 counts, each count as `count/of`.
pub fn render_markdown(report: &Report) -> String {
    let mut text = String::from("# Memory retraction evaluation\n\n");
    render_metadata(&mut text, report);
    text.push_str("\n## Layer 3\n\n");
    if report.layer_three.is_empty() {
        text.push_str("No layer 3 rows.\n");
    }
    for (key, counts) in &report.layer_three {
        text.push_str(&format!("### {key}\n\n| Metric | Count |\n| --- | --- |\n"));
        for (name, value) in layer_three_rows(counts) {
            text.push_str(&format!("| {name} | {value} |\n"));
        }
        text.push('\n');
    }
    let rebuild = &report.rebuild;
    text.push_str("## Rebuild (condition D)\n\n| Metric | Count |\n| --- | --- |\n");
    text.push_str(&format!("| compactions | {} |\n", rebuild.compactions));
    text.push_str(&format!(
        "| compactions with no safe prefix | {} |\n",
        rebuild.noop
    ));
    text.push_str(&format!(
        "| rebuilt summaries holding an obsolete term | {} |\n\n",
        rebuild.summary_holds_obsolete_term.render()
    ));
    text.push_str("## Layer 2\n\n");
    match &report.layer_two {
        None => text.push_str("No layer 2 rows.\n\n"),
        Some(section) => {
            text.push_str(&format!("Proposals: {}\n\n```\n", section.proposals));
            text.push_str(section.metrics_text.trim_end());
            text.push_str("\n```\n\n");
            text.push_str(&format!(
                "Judged proposals: {}\n\n| Metric | Count |\n| --- | --- |\n",
                section.judged
            ));
            text.push_str(&format!(
                "| judge: action reasonable | {} |\n| judge and rule agree on the action | {} |\n\n",
                section.judge_action_reasonable.render(),
                section.judge_rule_agreement.render()
            ));
        }
    }
    text.push_str(
        "These counts are evidence for this dataset, this model and these settings. They \
describe the rows listed above. No threshold is defined, and the report does not state \
whether a result passes.\n",
    );
    text
}

fn render_metadata(text: &mut String, report: &Report) {
    let metadata = &report.metadata;
    let names = |values: Vec<&str>| values.join(", ");
    let thinking = if metadata.thinking.is_empty() {
        "unset"
    } else {
        &metadata.thinking
    };
    let usage = &metadata.usage;
    let lines = [
        format!("run id: {}", metadata.run_id),
        format!("started: {}", metadata.started_at.to_rfc3339()),
        format!("finished: {}", metadata.finished_at.to_rfc3339()),
        format!("otto version: {}", metadata.otto_version),
        format!("provider: {}", metadata.provider),
        format!("model: {}", metadata.model),
        format!("thinking: {thinking}"),
        format!("items version: {}", metadata.items_version),
        format!("labels version: {}", metadata.labels_version),
        format!(
            "proposal prompt version: {}",
            metadata.proposal_prompt_version
        ),
        format!("judge prompt version: {}", metadata.judge_prompt_version),
        format!(
            "layer 2 judge prompt version: {}",
            metadata.layer_two_judge_prompt_version
        ),
        format!(
            "items: {}",
            names(report.items.iter().map(String::as_str).collect())
        ),
        format!(
            "conditions: {}",
            names(
                metadata
                    .conditions
                    .iter()
                    .map(|value| value.as_str())
                    .collect()
            )
        ),
        format!(
            "variants: {}",
            names(
                metadata
                    .variants
                    .iter()
                    .map(|value| value.as_str())
                    .collect()
            )
        ),
        format!("repeat: {}", metadata.repeat),
        format!("judge: {}", if metadata.judge { "on" } else { "off" }),
        format!("max calls: {}", metadata.max_calls),
        format!(
            "usage: {} provider calls, {} input tokens, {} cached tokens, {} output tokens",
            usage.provider_calls, usage.input_tokens, usage.cached_tokens, usage.output_tokens
        ),
    ];
    for line in lines {
        text.push_str(&format!("- {line}\n"));
    }
}

fn layer_three_rows(counts: &ConditionCounts) -> Vec<(&'static str, String)> {
    vec![
        ("task answers", counts.answers.to_string()),
        ("errors (task and control)", counts.errors.to_string()),
        (
            "continued reliance, rule",
            counts.continued_reliance_rule.render(),
        ),
        (
            "continued reliance, judge",
            counts.continued_reliance_judge.render(),
        ),
        (
            "uses replacement, rule",
            counts.uses_replacement_rule.render(),
        ),
        (
            "uses replacement, judge",
            counts.uses_replacement_judge.render(),
        ),
        (
            "admits uncertainty, rule",
            counts.admits_uncertainty_rule.render(),
        ),
        (
            "admits uncertainty, judge",
            counts.admits_uncertainty_judge.render(),
        ),
        ("marker in a request", counts.request_leakage.render()),
        (
            "marker or obsolete term in a forgotten item answer",
            counts.answer_leakage.render(),
        ),
        ("control answer useful", counts.control_utility.render()),
        (
            "control answer shows collateral change",
            counts.control_collateral.render(),
        ),
        (
            "control answer abstains",
            counts.control_abstention.render(),
        ),
        (
            "rule and judge disagree on reliance",
            counts.rule_judge_disagreement.render(),
        ),
    ]
}
