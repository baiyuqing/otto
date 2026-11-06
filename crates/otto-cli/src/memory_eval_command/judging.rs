//! The LLM judges: a fixed rubric per layer, JSON verdicts, stored next to the rule checks.

use std::sync::Arc;

use otto::memory::proposals::Proposal;
use otto::memory::proposals::evaluation::LabeledItem;
use otto_core::provider::Provider;
use serde::{Deserialize, Serialize};

use super::items::Item;
use crate::eval_command::model_calls::EvalEnvironment;

/// Version of the layer-3 judge prompt, recorded in the report.
pub const JUDGE_PROMPT_VERSION: &str = "memory-retraction-judge/1";
/// Version of the layer-2 judge prompt, recorded in the report.
pub const LAYER_TWO_JUDGE_PROMPT_VERSION: &str = "memory-proposal-judge/1";

/// The judge's reading of one task answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeVerdict {
    /// The answer presents the obsolete claim as current.
    pub relies_on_obsolete: bool,
    /// The answer gives the replacement value; `None` when the item has none.
    pub uses_replacement: Option<bool>,
    /// The answer says the current value is unknown or should be confirmed.
    pub admits_uncertainty: bool,
    pub reason: String,
}

/// The judge's reading of one layer-2 proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalVerdict {
    /// The action follows from the evidence under the rubric.
    pub action_reasonable: bool,
    /// The cited evidence is the evidence that supports the action.
    pub evidence_cited_correctly: bool,
    /// The replacement keeps the claims the evidence does not change; `None` without one.
    pub replacement_preserves_valid_claims: Option<bool>,
    pub reason: String,
}

/// Judges `answer` to `item.task`. The obsolete claim is passed with the marker removed.
pub async fn judge_task_answer<P: Provider + Clone + Send + Sync>(
    _env: &Arc<EvalEnvironment<P>>,
    _item: &Item,
    _answer: &str,
) -> Result<JudgeVerdict, String> {
    Err("not implemented".into())
}

/// Judges `proposal` for `item` without seeing the label.
pub async fn judge_proposal<P: Provider + Clone + Send + Sync>(
    _env: &Arc<EvalEnvironment<P>>,
    _item: &LabeledItem,
    _proposal: &Proposal,
) -> Result<ProposalVerdict, String> {
    Err("not implemented".into())
}
