//! Trace span construction for turns, tool calls and provider requests.

use super::*;

// ---------------------------------------------------------------------------
// spans of one session
// ---------------------------------------------------------------------------

pub(super) fn new_span(
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
pub(super) fn cut(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut cut: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        cut.push('…');
    }
    cut
}

pub(super) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn millis_of(message: &Message) -> i64 {
    message.created_at.timestamp_millis()
}

/// The arguments of the tool call `tool_call_id` as parsed JSON.
pub(super) fn call_arguments(
    messages: &[Message],
    tool_call_id: &str,
) -> Option<serde_json::Value> {
    messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .flat_map(|message| &message.blocks)
        .find(|block| block.block_type == BlockType::ToolCall && block.tool_call_id == tool_call_id)
        .and_then(|block| block.arguments.as_deref())
        .and_then(|raw| serde_json::from_str(raw.get()).ok())
}

/// The first string argument, or the compact JSON when there is none.
pub(super) fn argument_preview(arguments: &serde_json::Value) -> String {
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

pub(super) fn prefixed(prefix: &str, id: &str) -> String {
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
pub(super) fn number_models(spans: &mut [Span]) {
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
pub(super) fn turn_spans(source: &TraceSource<'_>, scan: &Scan, prefix: &str) -> Vec<Span> {
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
