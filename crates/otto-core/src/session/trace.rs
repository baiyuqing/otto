//! A timeline of turns, provider requests, tool calls, compactions and
//! sub-agents, built from session entries. It performs no I/O.
//!
//! The same builder serves a running turn and a turn read back after a
//! restart: a span without an end is `running` while the session runs a turn
//! and `interrupted` otherwise.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::context::parse_rfc3339;
use super::operation::{DecodedOperationFact, OPERATION_CUSTOM_TYPE, OperationFact};
use super::provider_call::{
    PROVIDER_CALL_CUSTOM_TYPE, ProviderCallKind, ProviderCallRecord, ProviderRetryRecord,
    RequestEstimate, decode_provider_call,
};
use super::turn::{TURN_CUSTOM_TYPE, TurnOutcome, TurnRecord, TurnTrigger, decode_turn_record};
use super::{PiEntry, decode_operation_fact};
use crate::model::{BlockType, FinishReason, Message, OperationDisposition, Role, Usage};

/// The custom entry type that starts every child session file.
const TASK_SPEC_CUSTOM_TYPE: &str = "otto.task_spec";
/// Longest argument preview in a tool label, in characters.
const TOOL_PREVIEW_CHARS: usize = 60;
/// Longest sub-agent description in a label, in characters.
const AGENT_LABEL_CHARS: usize = 40;

/// One session file's contents.
pub struct TraceSource<'a> {
    /// Raw entries in file order.
    pub entries: &'a [PiEntry],
    /// The resolved transcript.
    pub messages: &'a [Message],
    /// True when a turn is running in this session now.
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trace {
    pub session_id: String,
    pub turns: Vec<Span>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Turn,
    Provider,
    Tool,
    Compaction,
    Subagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanStatus {
    Running,
    Ok,
    Error,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Span {
    /// A turn id, an operation id or a task id. Spans inside a child session
    /// are prefixed with `<task id>:` so that ids stay unique in one trace.
    pub id: String,
    pub kind: SpanKind,
    pub label: String,
    /// Unix milliseconds.
    pub started_at: i64,
    /// Absent while running and after an interruption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    pub status: SpanStatus,
    /// The timing is derived from neighbouring entries, not recorded.
    #[serde(default, skip_serializing_if = "is_false")]
    pub estimated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderTiming>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_call_id: String,
    /// The sub-agent task a span belongs to; empty for parent spans.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task_id: String,
    /// Turn spans: task ids of the notifications delivered in the turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggered_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Span>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTiming {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_output_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retries: Vec<ProviderRetryRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestEstimate>,
}

/// The text of one span, in titled sections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpanContent {
    pub span_id: String,
    pub sections: Vec<ContentSection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentSection {
    pub title: String,
    pub text: String,
}

// ---------------------------------------------------------------------------
// scanning entries
// ---------------------------------------------------------------------------

struct TurnFact {
    id: String,
    trigger: TurnTrigger,
    start: i64,
    end: Option<i64>,
    outcome: Option<TurnOutcome>,
}

struct ToolFact {
    operation_id: String,
    tool_call_id: String,
    tool_name: String,
    start: i64,
    end: Option<i64>,
    disposition: Option<OperationDisposition>,
}

#[derive(Default)]
struct Scan {
    turns: Vec<TurnFact>,
    tools: Vec<ToolFact>,
    calls: Vec<ProviderCallRecord>,
    /// Timestamp and summary of each compaction entry.
    compactions: Vec<(i64, String)>,
    /// Timestamp and JSON data of the `otto.task_spec` entry.
    spec: Option<(i64, serde_json::Value)>,
    /// Every entry timestamp, in file order.
    times: Vec<i64>,
}

fn entry_millis(entry: &PiEntry) -> Option<i64> {
    parse_rfc3339(&entry.timestamp).map(|time| time.timestamp_millis())
}

impl Scan {
    fn new(entries: &[PiEntry]) -> Self {
        let mut scan = Self::default();
        for entry in entries {
            let Some(at) = entry_millis(entry) else {
                continue;
            };
            scan.times.push(at);
            if let Some(compaction) = &entry.compaction {
                scan.compactions.push((at, compaction.summary.clone()));
            }
            let Some(custom) = &entry.custom else {
                continue;
            };
            let Some(data) = custom.data.as_deref().map(RawValue::get) else {
                continue;
            };
            match custom.custom_type.as_str() {
                TURN_CUSTOM_TYPE => scan.apply_turn(at, data),
                OPERATION_CUSTOM_TYPE => scan.apply_operation(at, data),
                PROVIDER_CALL_CUSTOM_TYPE => scan.calls.extend(decode_provider_call(data)),
                TASK_SPEC_CUSTOM_TYPE if scan.spec.is_none() => {
                    scan.spec = serde_json::from_str(data).ok().map(|value| (at, value));
                }
                _ => {}
            }
        }
        scan
    }

    fn apply_turn(&mut self, at: i64, data: &str) {
        match decode_turn_record(data) {
            Some(TurnRecord::Started {
                turn_id, trigger, ..
            }) => self.turns.push(TurnFact {
                id: turn_id,
                trigger,
                start: at,
                end: None,
                outcome: None,
            }),
            Some(TurnRecord::Ended {
                turn_id, outcome, ..
            }) => {
                if let Some(turn) = self
                    .turns
                    .iter_mut()
                    .rev()
                    .find(|turn| turn.id == turn_id && turn.end.is_none())
                {
                    turn.end = Some(at);
                    turn.outcome = Some(outcome);
                }
            }
            None => {}
        }
    }

    fn apply_operation(&mut self, at: i64, data: &str) {
        let Ok(DecodedOperationFact::Fact(fact)) = decode_operation_fact(data) else {
            return;
        };
        let id = fact.operation_id().as_str();
        match &fact {
            OperationFact::Attempt { .. } => {
                // The last attempt is the one the terminal fact settles.
                match self.tools.iter_mut().find(|tool| tool.operation_id == id) {
                    Some(tool) => tool.start = at,
                    None => self.tools.push(ToolFact {
                        operation_id: id.to_owned(),
                        tool_call_id: fact.tool_call_id().to_owned(),
                        tool_name: fact.tool_name().to_owned(),
                        start: at,
                        end: None,
                        disposition: None,
                    }),
                }
            }
            OperationFact::Terminal { outcome, .. } => {
                if let Some(tool) = self.tools.iter_mut().find(|tool| tool.operation_id == id) {
                    tool.end = Some(at);
                    tool.disposition = Some(outcome.disposition);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// sub-agents
// ---------------------------------------------------------------------------

fn set_task_id(span: &mut Span, task_id: &str) {
    task_id.clone_into(&mut span.task_id);
    for child in &mut span.children {
        set_task_id(child, task_id);
    }
}

fn spec_str<'a>(spec: &'a serde_json::Value, key: &str) -> &'a str {
    spec.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

/// The sub-agent span of one child session and the parent tool call that
/// started it. `None` when the child has no usable task spec.
fn subagent_span(child: &TraceSource<'_>) -> Option<(Span, String)> {
    let scan = Scan::new(child.entries);
    let (started_at, spec) = scan.spec.as_ref()?;
    let id = spec_str(spec, "id");
    if id.is_empty() {
        return None;
    }
    let description = match spec_str(spec, "description") {
        "" => spec_str(spec, "name"),
        description => description,
    };
    let label = format!(
        "agent {id} \"{}\"",
        cut(&collapse_whitespace(description), AGENT_LABEL_CHARS)
    );

    let turns = turn_spans(child, &scan, id);
    let all_ended = !turns.is_empty() && scan.turns.iter().all(|turn| turn.end.is_some());
    let last = turns.last();
    let status = if child.active {
        SpanStatus::Running
    } else {
        last.map_or(SpanStatus::Interrupted, |turn| turn.status)
    };
    let ended_at = (all_ended && !child.active)
        .then(|| last.and_then(|turn| turn.ended_at))
        .flatten();
    let mut span = new_span(
        id.to_owned(),
        SpanKind::Subagent,
        label,
        *started_at,
        status,
    );
    span.ended_at = ended_at;
    span.children = turns.into_iter().flat_map(|turn| turn.children).collect();
    number_models(&mut span.children);
    set_task_id(&mut span, id);
    Some((span, spec_str(spec, "parentToolCallId").to_owned()))
}

/// Places `agent` under the tool span of `parent_tool_call_id`, else under
/// the last turn that started at or before it.
fn place(turns: &mut [Span], agent: Span, parent_tool_call_id: &str) {
    let started_at = agent.started_at;
    let tool = (!parent_tool_call_id.is_empty())
        .then(|| {
            turns
                .iter_mut()
                .flat_map(|turn| turn.children.iter_mut())
                .find(|child| {
                    child.kind == SpanKind::Tool && child.tool_call_id == parent_tool_call_id
                })
        })
        .flatten();
    let host = match tool {
        Some(tool) => tool,
        None => match turns.iter_mut().rfind(|turn| turn.started_at <= started_at) {
            Some(turn) => turn,
            None => return,
        },
    };
    host.children.push(agent);
    host.children.sort_by_key(|child| child.started_at);
}

/// Builds the trace of a session and its sub-agents.
pub fn build(session_id: &str, parent: &TraceSource<'_>, children: &[TraceSource<'_>]) -> Trace {
    let scan = Scan::new(parent.entries);
    let mut turns = turn_spans(parent, &scan, "");
    for child in children {
        if let Some((agent, parent_tool_call_id)) = subagent_span(child) {
            place(&mut turns, agent, &parent_tool_call_id);
        }
    }
    Trace {
        session_id: session_id.to_owned(),
        turns,
    }
}

mod content;
mod spans;

pub use content::*;

use spans::*;

#[cfg(test)]
mod tests;
