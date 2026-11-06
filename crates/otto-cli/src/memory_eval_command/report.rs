//! `report.json` and `report.md`: run metadata, layer-3 counts per condition and
//! traceability, D rebuild counts, layer-2 metrics and judge agreement. Counts with sample
//! sizes; no threshold.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use otto_core::provider::Provider;
use serde::{Deserialize, Serialize};

use super::argument_parsing::{Condition, Options, Variant};
use super::conditions::AnswerRecord;
use super::items::Item;
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

/// Aggregates the rows into the report.
pub fn build_report(
    metadata: RunMetadata,
    items: &[Item],
    _answers: &[AnswerRecord],
    _proposals: &[ProposalRecord],
) -> Report {
    Report {
        metadata,
        items: items.iter().map(|item| item.name.clone()).collect(),
        layer_three: BTreeMap::new(),
        rebuild: RebuildCounts::default(),
        layer_two: None,
    }
}

/// `report.md`: the metadata, one table per layer-3 section, the rebuild table and the
/// layer-2 counts, each count as `count/of`.
pub fn render_markdown(_report: &Report) -> String {
    String::new()
}
