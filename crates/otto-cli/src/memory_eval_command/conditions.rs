//! One case of layer 3: a transcript under a condition, the task or control turn through the
//! real agent loop with the evaluation memory binding, the request facts and the rule checks.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use otto_core::agent::{Agent, CompactionSettings, Event, Options};
use otto_core::model::{Message, Role as ModelRole};
use otto_core::provider::{Provider, ProviderError, Request, RequestSizer};
use otto_core::session::{MemorySession, Session};
use serde::{Deserialize, Serialize};

use super::answer_checks::{RuleChecks, check_control_answer, check_task_answer, contains_term};
use super::argument_parsing::{Condition, Variant};
use super::items::Item;
use super::judging::JudgeVerdict;
use super::memory_binding::EvalMemory;
use super::provider_capture::{CapturedRequest, RequestLog, request_text};
use super::transcripts::build_transcript;
use crate::eval_command::model_calls::{EvalEnvironment, TokenUsage};
use crate::eval_command::tool_executors::NoTools;

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
/// The request facts cover the requests of the turn itself; `request_count` and `usage` also
/// cover the compaction requests of D.
pub async fn run_case<P: Provider + Clone + Send + Sync + 'static>(
    env: &Arc<EvalEnvironment<P>>,
    log: &RequestLog,
    item: &Item,
    variant: Variant,
    condition: Condition,
    repetition: u32,
    role: Role,
) -> Result<AnswerRecord, String> {
    let transcript = build_transcript(item, variant).await?;
    let first_request = log.len();
    let agent = Agent::with_redactor(
        env.provider.clone(),
        NoTools,
        transcript.session,
        agent_options(env, item, condition),
        env.redactor.clone(),
    );
    let mut errors: Vec<String> = Vec::new();
    let mut compaction = None;
    if condition == Condition::D {
        let mut sink = |_event: Event| {};
        match agent.compact("", &mut sink, &env.cancel).await {
            Ok(result) => compaction = Some(compaction_record(env, item, &agent, result.noop)),
            Err(error) => errors.push(format!(
                "compaction: {}",
                env.redactor.redact_string(&error.to_string())
            )),
        }
    }
    let first_turn_request = log.len();
    let turn_text = match role {
        Role::Task => &item.task,
        Role::Control => &item.control.task,
    };
    let messages_before_turn = agent.session().messages().len();
    let mut sink = |_event: Event| {};
    if let Err(error) = agent.run(turn_text, &mut sink, &env.cancel).await {
        errors.push(env.redactor.redact_string(&error.to_string()));
    }
    let answer = newest_assistant_text(&agent.session().messages()[messages_before_turn..]);
    let all_requests = log.since(first_request);
    let mut usage = TokenUsage::default();
    for request in &all_requests {
        usage.add(&request.usage);
    }
    let requests = request_facts(item, &log.since(first_turn_request), all_requests.len());
    let rules = match role {
        Role::Task => check_task_answer(item, &answer),
        Role::Control => check_control_answer(item, &answer),
    };
    Ok(AnswerRecord {
        key: case_key(&item.name, variant, condition, repetition, role),
        item: item.name.clone(),
        variant,
        condition,
        repetition,
        role,
        traceable: variant.traceable(),
        answer,
        error: errors.join("; "),
        usage,
        requests,
        compaction,
        rules,
        judge: None,
        judge_error: String::new(),
    })
}

/// The agent options of one case: the evaluation memory binding of `condition`, manual
/// compaction only, and the fixed system prompt.
fn agent_options<P>(env: &EvalEnvironment<P>, item: &Item, condition: Condition) -> Options {
    let counter = AtomicU64::new(0);
    Options {
        model: env.profile_model.clone(),
        provider_name: env.provider_name.clone(),
        thinking: env.thinking.clone(),
        system_prompt: AGENT_SYSTEM_PROMPT.to_owned(),
        cache_key: String::new(),
        now: Box::new(Utc::now),
        new_id: Box::new(move || {
            format!(
                "{:032x}",
                counter.fetch_add(1, Ordering::SeqCst) + 1_000_000
            )
        }),
        request_sizer: Some(Arc::new(RequestTextSizer)),
        memory: Some(Arc::new(EvalMemory::for_condition(item, condition))),
        compaction: CompactionSettings {
            auto: false,
            hard_input_window: env.compaction.hard_input_window,
            working_window: env.compaction.working_window,
            reserve_tokens: env.compaction.reserve_tokens,
            keep_recent_tokens: KEEP_RECENT_TOKENS,
        },
        ..Options::default()
    }
}

/// Compaction keeps no recent tokens verbatim, so the whole post-checkpoint transcript is
/// summarized.
const KEEP_RECENT_TOKENS: i64 = 0;

/// Sizes a request as the byte length of its text; the evaluation's provider is not
/// necessarily a wire client, and the size only bounds the summary request.
struct RequestTextSizer;

impl RequestSizer for RequestTextSizer {
    fn serialized_request_size(&self, request: &Request) -> Result<usize, ProviderError> {
        Ok(request_text(request).len())
    }
}

/// The text of the newest assistant message of `messages` that has text, or empty.
fn newest_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .filter(|message| message.role == ModelRole::Assistant)
        .map(Message::text)
        .find(|text| !text.trim().is_empty())
        .unwrap_or_default()
}

fn compaction_record<P: Provider + Clone + Send + Sync + 'static>(
    env: &EvalEnvironment<P>,
    item: &Item,
    agent: &Agent<P, NoTools, MemorySession>,
    noop: bool,
) -> CompactionRecord {
    let summary = if noop {
        String::new()
    } else {
        agent
            .session()
            .latest_compaction()
            .map(|latest| env.redactor.redact_string(&latest.summary))
            .unwrap_or_default()
    };
    CompactionRecord {
        noop,
        summary_holds_obsolete_term: item
            .obsolete_terms
            .iter()
            .any(|term| contains_term(&summary, term)),
        summary,
    }
}

/// The substring facts over `turn_requests`; `request_count` is the count of the whole case.
fn request_facts(
    item: &Item,
    turn_requests: &[CapturedRequest],
    request_count: usize,
) -> RequestFacts {
    let any =
        |holds: &dyn Fn(&str) -> bool| turn_requests.iter().any(|request| holds(&request.text));
    let notice_prefix = format!("<memory_retraction id=\"{}\"", item.record.id);
    RequestFacts {
        request_count,
        marker_in_request: any(&|text| text.contains(&item.record.marker)),
        notice_in_request: any(&|text| text.contains(&notice_prefix)),
        replacement_in_request: item
            .replacement
            .as_ref()
            .is_some_and(|replacement| any(&|text| text.contains(&replacement.text))),
        summary_affected_in_request: any(&|text| text.contains("<memory_summary_affected")),
    }
}
