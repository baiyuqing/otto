//! The deterministic scorer: one raw proposal against one label.

use serde::{Serialize, Serializer};

use super::super::{Outcome, Proposal, ProposalAction, validate_proposal};
use super::labels::{LabeledItem, contains_term, requires_evidence, retires};

/// What the generator returned for one item.
#[derive(Debug, Clone)]
pub enum Generated {
    Proposal(Proposal),
    /// No proposal: the model was unreachable, answered out of form or the
    /// input was rejected. Holds the reason.
    Unavailable(String),
}

impl Generated {
    /// Maps the result of `Generator::generate`.
    pub fn from_result(result: Result<Proposal, Outcome>) -> Self {
        match result {
            Ok(proposal) => Generated::Proposal(proposal),
            Err(Outcome::Rejected(reason) | Outcome::Unavailable(reason)) => {
                Generated::Unavailable(reason)
            }
            Err(other) => Generated::Unavailable(format!("unexpected outcome {other:?}")),
        }
    }
}

/// Whether an abstaining proposal matched the label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Abstention {
    /// The proposal did not abstain, or there was no proposal.
    NotApplicable,
    /// The proposal abstained and an expected action abstains.
    Appropriate,
    /// The proposal abstained and every expected action retires.
    Excessive,
}

/// The layer-1 verdict next to the raw score.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerOne {
    Accepted,
    Rejected(String),
    NotRun,
}

fn serialize_optional_action<S: Serializer>(
    action: &Option<ProposalAction>,
    out: S,
) -> Result<S::Ok, S::Error> {
    match action {
        Some(action) => out.serialize_some(action.as_str()),
        None => out.serialize_none(),
    }
}

/// The score of one item.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Score {
    pub name: String,
    pub category: String,
    #[serde(serialize_with = "serialize_optional_action")]
    pub action: Option<ProposalAction>,
    pub unavailable: Option<String>,
    pub layer_one: LayerOne,
    pub action_correct: bool,
    pub abstention: Abstention,
    pub erroneous_retirement: bool,
    pub unsupported_inversion: bool,
    pub scope_violation: bool,
    /// `None` unless the proposal is a `supersede` or `correct`.
    pub claims_preserved: Option<bool>,
    /// The proposal cited evidence the label does not list as supporting.
    pub cited_unsupporting: bool,
    /// An action that needs evidence omitted a supporting item.
    pub missed_supporting: bool,
    /// A `forget` or `retract` repeated the record marker.
    pub leakage: bool,
}

/// Scores `generated` against the label of `item`, before layer 1; the
/// layer-1 verdict is recorded next to the result.
pub fn score(item: &LabeledItem, generated: &Generated) -> Score {
    let label = &item.label;
    let mut result = Score {
        name: item.name.clone(),
        category: item.category.clone(),
        action: None,
        unavailable: None,
        layer_one: LayerOne::NotRun,
        action_correct: false,
        abstention: Abstention::NotApplicable,
        erroneous_retirement: false,
        unsupported_inversion: false,
        scope_violation: false,
        claims_preserved: None,
        cited_unsupporting: false,
        missed_supporting: false,
        leakage: false,
    };
    let proposal = match generated {
        Generated::Proposal(proposal) => proposal,
        Generated::Unavailable(reason) => {
            result.unavailable = Some(reason.clone());
            return result;
        }
    };
    let action = proposal.action;
    result.action = Some(action);
    result.layer_one = match validate_proposal(&item.input("memory-eval"), proposal) {
        Ok(()) => LayerOne::Accepted,
        Err(reason) => LayerOne::Rejected(reason),
    };
    result.action_correct = label.expected_actions.contains(&action);
    if !retires(action) {
        let expected_abstains = label
            .expected_actions
            .iter()
            .any(|expected| !retires(*expected));
        result.abstention = if expected_abstains {
            Abstention::Appropriate
        } else {
            Abstention::Excessive
        };
    } else {
        result.erroneous_retirement = !label
            .expected_actions
            .iter()
            .any(|expected| retires(*expected));
    }
    let replacement = proposal.replacement.as_deref().unwrap_or("");
    let replaces = matches!(action, ProposalAction::Supersede | ProposalAction::Correct);
    if replaces {
        result.unsupported_inversion = label
            .inversion_terms
            .iter()
            .any(|term| contains_term(replacement, term));
        result.claims_preserved = Some(
            label
                .replacement_must_contain
                .iter()
                .all(|term| contains_term(replacement, term)),
        );
    }
    result.scope_violation = label
        .replacement_must_not_contain
        .iter()
        .any(|term| contains_term(replacement, term));
    result.cited_unsupporting = proposal
        .evidence_refs
        .iter()
        .any(|id| !label.supporting_evidence.contains(id));
    result.missed_supporting = requires_evidence(action)
        && label
            .supporting_evidence
            .iter()
            .any(|id| !proposal.evidence_refs.contains(id));
    result.leakage = matches!(action, ProposalAction::Forget | ProposalAction::Retract)
        && (contains_term(&proposal.explanation, &item.record.marker)
            || contains_term(replacement, &item.record.marker));
    result
}
