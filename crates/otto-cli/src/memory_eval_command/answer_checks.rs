//! Deterministic checks of an answer against the item: term presence only, recorded next to
//! the judge verdict and never merged with it.

use serde::{Deserialize, Serialize};

use super::items::{Item, Retirement};

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

/// Case-insensitive match of `term` in `text`. When `term` starts with a letter or digit, the
/// match must not continue a longer word: `npm install` does not match inside `pnpm install`,
/// and `30s` does not match inside `130s`.
pub fn contains_term(text: &str, term: &str) -> bool {
    let text = text.to_lowercase();
    let term = term.to_lowercase();
    if term.is_empty() {
        return false;
    }
    let starts_with_word_character = term.chars().next().is_some_and(char::is_alphanumeric);
    text.match_indices(&term).any(|(start, _)| {
        !starts_with_word_character
            || text[..start]
                .chars()
                .next_back()
                .is_none_or(|before| !before.is_alphanumeric())
    })
}

/// Whether `text` holds an uncertainty term.
pub fn admits_uncertainty(text: &str) -> bool {
    UNCERTAINTY_TERMS
        .iter()
        .any(|term| contains_term(text, term))
}

/// Whether `answer` holds any of `terms`.
fn holds_any(answer: &str, terms: &[String]) -> bool {
    terms.iter().any(|term| contains_term(answer, term))
}

/// The task-answer rules.
pub fn check_task_answer(item: &Item, answer: &str) -> RuleChecks {
    let continued_reliance = holds_any(answer, &item.obsolete_terms);
    RuleChecks {
        continued_reliance,
        uses_replacement: item
            .replacement
            .as_ref()
            .map(|_| holds_any(answer, &item.checks.must_contain_any)),
        admits_uncertainty: admits_uncertainty(answer),
        answer_leakage: item.retirement == Retirement::Forgotten
            && (answer.contains(&item.record.marker) || continued_reliance),
        ..RuleChecks::default()
    }
}

/// The control-answer rules.
pub fn check_control_answer(item: &Item, answer: &str) -> RuleChecks {
    let has_control_term = holds_any(answer, &item.control.must_contain_any);
    RuleChecks {
        control_utility: Some(has_control_term),
        control_collateral: Some(holds_any(answer, &item.control.must_not_contain)),
        control_abstention: Some(admits_uncertainty(answer) && !has_control_term),
        ..RuleChecks::default()
    }
}
