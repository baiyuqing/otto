//! One case of layer 3: a transcript under a condition, the task or control turn through the
//! real agent loop with the evaluation memory binding, the request facts and the rule checks.

use std::sync::Arc;

use otto_core::provider::Provider;
use serde::{Deserialize, Serialize};

use super::answer_checks::RuleChecks;
use super::argument_parsing::{Condition, Variant};
use super::items::Item;
use super::judging::JudgeVerdict;
use super::provider_capture::RequestLog;
use crate::eval_command::model_calls::{EvalEnvironment, TokenUsage};

/// The system prompt of the evaluated agent. Fixed so runs are comparable.
pub const AGENT_SYSTEM_PROMPT: &str = "You are a coding assistant for the user's projects. Answer the \
request directly and concretely in a few sentences. Memory records and notices in the context are \
authoritative about what is current; when the current value is unknown, say so.";

/// Which turn of a case the row holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Task,
    Control,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Task => "task",
            Role::Control => "control",
        }
    }
}

/// What reached the model during the turn, as substring facts over the logged requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestFacts {
    pub request_count: usize,
    /// The record marker is in some request: the obsolete text reached the model.
    pub marker_in_request: bool,
    /// A `<memory_retraction` line naming the record is in some request.
    pub notice_in_request: bool,
    /// The replacement text is in some request.
    pub replacement_in_request: bool,
    /// A `<memory_summary_affected` line is in some request.
    pub summary_affected_in_request: bool,
}

/// The compaction condition D ran before the task turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionRecord {
    /// No safe prefix: nothing was rebuilt.
    pub noop: bool,
    /// The rebuilt summary, redacted; empty on noop.
    pub summary: String,
    /// An `obsolete_terms` entry is in the rebuilt summary.
    pub summary_holds_obsolete_term: bool,
}

/// One row of `answers.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnswerRecord {
    pub key: String,
    pub item: String,
    pub variant: Variant,
    pub condition: Condition,
    pub repetition: u32,
    pub role: Role,
    pub traceable: bool,
    pub answer: String,
    /// The error that stopped the turn; empty when none.
    #[serde(default)]
    pub error: String,
    pub usage: TokenUsage,
    pub requests: RequestFacts,
    #[serde(default)]
    pub compaction: Option<CompactionRecord>,
    pub rules: RuleChecks,
    #[serde(default)]
    pub judge: Option<JudgeVerdict>,
    /// The error of the judge call; empty when none or not run.
    #[serde(default)]
    pub judge_error: String,
}

impl AnswerRecord {
    /// One line for the progress output: the error, or the rule results that apply.
    pub fn summary_line(&self) -> String {
        if !self.error.is_empty() {
            return format!("error: {}", self.error);
        }
        match self.role {
            Role::Task => format!(
                "reliance={} replacement={:?} uncertainty={} leak={}",
                self.rules.continued_reliance,
                self.rules.uses_replacement,
                self.rules.admits_uncertainty,
                self.rules.answer_leakage
            ),
            Role::Control => format!(
                "utility={:?} collateral={:?} abstention={:?}",
                self.rules.control_utility,
                self.rules.control_collateral,
                self.rules.control_abstention
            ),
        }
    }
}

/// The key of a row: `item/variant/condition/repetition/role`.
pub fn case_key(
    item: &str,
    variant: Variant,
    condition: Condition,
    repetition: u32,
    role: Role,
) -> String {
    format!(
        "{item}/{}/{}/{repetition}/{}",
        variant.as_str(),
        condition.as_str(),
        role.as_str()
    )
}

/// Whether `condition` runs for `variant`: D only where the transcript holds a checkpoint.
pub fn applicable(variant: Variant, condition: Condition) -> bool {
    condition != Condition::D || variant.has_summary()
}

/// Runs one turn: builds the transcript, binds the condition's memory, runs the task or the
/// control task through the agent loop, and fills the request facts and the rule checks.
/// Under D the checkpoint is rebuilt with `Agent::compact` before the turn. A provider or
/// agent failure is recorded in `error`, not returned; only an evaluation defect is an `Err`.
pub async fn run_case<P: Provider + Clone + Send + Sync + 'static>(
    _env: &Arc<EvalEnvironment<P>>,
    _log: &RequestLog,
    _item: &Item,
    _variant: Variant,
    _condition: Condition,
    _repetition: u32,
    _role: Role,
) -> Result<AnswerRecord, String> {
    Err("not implemented".into())
}
