//! Tests of the labeled dataset, the parser and the scorer.

use super::*;
use crate::memory::proposals::{Proposal, ProposalAction};
use otto_core::memory::Trust;

mod dataset;
mod end_to_end;
mod parser;
mod rubric;

/// A hand-built item: a `retract`-labeled preference with one supporting
/// local evidence item. Tests change the fields they exercise.
pub(super) fn base_item() -> LabeledItem {
    LabeledItem {
        name: "base_item".into(),
        category: "distinction".into(),
        record: LabeledRecord {
            text: "Prefers the vim editor for every project (m-90-base)".into(),
            kind: "preference".into(),
            key: "editor".into(),
            marker: "m-90-base".into(),
        },
        evidence: vec![
            LabeledEvidence {
                id: "e1".into(),
                text: "I stopped using vim.".into(),
                trust: Trust::Local,
                source: "user".into(),
            },
            LabeledEvidence {
                id: "e2".into(),
                text: "The weather is mild.".into(),
                trust: Trust::Local,
                source: "user".into(),
            },
        ],
        permitted: ProposalAction::ALL.to_vec(),
        label: Label {
            expected_actions: vec![ProposalAction::Retract],
            supporting_evidence: vec!["e1".into()],
            replacement_must_contain: vec![],
            replacement_must_not_contain: vec![],
            inversion_terms: vec![],
            reference_replacement: None,
            rationale: "Evidence e1 says the use stopped.".into(),
        },
    }
}

pub(super) fn proposal(
    action: ProposalAction,
    refs: &[&str],
    explanation: &str,
    replacement: Option<&str>,
) -> Generated {
    Generated::Proposal(Proposal {
        action,
        evidence_refs: refs.iter().map(|id| (*id).to_owned()).collect(),
        explanation: explanation.into(),
        replacement: replacement.map(str::to_owned),
    })
}
