//! Deterministic checks of an answer against the item: term presence only, recorded next to
//! the judge verdict and never merged with it.

use serde::{Deserialize, Serialize};

use super::items::Item;

/// Terms that admit the current value is unknown. Lowercase; matched as substrings.
pub const UNCERTAINTY_TERMS: &[&str] = &[
    "not sure",
    "don't know",
    "do not know",
    "unknown",
    "unclear",
    "cannot confirm",
    "can't confirm",
    "no longer have",
    "no current",
    "not certain",
    "uncertain",
    "please check",
    "please confirm",
];

/// The rule results of one answer. A task answer fills the first four; a control answer
/// fills the last three; the rest stay at their defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleChecks {
    /// An `obsolete_terms` entry is in the task answer.
    pub continued_reliance: bool,
    /// A `checks.must_contain_any` entry is in the task answer; `None` without a replacement.
    pub uses_replacement: Option<bool>,
    /// An uncertainty term is in the task answer.
    pub admits_uncertainty: bool,
    /// The marker or an obsolete term is in the task answer of a `forgotten` item.
    pub answer_leakage: bool,
    /// A `control.must_contain_any` entry is in the control answer.
    pub control_utility: Option<bool>,
    /// A `control.must_not_contain` entry is in the control answer.
    pub control_collateral: Option<bool>,
    /// The control answer has an uncertainty term and no `control.must_contain_any` entry.
    pub control_abstention: Option<bool>,
}

/// Case-insensitive substring match of `term` in `text`.
pub fn contains_term(text: &str, term: &str) -> bool {
    text.to_lowercase().contains(&term.to_lowercase())
}

/// Whether `text` holds an uncertainty term.
pub fn admits_uncertainty(text: &str) -> bool {
    UNCERTAINTY_TERMS
        .iter()
        .any(|term| contains_term(text, term))
}

/// The task-answer rules.
pub fn check_task_answer(_item: &Item, _answer: &str) -> RuleChecks {
    RuleChecks::default()
}

/// The control-answer rules.
pub fn check_control_answer(_item: &Item, _answer: &str) -> RuleChecks {
    RuleChecks::default()
}
