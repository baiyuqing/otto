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
// spans of one session
// ---------------------------------------------------------------------------

fn new_span(
    id: String,
    kind: SpanKind,
    label: String,
    started_at: i64,
    status: SpanStatus,
) -> Span {
    Span {
        id,
        kind,
        label,
        started_at,
        ended_at: None,
        status,
        estimated: false,
        provider: None,
        tool_call_id: String::new(),
        task_id: String::new(),
        triggered_by: Vec::new(),
        children: Vec::new(),
    }
}

fn status_of(disposition: OperationDisposition) -> SpanStatus {
    match disposition {
        OperationDisposition::Succeeded => SpanStatus::Ok,
        OperationDisposition::Cancelled => SpanStatus::Cancelled,
        OperationDisposition::Error | OperationDisposition::DeadlineExceeded => SpanStatus::Error,
        OperationDisposition::Interrupted => SpanStatus::Interrupted,
    }
}

/// `text` cut to `limit` characters, with `…` appended when cut.
fn cut(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut cut: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        cut.push('…');
    }
    cut
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn millis_of(message: &Message) -> i64 {
    message.created_at.timestamp_millis()
}

/// The arguments of the tool call `tool_call_id` as parsed JSON.
fn call_arguments(messages: &[Message], tool_call_id: &str) -> Option<serde_json::Value> {
    messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .flat_map(|message| &message.blocks)
        .find(|block| block.block_type == BlockType::ToolCall && block.tool_call_id == tool_call_id)
        .and_then(|block| block.arguments.as_deref())
        .and_then(|raw| serde_json::from_str(raw.get()).ok())
}

/// The first string argument, or the compact JSON when there is none.
fn argument_preview(arguments: &serde_json::Value) -> String {
    let first = arguments
        .as_object()
        .and_then(|object| object.values().find_map(serde_json::Value::as_str));
    let text = first.map_or_else(|| arguments.to_string(), str::to_owned);
    cut(&collapse_whitespace(&text), TOOL_PREVIEW_CHARS)
}

fn tool_span(tool: &ToolFact, running: bool, messages: &[Message], prefix: &str) -> Span {
    let status = match (tool.disposition, running) {
        (Some(disposition), _) => status_of(disposition),
        (None, true) => SpanStatus::Running,
        (None, false) => SpanStatus::Interrupted,
    };
    let mut label = tool.tool_name.clone();
    if let Some(arguments) = call_arguments(messages, &tool.tool_call_id) {
        let preview = argument_preview(&arguments);
        if !preview.is_empty() {
            label = format!("{label} {preview}");
        }
    }
    let mut span = new_span(
        prefixed(prefix, &tool.operation_id),
        SpanKind::Tool,
        label,
        tool.start,
        status,
    );
    span.ended_at = tool.end;
    span.tool_call_id = tool.tool_call_id.clone();
    span
}

fn prefixed(prefix: &str, id: &str) -> String {
    if prefix.is_empty() {
        id.to_owned()
    } else {
        format!("{prefix}:{id}")
    }
}

fn call_span(call: &ProviderCallRecord, prefix: &str) -> Span {
    let ordinary = call.kind == ProviderCallKind::Ordinary;
    let mut span = new_span(
        prefixed(prefix, &call.operation_id),
        if ordinary {
            SpanKind::Provider
        } else {
            SpanKind::Compaction
        },
        if ordinary { "model" } else { "compaction" }.to_owned(),
        call.started_at,
        status_of(call.outcome.disposition),
    );
    span.ended_at = Some(
        call.started_at
            .saturating_add(i64::try_from(call.duration_ms).unwrap_or(i64::MAX)),
    );
    span.provider = Some(ProviderTiming {
        first_output_ms: call.first_output_ms,
        attempts: call.attempts,
        retries: call.retries.clone(),
        usage: call.usage,
        finish_reason: call.finish_reason.clone(),
        request: call.request.clone(),
    });
    span
}

/// Labels the provider spans among `spans` `model #1`, `model #2`, ... in
/// order.
fn number_models(spans: &mut [Span]) {
    let mut number = 0;
    for span in spans
        .iter_mut()
        .filter(|span| span.kind == SpanKind::Provider)
    {
        number += 1;
        span.label = format!("model #{number}");
    }
}

fn trigger_label(trigger: TurnTrigger) -> &'static str {
    match trigger {
        TurnTrigger::User => "user",
        TurnTrigger::Wake => "wake",
        TurnTrigger::Recovery => "recovery",
    }
}

/// One span per turn of a session, each holding its provider, tool and
/// compaction spans. `prefix` is prepended to the ids of the inner spans.
fn turn_spans(source: &TraceSource<'_>, scan: &Scan, prefix: &str) -> Vec<Span> {
    let last_entry = scan.times.last().copied().unwrap_or_default();
    let has_calls = !scan.calls.is_empty();
    let mut turns = Vec::new();
    for (index, turn) in scan.turns.iter().enumerate() {
        // A turn without an end ends at the next turn's start at the latest.
        let bound = turn
            .end
            .or_else(|| scan.turns.get(index + 1).map(|next| next.start))
            .unwrap_or(i64::MAX);
        let in_range = |at: i64| at >= turn.start && at <= bound;
        let open = turn.end.is_none();
        let running = open && source.active;
        let status = match (turn.outcome, running) {
            (Some(TurnOutcome::Completed), _) => SpanStatus::Ok,
            (Some(TurnOutcome::Failed), _) => SpanStatus::Error,
            (Some(TurnOutcome::Cancelled), _) => SpanStatus::Cancelled,
            (None, true) => SpanStatus::Running,
            (None, false) => SpanStatus::Interrupted,
        };
        let mut span = new_span(
            turn.id.clone(),
            SpanKind::Turn,
            trigger_label(turn.trigger).to_owned(),
            turn.start,
            status,
        );
        span.ended_at = turn.end;
        span.triggered_by = source
            .messages
            .iter()
            .filter(|message| message.role == Role::Context && in_range(millis_of(message)))
            .filter_map(|message| message.context_metadata.as_ref())
            .filter(|metadata| !metadata.task_id.is_empty())
            .map(|metadata| metadata.task_id.clone())
            .collect();

        let mut children = Vec::new();
        let mut tool_open = false;
        for tool in scan.tools.iter().filter(|tool| in_range(tool.start)) {
            tool_open |= tool.end.is_none();
            children.push(tool_span(tool, running, source.messages, prefix));
        }
        children.extend(
            scan.calls
                .iter()
                .filter(|call| in_range(call.started_at))
                .map(|call| call_span(call, prefix)),
        );
        if !has_calls {
            children.extend(estimated_provider_spans(source, scan, turn, bound, prefix));
        }
        children.sort_by_key(|child| child.started_at);
        number_models(&mut children);
        if running && !tool_open {
            let pending = new_span(
                prefixed(prefix, &format!("{}:pending", turn.id)),
                SpanKind::Provider,
                String::new(),
                last_entry.max(turn.start),
                SpanStatus::Running,
            );
            children.push(pending);
            number_models(&mut children);
        }
        span.children = children;
        turns.push(span);
    }
    turns
}

/// Provider spans for a session written before `otto.provider_call`: each
/// runs from the entry before an assistant message to that message.
fn estimated_provider_spans(
    source: &TraceSource<'_>,
    scan: &Scan,
    turn: &TurnFact,
    bound: i64,
    prefix: &str,
) -> Vec<Span> {
    source
        .messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .filter(|message| (turn.start..=bound).contains(&millis_of(message)))
        .enumerate()
        .map(|(index, message)| {
            let end = millis_of(message);
            let start = scan
                .times
                .iter()
                .copied()
                .filter(|time| *time < end)
                .max()
                .unwrap_or(turn.start)
                .max(turn.start);
            let mut span = new_span(
                prefixed(prefix, &format!("{}:m{}", turn.id, index + 1)),
                SpanKind::Provider,
                String::new(),
                start,
                SpanStatus::Ok,
            );
            span.ended_at = Some(end);
            span.estimated = true;
            span.provider = Some(ProviderTiming {
                usage: message.usage,
                finish_reason: message.finish_reason.clone(),
                ..ProviderTiming::default()
            });
            span
        })
        .collect()
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

// ---------------------------------------------------------------------------
// span content
// ---------------------------------------------------------------------------

struct Found<'a> {
    span: &'a Span,
    /// The spans next to it, in order, and its position among them.
    siblings: &'a [Span],
    index: usize,
    /// Where the enclosing span starts.
    container_start: i64,
}

fn find<'a>(spans: &'a [Span], id: &str, container_start: i64) -> Option<Found<'a>> {
    for (index, span) in spans.iter().enumerate() {
        if span.id == id {
            return Some(Found {
                span,
                siblings: spans,
                index,
                container_start,
            });
        }
        if let Some(found) = find(&span.children, id, span.started_at) {
            return Some(found);
        }
    }
    None
}

fn section(title: &str, text: String) -> Option<ContentSection> {
    (!text.is_empty()).then(|| ContentSection {
        title: title.to_owned(),
        text,
    })
}

/// A message as `<role>: <text>`; tool calls are `name(arguments)`.
fn render_message(message: &Message) -> String {
    let role: String = message.role.clone().into();
    format!("{role}: {}", render_blocks(message))
}

fn render_blocks(message: &Message) -> String {
    message
        .blocks
        .iter()
        .filter_map(|block| match block.block_type {
            BlockType::Text | BlockType::ToolResult => Some(block.text.clone()),
            BlockType::ToolCall => Some(format!(
                "{}({})",
                block.tool_name,
                block.arguments.as_deref().map_or("", RawValue::get)
            )),
            BlockType::Image => Some(format!("[image {}]", block.mime_type)),
            BlockType::Reasoning | BlockType::Other(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_request(request: &RequestEstimate) -> String {
    let mut lines = vec![
        format!("system prompt  {} tokens", request.system_prompt_tokens),
        format!("tools  {} tokens", request.tool_tokens),
        format!("messages  {} tokens", request.message_tokens),
        format!("memory  {} tokens", request.memory_tokens),
    ];
    if !request.memory_record_ids.is_empty() {
        lines.push(format!(
            "memory records: {}",
            request.memory_record_ids.join(", ")
        ));
    }
    lines.join("\n")
}

fn provider_content(found: &Found<'_>, messages: &[Message]) -> Vec<ContentSection> {
    let span = found.span;
    // Input starts where the previous provider request started, so the
    // reply that request produced is part of it.
    let lower = found.siblings[..found.index]
        .iter()
        .rfind(|sibling| sibling.kind == SpanKind::Provider)
        .map_or(found.container_start, |previous| previous.started_at);
    let input = messages
        .iter()
        .filter(|message| (lower..span.started_at).contains(&millis_of(message)))
        .map(render_message)
        .collect::<Vec<_>>()
        .join("\n\n");
    let reply = messages
        .iter()
        .find(|message| message.role == Role::Assistant && millis_of(message) >= span.started_at)
        .map(render_blocks)
        .unwrap_or_default();
    let request = span
        .provider
        .as_ref()
        .and_then(|timing| timing.request.as_ref())
        .map(render_request)
        .unwrap_or_default();
    [
        section("input", input),
        section("request", request),
        section("reply", reply),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn tool_content(span: &Span, messages: &[Message]) -> Vec<ContentSection> {
    let arguments = call_arguments(messages, &span.tool_call_id)
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_default();
    let result = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| {
            block.block_type == BlockType::ToolResult && block.tool_call_id == span.tool_call_id
        })
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    [section("arguments", arguments), section("result", result)]
        .into_iter()
        .flatten()
        .collect()
}

fn turn_content(found: &Found<'_>, messages: &[Message]) -> Vec<ContentSection> {
    let span = found.span;
    let bound = span
        .ended_at
        .or_else(|| {
            found
                .siblings
                .get(found.index + 1)
                .map(|next| next.started_at)
        })
        .unwrap_or(i64::MAX);
    let input = messages
        .iter()
        .filter(|message| (span.started_at..=bound).contains(&millis_of(message)))
        .filter(|message| {
            message.role == Role::User
                || (message.role == Role::Context
                    && message
                        .context_metadata
                        .as_ref()
                        .is_some_and(|metadata| !metadata.task_id.is_empty()))
        })
        .map(Message::text)
        .collect::<Vec<_>>()
        .join("\n\n");
    section("input", input).into_iter().collect()
}

/// The text behind one span of the trace `build` returns, or `None` when no
/// span has that id. Session content is already redacted when written.
pub fn span_content(
    parent: &TraceSource<'_>,
    children: &[TraceSource<'_>],
    span_id: &str,
) -> Option<SpanContent> {
    let trace = build("", parent, children);
    let found = find(&trace.turns, span_id, i64::MIN)?;
    let span = found.span;
    // A span inside a child session reads that child's entries and messages.
    let child = (!span.task_id.is_empty())
        .then(|| {
            children.iter().find(|child| {
                Scan::new(child.entries)
                    .spec
                    .is_some_and(|(_, spec)| spec_str(&spec, "id") == span.task_id)
            })
        })
        .flatten();
    let source = child.unwrap_or(parent);
    let sections = match span.kind {
        SpanKind::Turn => turn_content(&found, source.messages),
        SpanKind::Tool => tool_content(span, source.messages),
        SpanKind::Provider => provider_content(&found, source.messages),
        SpanKind::Compaction => {
            let after = span.ended_at.unwrap_or(span.started_at);
            let summary = Scan::new(source.entries)
                .compactions
                .into_iter()
                .find(|(at, _)| *at >= after)
                .map(|(_, summary)| summary)
                .unwrap_or_default();
            section("summary", summary).into_iter().collect()
        }
        SpanKind::Subagent => {
            let spec = Scan::new(source.entries).spec.map(|(_, spec)| spec);
            let prompt = spec
                .as_ref()
                .map(|spec| spec_str(spec, "prompt").to_owned())
                .unwrap_or_default();
            let result = source
                .messages
                .iter()
                .rfind(|message| message.role == Role::Assistant && !message.text().is_empty())
                .map(Message::text)
                .unwrap_or_default();
            [section("prompt", prompt), section("result", result)]
                .into_iter()
                .flatten()
                .collect()
        }
    };
    Some(SpanContent {
        span_id: span_id.to_owned(),
        sections,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, SecondsFormat, Utc};

    use super::*;
    use crate::model::{
        Block, ContextMetadata, EffectCertainty, OperationId, OperationOutcome, Role,
    };
    use crate::session::operation::encode_operation_fact;
    use crate::session::pi::{PiCompaction, PiCustom};
    use crate::session::turn::encode_turn_record;

    const BASE: i64 = 1_800_000_000_000;

    fn time(offset: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(BASE + offset).expect("in range")
    }

    fn custom(at: i64, custom_type: &str, data: &str) -> PiEntry {
        let mut entry = PiEntry::new(
            "custom",
            &format!("e{at}"),
            None,
            &time(at).to_rfc3339_opts(SecondsFormat::Millis, true),
        );
        entry.custom = Some(PiCustom {
            custom_type: custom_type.into(),
            data: Some(RawValue::from_string(data.into()).expect("json")),
        });
        entry
    }

    fn turn_start(at: i64, id: &str, trigger: TurnTrigger) -> PiEntry {
        let record = TurnRecord::started(id, trigger, 1, None);
        custom(at, TURN_CUSTOM_TYPE, &encode_turn_record(&record))
    }

    fn turn_end(at: i64, id: &str, outcome: TurnOutcome) -> PiEntry {
        let record = TurnRecord::ended(id, outcome);
        custom(at, TURN_CUSTOM_TYPE, &encode_turn_record(&record))
    }

    fn tool_attempt(at: i64, op: &str, call: &str, name: &str) -> PiEntry {
        let fact = OperationFact::attempt(OperationId::new(op).unwrap(), 1, call, name);
        custom(
            at,
            OPERATION_CUSTOM_TYPE,
            &encode_operation_fact(&fact).unwrap(),
        )
    }

    fn tool_terminal(at: i64, op: &str, call: &str, name: &str, ok: bool) -> PiEntry {
        let outcome = OperationOutcome {
            disposition: if ok {
                OperationDisposition::Succeeded
            } else {
                OperationDisposition::Error
            },
            effect_certainty: EffectCertainty::Completed,
            stop_reason: None,
        };
        let fact = OperationFact::terminal(OperationId::new(op).unwrap(), 1, call, name, outcome);
        custom(
            at,
            OPERATION_CUSTOM_TYPE,
            &encode_operation_fact(&fact).unwrap(),
        )
    }

    fn model_call(at: i64, op: &str, kind: ProviderCallKind, started: i64, ms: u64) -> PiEntry {
        let record = ProviderCallRecord {
            schema_version: 1,
            operation_id: op.into(),
            kind,
            started_at: BASE + started,
            duration_ms: ms,
            first_output_ms: Some(5),
            attempts: 2,
            retries: vec![ProviderRetryRecord {
                attempt: 2,
                max_attempts: 3,
                delay_ms: 10,
                reason: "timeout".into(),
                interrupted_output: false,
                at_ms: 3,
            }],
            outcome: OperationOutcome {
                disposition: OperationDisposition::Succeeded,
                effect_certainty: EffectCertainty::Completed,
                stop_reason: None,
            },
            finish_reason: Some(FinishReason::ToolCalls),
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 7,
                cached_input_tokens: 0,
            }),
            request: Some(RequestEstimate {
                system_prompt_tokens: 11,
                tool_tokens: 22,
                message_tokens: 33,
                memory_tokens: 44,
                memory_record_ids: vec!["m1".into(), "m2".into()],
            }),
        };
        custom(
            at,
            PROVIDER_CALL_CUSTOM_TYPE,
            &serde_json::to_string(&record).unwrap(),
        )
    }

    fn message(at: i64, role: Role, blocks: Vec<Block>) -> Message {
        Message {
            role,
            blocks,
            created_at: time(at),
            ..Message::default()
        }
    }

    fn text(at: i64, role: Role, text: &str) -> Message {
        message(at, role, vec![Block::text(text)])
    }

    fn tool_call(call: &str, name: &str, arguments: &str) -> Block {
        Block {
            block_type: BlockType::ToolCall,
            tool_call_id: call.into(),
            tool_name: name.into(),
            arguments: Some(RawValue::from_string(arguments.into()).unwrap()),
            ..Block::default()
        }
    }

    fn tool_result(call: &str, text: &str) -> Block {
        Block {
            block_type: BlockType::ToolResult,
            tool_call_id: call.into(),
            text: text.into(),
            ..Block::default()
        }
    }

    fn marker(at: i64) -> PiEntry {
        PiEntry::new(
            "message",
            &format!("m{at}"),
            None,
            &time(at).to_rfc3339_opts(SecondsFormat::Millis, true),
        )
    }

    /// One finished user turn: two model requests around a bash call, then a
    /// compaction.
    fn finished_turn() -> (Vec<PiEntry>, Vec<Message>) {
        let mut compaction = PiEntry::new(
            "compaction",
            "c1",
            None,
            &time(560).to_rfc3339_opts(SecondsFormat::Millis, true),
        );
        compaction.compaction = Some(Box::new(PiCompaction {
            summary: "the summary".into(),
            ..PiCompaction::default()
        }));
        let entries = vec![
            turn_start(0, "T1", TurnTrigger::User),
            model_call(100, "op1", ProviderCallKind::Ordinary, 10, 90),
            tool_attempt(110, "op2", "c1", "bash"),
            tool_terminal(400, "op2", "c1", "bash", true),
            model_call(500, "op3", ProviderCallKind::Ordinary, 410, 90),
            model_call(550, "op4", ProviderCallKind::LocalCompaction, 520, 30),
            compaction,
            turn_end(600, "T1", TurnOutcome::Completed),
        ];
        let messages = vec![
            text(5, Role::User, "run the tests"),
            message(
                100,
                Role::Assistant,
                vec![tool_call(
                    "c1",
                    "bash",
                    r#"{"command":"cargo   test\n --all","timeout":5}"#,
                )],
            ),
            message(400, Role::Tool, vec![tool_result("c1", "all passed")]),
            text(500, Role::Assistant, "done"),
        ];
        (entries, messages)
    }

    fn source<'a>(
        entries: &'a [PiEntry],
        messages: &'a [Message],
        active: bool,
    ) -> TraceSource<'a> {
        TraceSource {
            entries,
            messages,
            active,
        }
    }

    fn ids(spans: &[Span]) -> Vec<&str> {
        spans.iter().map(|span| span.id.as_str()).collect()
    }

    #[test]
    fn a_finished_turn_nests_provider_tool_and_compaction_spans_in_time_order() {
        let (entries, messages) = finished_turn();
        let trace = build("s1", &source(&entries, &messages, false), &[]);
        assert_eq!(trace.session_id, "s1");
        assert_eq!(trace.turns.len(), 1);
        let turn = &trace.turns[0];
        assert_eq!(
            (turn.id.as_str(), turn.label.as_str(), turn.status),
            ("T1", "user", SpanStatus::Ok)
        );
        assert_eq!((turn.started_at, turn.ended_at), (BASE, Some(BASE + 600)));
        assert_eq!(ids(&turn.children), ["op1", "op2", "op3", "op4"]);
        let labels: Vec<_> = turn.children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "model #1",
                "bash cargo test --all",
                "model #2",
                "compaction"
            ]
        );
        let kinds: Vec<_> = turn.children.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                SpanKind::Provider,
                SpanKind::Tool,
                SpanKind::Provider,
                SpanKind::Compaction
            ]
        );
        let first = &turn.children[0];
        assert_eq!(
            (first.started_at, first.ended_at),
            (BASE + 10, Some(BASE + 100))
        );
        let timing = first.provider.as_ref().expect("timing");
        assert_eq!((timing.first_output_ms, timing.attempts), (Some(5), 2));
        assert_eq!(timing.retries[0].at_ms, 3);
        assert_eq!(timing.usage.map(|usage| usage.input_tokens), Some(100));
        assert_eq!(timing.finish_reason, Some(FinishReason::ToolCalls));
        let tool = &turn.children[1];
        assert_eq!(
            (tool.started_at, tool.ended_at),
            (BASE + 110, Some(BASE + 400))
        );
        assert_eq!(
            (tool.tool_call_id.as_str(), tool.status),
            ("c1", SpanStatus::Ok)
        );
        assert!(!first.estimated);
    }

    #[test]
    fn a_failed_tool_terminal_marks_the_span_as_an_error() {
        let entries = vec![
            turn_start(0, "T1", TurnTrigger::User),
            tool_attempt(10, "op1", "c1", "bash"),
            tool_terminal(20, "op1", "c1", "bash", false),
            turn_end(30, "T1", TurnOutcome::Failed),
        ];
        let trace = build("s", &source(&entries, &[], false), &[]);
        assert_eq!(trace.turns[0].status, SpanStatus::Error);
        assert_eq!(trace.turns[0].children[0].status, SpanStatus::Error);
    }

    #[test]
    fn an_open_active_turn_gets_a_running_model_span_unless_a_tool_is_open() {
        let mut entries = vec![
            turn_start(0, "T1", TurnTrigger::User),
            model_call(100, "op1", ProviderCallKind::Ordinary, 10, 90),
            tool_attempt(110, "op2", "c1", "bash"),
            tool_terminal(300, "op2", "c1", "bash", true),
        ];
        let trace = build("s", &source(&entries, &[], true), &[]);
        let turn = &trace.turns[0];
        assert_eq!((turn.status, turn.ended_at), (SpanStatus::Running, None));
        let pending = turn.children.last().expect("pending span");
        assert_eq!(pending.id, "T1:pending");
        assert_eq!(pending.label, "model #2");
        assert_eq!(
            (pending.started_at, pending.ended_at, pending.status),
            (BASE + 300, None, SpanStatus::Running)
        );

        // With the tool still open there is no pending model span.
        entries.pop();
        let trace = build("s", &source(&entries, &[], true), &[]);
        let turn = &trace.turns[0];
        assert_eq!(ids(&turn.children), ["op1", "op2"]);
        assert_eq!(turn.children[1].status, SpanStatus::Running);
        assert_eq!(turn.children[1].ended_at, None);
    }

    #[test]
    fn an_open_turn_in_an_idle_session_is_interrupted() {
        let entries = vec![
            turn_start(0, "T1", TurnTrigger::User),
            tool_attempt(110, "op2", "c1", "bash"),
        ];
        let trace = build("s", &source(&entries, &[], false), &[]);
        let turn = &trace.turns[0];
        assert_eq!(
            (turn.status, turn.ended_at),
            (SpanStatus::Interrupted, None)
        );
        assert_eq!(turn.children.len(), 1);
        assert_eq!(turn.children[0].status, SpanStatus::Interrupted);
    }

    #[test]
    fn a_wake_turn_lists_the_tasks_whose_notifications_it_delivered() {
        let entries = vec![
            turn_start(0, "T1", TurnTrigger::User),
            turn_end(100, "T1", TurnOutcome::Completed),
            turn_start(1000, "T2", TurnTrigger::Wake),
            turn_end(1100, "T2", TurnOutcome::Completed),
        ];
        let notification = |at: i64, task: &str| Message {
            context_metadata: Some(ContextMetadata {
                task_id: task.into(),
            }),
            ..text(at, Role::Context, "finished")
        };
        let messages = vec![notification(50, "t9"), notification(1005, "t3")];
        let trace = build("s", &source(&entries, &messages, false), &[]);
        assert_eq!(trace.turns[1].label, "wake");
        assert_eq!(trace.turns[1].triggered_by, ["t3"]);
        assert_eq!(trace.turns[0].triggered_by, ["t9"]);
    }

    fn child_entries(task: &str, extra_spec: &str, at: i64) -> Vec<PiEntry> {
        let spec = format!(
            r#"{{"id":"{task}","name":"review","description":"review the diff","prompt":"look at it"{extra_spec}}}"#
        );
        vec![
            custom(at, TASK_SPEC_CUSTOM_TYPE, &spec),
            turn_start(at + 10, "CT", TurnTrigger::User),
            model_call(at + 70, "op1", ProviderCallKind::Ordinary, at + 20, 50),
            tool_attempt(at + 80, "op2", "k1", "bash"),
            tool_terminal(at + 150, "op2", "k1", "bash", true),
            turn_end(at + 160, "CT", TurnOutcome::Completed),
        ]
    }

    #[test]
    fn a_child_is_nested_by_parent_tool_call_id_or_else_by_time() {
        let entries = vec![
            turn_start(0, "T1", TurnTrigger::User),
            model_call(90, "p1", ProviderCallKind::Ordinary, 20, 70),
            tool_attempt(100, "op1", "c7", "agent"),
            tool_terminal(900, "op1", "c7", "agent", true),
            turn_end(1000, "T1", TurnOutcome::Completed),
        ];
        let messages = vec![message(
            100,
            Role::Assistant,
            vec![tool_call("c7", "agent", r#"{"prompt":"review"}"#)],
        )];
        let linked = child_entries("t1", r#","parentToolCallId":"c7""#, 150);
        let linked_messages = vec![
            text(180, Role::Assistant, "first"),
            text(300, Role::Assistant, "the review"),
        ];
        let unlinked = child_entries("t2", "", 500);
        let trace = build(
            "s",
            &source(&entries, &messages, false),
            &[
                source(&linked, &linked_messages, false),
                source(&unlinked, &[], true),
            ],
        );
        let turn = &trace.turns[0];
        assert_eq!(ids(&turn.children), ["p1", "op1", "t2"]);

        let agent = &turn.children[1].children[0];
        assert_eq!(agent.kind, SpanKind::Subagent);
        assert_eq!(agent.id, "t1");
        assert_eq!(agent.label, r#"agent t1 "review the diff""#);
        assert_eq!(agent.task_id, "t1");
        assert_eq!(
            (agent.started_at, agent.ended_at),
            (BASE + 150, Some(BASE + 310))
        );
        assert_eq!(agent.status, SpanStatus::Ok);
        // Child ids are prefixed, so equal operation ids do not collide.
        assert_eq!(ids(&agent.children), ["t1:op1", "t1:op2"]);
        assert_eq!(agent.children[0].label, "model #1");
        assert!(agent.children.iter().all(|span| span.task_id == "t1"));

        let by_time = &turn.children[2];
        assert_eq!(
            (by_time.id.as_str(), by_time.status),
            ("t2", SpanStatus::Running)
        );
        assert_eq!(by_time.ended_at, None);
        assert_eq!(by_time.task_id, "t2");
    }

    #[test]
    fn a_session_without_provider_calls_gets_estimated_provider_spans() {
        let entries = vec![
            turn_start(0, "T1", TurnTrigger::User),
            marker(5),
            marker(300),
            marker(500),
            turn_end(600, "T1", TurnOutcome::Completed),
        ];
        let mut reply = text(500, Role::Assistant, "hi");
        reply.usage = Some(Usage {
            input_tokens: 9,
            output_tokens: 1,
            cached_input_tokens: 0,
        });
        reply.finish_reason = Some(FinishReason::Stop);
        let messages = vec![text(5, Role::User, "hello"), reply];
        let trace = build("s", &source(&entries, &messages, false), &[]);
        let spans = &trace.turns[0].children;
        assert_eq!(spans.len(), 1);
        let provider = &spans[0];
        assert_eq!(provider.label, "model #1");
        assert!(provider.estimated);
        assert_eq!(
            (provider.started_at, provider.ended_at),
            (BASE + 300, Some(BASE + 500))
        );
        let timing = provider.provider.as_ref().expect("timing");
        assert_eq!(timing.usage.map(|usage| usage.input_tokens), Some(9));
        assert_eq!(timing.finish_reason, Some(FinishReason::Stop));
        assert_eq!(timing.attempts, 0);
        assert!(timing.request.is_none());
    }

    fn section<'a>(content: &'a SpanContent, title: &str) -> &'a str {
        &content
            .sections
            .iter()
            .find(|section| section.title == title)
            .unwrap_or_else(|| panic!("no section {title}"))
            .text
    }

    #[test]
    fn span_content_reads_tool_provider_turn_and_compaction_text() {
        let (entries, messages) = finished_turn();
        let parent = source(&entries, &messages, false);

        let tool = span_content(&parent, &[], "op2").expect("tool content");
        assert!(section(&tool, "arguments").contains("\"command\": \"cargo   test\\n --all\""));
        assert_eq!(section(&tool, "result"), "all passed");

        let second = span_content(&parent, &[], "op3").expect("provider content");
        // Input: the reply and tool result since the previous request began.
        let input = section(&second, "input");
        assert!(input.starts_with("assistant: bash({"), "{input}");
        assert!(input.contains("tool: all passed"));
        assert!(!input.contains("run the tests"));
        assert_eq!(section(&second, "reply"), "done");
        let request = section(&second, "request");
        assert!(request.contains("system prompt  11 tokens"));
        assert!(request.contains("memory records: m1, m2"));

        let first = span_content(&parent, &[], "op1").expect("provider content");
        assert!(section(&first, "input").contains("user: run the tests"));
        assert!(section(&first, "reply").starts_with("bash({"));

        let turn = span_content(&parent, &[], "T1").expect("turn content");
        assert_eq!(section(&turn, "input"), "run the tests");

        let compaction = span_content(&parent, &[], "op4").expect("compaction content");
        assert_eq!(section(&compaction, "summary"), "the summary");

        assert_eq!(span_content(&parent, &[], "nope"), None);
    }

    #[test]
    fn span_content_of_a_subagent_reads_the_child_session() {
        let entries = vec![
            turn_start(0, "T1", TurnTrigger::User),
            turn_end(1000, "T1", TurnOutcome::Completed),
        ];
        let child = child_entries("t1", "", 150);
        let child_messages = vec![
            text(180, Role::Assistant, "first"),
            text(300, Role::Assistant, "the review"),
        ];
        let parent = source(&entries, &[], false);
        let children = [source(&child, &child_messages, false)];
        let agent = span_content(&parent, &children, "t1").expect("agent content");
        assert_eq!(section(&agent, "prompt"), "look at it");
        assert_eq!(section(&agent, "result"), "the review");
        let tool = span_content(&parent, &children, "t1:op2").expect("child tool");
        assert!(tool.sections.is_empty());
    }

    #[test]
    fn trace_json_is_camel_case_and_omits_empty_fields() {
        let (entries, messages) = finished_turn();
        let trace = build("s1", &source(&entries, &messages, false), &[]);
        let json = serde_json::to_value(&trace).expect("encode");
        assert_eq!(json["sessionId"], "s1");
        let tool = &json["turns"][0]["children"][1];
        assert_eq!(tool["startedAt"], BASE + 110);
        assert_eq!(tool["toolCallId"], "c1");
        assert_eq!(tool["kind"], "tool");
        for key in ["estimated", "provider", "taskId", "triggeredBy", "children"] {
            assert!(tool.get(key).is_none(), "{key} present on a leaf tool span");
        }
        let model = &json["turns"][0]["children"][0];
        assert_eq!(model["provider"]["firstOutputMs"], 5);
        assert_eq!(model["provider"]["retries"][0]["atMs"], 3);
        assert_eq!(model["provider"]["request"]["memoryRecordIds"][1], "m2");
        let back: Trace = serde_json::from_value(json).expect("decode");
        assert_eq!(back, trace);
    }

    #[test]
    fn previews_are_cut_on_character_boundaries() {
        let long = "界".repeat(70);
        let preview = argument_preview(&serde_json::json!({ "path": long }));
        assert_eq!(preview.chars().count(), TOOL_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
        assert_eq!(
            argument_preview(&serde_json::json!({ "n": 1 })),
            r#"{"n":1}"#
        );
        assert_eq!(cut("short", 60), "short");
    }
}
