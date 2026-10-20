//! Rendering of the content sections of one trace span.

use super::*;

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

pub(super) fn section(title: &str, text: String) -> Option<ContentSection> {
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
