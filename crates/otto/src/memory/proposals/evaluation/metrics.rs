//! Aggregate counts over a set of scores, reported as count/total.

use std::collections::BTreeMap;
use std::fmt::Write;

use serde::Serialize;

use super::scoring::{Abstention, LayerOne, Score};

/// Per-category counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CategoryMetrics {
    pub items: usize,
    pub action_correct: usize,
}

/// Separate counts, never combined into one number. Every count is over
/// `items`; an unavailable item counts only in `unavailable`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Metrics {
    pub items: usize,
    pub unavailable: usize,
    pub rejected_by_layer_one: usize,
    pub action_correct: usize,
    pub unsupported_inversion: usize,
    pub erroneous_retirement: usize,
    pub appropriate_abstention: usize,
    pub excessive_abstention: usize,
    pub scope_violations: usize,
    pub claims_not_preserved: usize,
    pub cited_unsupporting: usize,
    pub missed_supporting: usize,
    pub leakage: usize,
    pub per_category: BTreeMap<String, CategoryMetrics>,
}

impl Metrics {
    pub fn from_scores(scores: &[Score]) -> Metrics {
        let mut metrics = Metrics {
            items: scores.len(),
            ..Metrics::default()
        };
        for score in scores {
            let category = metrics
                .per_category
                .entry(score.category.clone())
                .or_default();
            category.items += 1;
            category.action_correct += usize::from(score.action_correct);
            metrics.unavailable += usize::from(score.unavailable.is_some());
            metrics.rejected_by_layer_one +=
                usize::from(matches!(score.layer_one, LayerOne::Rejected(_)));
            metrics.action_correct += usize::from(score.action_correct);
            metrics.unsupported_inversion += usize::from(score.unsupported_inversion);
            metrics.erroneous_retirement += usize::from(score.erroneous_retirement);
            metrics.appropriate_abstention +=
                usize::from(score.abstention == Abstention::Appropriate);
            metrics.excessive_abstention += usize::from(score.abstention == Abstention::Excessive);
            metrics.scope_violations += usize::from(score.scope_violation);
            metrics.claims_not_preserved += usize::from(score.claims_preserved == Some(false));
            metrics.cited_unsupporting += usize::from(score.cited_unsupporting);
            metrics.missed_supporting += usize::from(score.missed_supporting);
            metrics.leakage += usize::from(score.leakage);
        }
        metrics
    }

    /// A plain text table: one `<name>: <count>/<items>` line per metric and a
    /// per-category block.
    pub fn render(&self) -> String {
        let lines = [
            ("unavailable", self.unavailable),
            ("rejected_by_layer_one", self.rejected_by_layer_one),
            ("action_correct", self.action_correct),
            ("unsupported_inversion", self.unsupported_inversion),
            ("erroneous_retirement", self.erroneous_retirement),
            ("appropriate_abstention", self.appropriate_abstention),
            ("excessive_abstention", self.excessive_abstention),
            ("scope_violations", self.scope_violations),
            ("claims_not_preserved", self.claims_not_preserved),
            ("cited_unsupporting", self.cited_unsupporting),
            ("missed_supporting", self.missed_supporting),
            ("leakage", self.leakage),
        ];
        let mut text = String::new();
        for (name, count) in lines {
            let _ = writeln!(text, "{name}: {count}/{}", self.items);
        }
        text.push_str("per category (action_correct):\n");
        for (category, counts) in &self.per_category {
            let _ = writeln!(
                text,
                "  {category}: {}/{}",
                counts.action_correct, counts.items
            );
        }
        text
    }
}
