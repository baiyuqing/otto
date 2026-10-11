use std::collections::{BTreeMap, HashSet};

use chrono::Utc;
use otto_core::approval::Request as ApprovalRequest;
use otto_core::model::FinishReason;
use otto_core::session::trace::{Span, SpanKind, SpanStatus, Trace};

use super::{Approval, Document, Presenter, Transition};
use crate::subagent::format::one_line;
use crate::tui::backend::{Reply, Request};
use crate::tui::interaction::{Action, Activation, Bar, Row, Segment, SegmentKind, Timeline, View};
use crate::tui::layout::format_token_count;

const PREVIOUS: &str = "turn:previous";
const NEXT: &str = "turn:next";
const APPROVAL: &str = "approval";

/// The `/trace` panel: one turn's span tree with a timeline, and the other
/// turns as collapsed rows.
pub(crate) struct TraceView {
    trace: Option<Trace>,
    /// The turn the user chose; the last turn while unset.
    turn: Option<String>,
    /// Sub-agent spans whose expansion differs from the default: running
    /// ones are expanded, finished ones collapsed.
    toggled: HashSet<String>,
    approvals: Vec<ApprovalRequest>,
}

impl TraceView {
    pub(crate) fn new() -> Self {
        Self {
            trace: None,
            turn: None,
            toggled: HashSet::new(),
            approvals: Vec::new(),
        }
    }

    fn selected(&self) -> Option<usize> {
        let trace = self.trace.as_ref()?;
        self.turn
            .as_ref()
            .and_then(|id| trace.turns.iter().position(|turn| &turn.id == id))
            .or(trace.turns.len().checked_sub(1))
    }

    fn expanded(&self, span: &Span) -> bool {
        (span.status == SpanStatus::Running) != self.toggled.contains(&span.id)
    }

    fn shift(&mut self, delta: isize) {
        let (Some(index), Some(trace)) = (self.selected(), &self.trace) else {
            return;
        };
        let next = (index as isize + delta).clamp(0, trace.turns.len() as isize - 1) as usize;
        self.turn = Some(trace.turns[next].id.clone());
    }

    /// The oldest approval waiting for a human decision.
    fn oldest_approval(&self) -> Option<&ApprovalRequest> {
        self.approvals
            .iter()
            .min_by_key(|request| request.created_at)
    }

    fn view_at(&self, now: i64) -> View {
        let Some(trace) = &self.trace else {
            return View {
                collection: true,
                title: "Trace".into(),
                loading: true,
                ..View::default()
            };
        };
        let Some(index) = self.selected() else {
            return View {
                collection: true,
                title: "Trace".into(),
                description: "No turns recorded yet.".into(),
                ..View::default()
            };
        };
        let selected = &trace.turns[index];
        let mut rows = Vec::new();
        let mut bars = BTreeMap::new();
        let mut add = |rows: &mut Vec<Row>, text: String, span: &Span| {
            // Only the selected turn is on the axis.
            if span.kind != SpanKind::Turn || span.id == selected.id {
                bars.insert(span.id.clone(), self.bar(span, now));
            }
            rows.push(Row {
                id: span.id.clone(),
                cells: vec![text],
                detail: detail(span, now),
            });
        };
        for (i, turn) in trace.turns.iter().enumerate() {
            if i != index {
                let text = format!("▸ {}", turn_text(i, turn, now));
                add(&mut rows, text, turn);
                continue;
            }
            add(&mut rows, format!("▾ {}", turn_text(i, turn, now)), turn);
            self.walk(turn, "  ", now, &mut |text, span| {
                add(&mut rows, text, span)
            });
        }
        let (mut input, mut output) = (0, 0);
        tally(selected, &mut input, &mut output);
        let end = span_end(selected, now);
        let notice = self.oldest_approval().map(|oldest| {
            let mut text = format!("{} approval pending · a open", self.approvals.len());
            if let Some(expires) = oldest.expires_at {
                text.push_str(&format!(
                    " · expires in {}",
                    duration((expires * 1000 - now).max(0))
                ));
            }
            text
        });
        View {
            collection: true,
            title: format!(
                "Trace · turn {}/{} · {} · {} · in {} out {}",
                index + 1,
                trace.turns.len(),
                selected.label,
                duration(end - selected.started_at),
                format_token_count(input).to_lowercase(),
                format_token_count(output).to_lowercase(),
            ),
            rows,
            default_row: Some(selected.id.clone()),
            actions: vec![
                Action {
                    id: PREVIOUS.into(),
                    label: "Previous turn".into(),
                    enabled: index > 0,
                    shortcut: Some('['),
                    ..Action::default()
                },
                Action {
                    id: NEXT.into(),
                    label: "Next turn".into(),
                    enabled: index + 1 < trace.turns.len(),
                    shortcut: Some(']'),
                    ..Action::default()
                },
                Action {
                    id: APPROVAL.into(),
                    label: "Open approval".into(),
                    enabled: notice.is_some(),
                    shortcut: Some('a'),
                    ..Action::default()
                },
            ],
            timeline: Some(Timeline {
                start_ms: selected.started_at,
                end_ms: end.max(selected.started_at + 1),
                bars,
            }),
            notice,
            ..View::default()
        }
    }

    /// Visits the descendants of `span` as tree rows. `prefix` is the text
    /// drawn left of the branch glyph.
    fn walk<'a>(
        &self,
        span: &'a Span,
        prefix: &str,
        now: i64,
        visit: &mut dyn FnMut(String, &'a Span),
    ) {
        for (i, child) in span.children.iter().enumerate() {
            let last = i + 1 == span.children.len();
            let agent = child.kind == SpanKind::Subagent;
            let marker = match (agent, self.expanded(child)) {
                (false, _) => "",
                (true, true) => "▾ ",
                (true, false) => "▸ ",
            };
            visit(
                format!(
                    "{prefix}{} {marker}{}",
                    if last { "└" } else { "├" },
                    row_text(child, now)
                ),
                child,
            );
            if !agent || self.expanded(child) {
                let next = format!(
                    "{prefix}{}{}",
                    if last { "  " } else { "│ " },
                    if agent { "  " } else { "" }
                );
                self.walk(child, &next, now, visit);
            }
        }
    }

    fn bar(&self, span: &Span, now: i64) -> Bar {
        let (start, end) = (span.started_at, span_end(span, now));
        let mut segments = Vec::new();
        let mut push = |start_ms, end_ms, kind| {
            if end_ms > start_ms {
                segments.push(Segment {
                    start_ms,
                    end_ms,
                    kind,
                });
            }
        };
        match (&span.provider, span.kind) {
            (Some(timing), SpanKind::Provider) => {
                let first = start + timing.first_output_ms.map_or(end - start, |ms| ms as i64);
                push(start, first.min(end), SegmentKind::Waiting);
                push(first.min(end), end, SegmentKind::Output);
                for retry in &timing.retries {
                    let at = start + retry.at_ms as i64;
                    push(at, at + retry.delay_ms as i64, SegmentKind::RetryWait);
                }
            }
            _ => {
                push(start, end, SegmentKind::Output);
                if let Some(request) = self.approval_for(span) {
                    push(request.created_at * 1000, end, SegmentKind::ApprovalWait);
                }
            }
        }
        Bar {
            segments,
            failed: span.status == SpanStatus::Error,
            estimated: span.estimated,
        }
    }

    fn approval_for(&self, span: &Span) -> Option<&ApprovalRequest> {
        (span.kind == SpanKind::Tool && !span.tool_call_id.is_empty())
            .then(|| {
                self.approvals.iter().find(|request| {
                    request.subject.tool_call_id == span.tool_call_id
                        && request.scope.task_id == span.task_id
                })
            })
            .flatten()
    }
}

impl Presenter for TraceView {
    fn key(&self) -> String {
        "trace".into()
    }

    fn view(&self) -> View {
        self.view_at(Utc::now().timestamp_millis())
    }

    fn activate(&mut self, activation: Activation) -> Transition {
        match activation {
            Activation::Open(id) => {
                let Some(trace) = &self.trace else {
                    return Transition::Stay;
                };
                if trace.turns.iter().any(|turn| turn.id == id)
                    && self.selected().is_some_and(|i| trace.turns[i].id != id)
                {
                    self.turn = Some(id);
                } else if find(trace, &id).is_some_and(|span| span.kind == SpanKind::Subagent) {
                    if !self.toggled.remove(&id) {
                        self.toggled.insert(id);
                    }
                } else {
                    return Transition::Request(Request::TraceSpan(id));
                }
            }
            Activation::Action { id, .. } => match id.as_str() {
                PREVIOUS => self.shift(-1),
                NEXT => self.shift(1),
                APPROVAL => {
                    if let Some(request) = self.oldest_approval() {
                        return Transition::Push(Box::new(Approval::shared(request)));
                    }
                }
                _ => {}
            },
            Activation::Back => {}
        }
        Transition::Stay
    }

    fn receive(&mut self, reply: Reply) -> Transition {
        match reply {
            Reply::Trace(trace) => self.trace = Some(trace),
            Reply::TraceSpan(content) => {
                let label = self
                    .trace
                    .as_ref()
                    .and_then(|trace| find(trace, &content.span_id))
                    .map_or(content.span_id.as_str(), |span| span.label.as_str())
                    .to_owned();
                let text = content
                    .sections
                    .iter()
                    .map(|section| format!("{}\n{}", section.title, section.text))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                return Transition::Push(Box::new(Document {
                    key: format!("trace:{}", content.span_id),
                    title: label,
                    text,
                }));
            }
            _ => {}
        }
        Transition::Stay
    }

    fn refresh(&self) -> Option<Request> {
        Some(Request::Trace)
    }

    fn periodic(&self) -> bool {
        true
    }

    fn defers_approvals(&self) -> bool {
        true
    }

    fn pending_approvals(&mut self, requests: &[ApprovalRequest]) {
        self.approvals = requests
            .iter()
            .filter(|request| request.is_human_pending())
            .cloned()
            .collect();
    }
}

fn find<'a>(trace: &'a Trace, id: &str) -> Option<&'a Span> {
    fn search<'a>(spans: &'a [Span], id: &str) -> Option<&'a Span> {
        spans.iter().find_map(|span| {
            (span.id == id)
                .then_some(span)
                .or_else(|| search(&span.children, id))
        })
    }
    search(&trace.turns, id)
}

/// The end of a span; a running span ends at `now`.
fn span_end(span: &Span, now: i64) -> i64 {
    span.ended_at
        .unwrap_or(if span.status == SpanStatus::Running {
            now.max(span.started_at)
        } else {
            span.started_at
        })
}

fn tally(span: &Span, input: &mut i64, output: &mut i64) {
    if let Some(usage) = span.provider.as_ref().and_then(|timing| timing.usage) {
        *input += usage.input_tokens;
        *output += usage.output_tokens;
    }
    for child in &span.children {
        tally(child, input, output);
    }
}

fn duration(ms: i64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{}s", ms / 60_000, ms / 1000 % 60)
    }
}

fn status_word(status: SpanStatus) -> &'static str {
    match status {
        SpanStatus::Running => "running",
        SpanStatus::Ok => "ok",
        SpanStatus::Error => "error",
        SpanStatus::Cancelled => "cancelled",
        SpanStatus::Interrupted => "interrupted",
    }
}

fn row_text(span: &Span, now: i64) -> String {
    let mut text = format!(
        "{}  {}",
        one_line(&span.label),
        duration(span_end(span, now) - span.started_at)
    );
    if span.status != SpanStatus::Ok {
        text.push_str(&format!(" {}", status_word(span.status)));
    }
    text
}

fn turn_text(index: usize, turn: &Span, now: i64) -> String {
    let mut text = format!("turn {}  {}", index + 1, row_text(turn, now));
    if !turn.triggered_by.is_empty() {
        text.push_str(&format!(" ← {}", turn.triggered_by.join(", ")));
    }
    text
}

fn finish_word(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop".into(),
        FinishReason::ToolCalls => "tool_calls".into(),
        FinishReason::Length => "length".into(),
        FinishReason::Unknown => "unknown".into(),
        FinishReason::Other(other) => other.clone(),
    }
}

/// The detail lines of a span: timing, status and, for provider spans,
/// attempts, retries, usage and the request estimates.
fn detail(span: &Span, now: i64) -> String {
    let mut head = format!(
        "{} · {} · {}",
        one_line(&span.label),
        duration(span_end(span, now) - span.started_at),
        status_word(span.status)
    );
    if span.estimated {
        head.push_str(" · estimated");
    }
    let Some(timing) = &span.provider else {
        return head;
    };
    head.push_str(&format!(" · attempts {}", timing.attempts.max(1)));
    for retry in &timing.retries {
        head.push_str(&format!(
            " · retry {}/{}: {}, waited {}",
            retry.attempt,
            retry.max_attempts,
            retry.reason,
            duration(retry.delay_ms as i64)
        ));
    }
    let mut lines = vec![head];
    let mut usage = Vec::new();
    if let Some(ms) = timing.first_output_ms {
        usage.push(format!("ttft {}", duration(ms as i64)));
    }
    if let Some(tokens) = timing.usage {
        usage.push(format!(
            "in {} (cached {})",
            format_token_count(tokens.input_tokens),
            format_token_count(tokens.cached_input_tokens)
        ));
        usage.push(format!("out {}", format_token_count(tokens.output_tokens)));
    }
    if let Some(reason) = &timing.finish_reason {
        usage.push(format!("stop {}", finish_word(reason)));
    }
    if !usage.is_empty() {
        lines.push(usage.join(" · "));
    }
    if let Some(request) = &timing.request {
        lines.push(format!(
            "request: system {} · tools {} · messages {} · memory {}{}",
            format_token_count(request.system_prompt_tokens),
            format_token_count(request.tool_tokens),
            format_token_count(request.message_tokens),
            format_token_count(request.memory_tokens),
            if request.memory_record_ids.is_empty() {
                String::new()
            } else {
                format!(" ({})", request.memory_record_ids.join(", "))
            }
        ));
    }
    lines.join("\n")
}

/// One turn of 48 s: a model call, a bash call, and an agent call whose
/// sub-agent ran a model call and a bash call. Times are unix milliseconds.
#[cfg(test)]
pub(crate) fn sample_trace() -> Trace {
    use otto_core::model::Usage;
    use otto_core::session::provider_call::ProviderRetryRecord;
    use otto_core::session::trace::ProviderTiming;

    const T0: i64 = 1_800_000_000_000;
    fn span(
        id: &str,
        kind: SpanKind,
        label: &str,
        start: i64,
        end: i64,
        children: Vec<Span>,
    ) -> Span {
        Span {
            id: id.into(),
            kind,
            label: label.into(),
            started_at: T0 + start,
            ended_at: Some(T0 + end),
            status: SpanStatus::Ok,
            estimated: false,
            provider: None,
            tool_call_id: String::new(),
            task_id: String::new(),
            triggered_by: Vec::new(),
            children,
        }
    }
    let mut model = span("m1", SpanKind::Provider, "model #1", 0, 3_600, vec![]);
    model.provider = Some(ProviderTiming {
        first_output_ms: Some(3_100),
        attempts: 2,
        retries: vec![ProviderRetryRecord {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 500,
            reason: "connection interrupted".into(),
            interrupted_output: false,
            at_ms: 1_000,
        }],
        usage: Some(Usage {
            input_tokens: 31_400,
            output_tokens: 2_100,
            cached_input_tokens: 11_200,
        }),
        finish_reason: Some(FinishReason::ToolCalls),
        request: None,
    });
    let mut bash = span(
        "b1",
        SpanKind::Tool,
        "bash cargo test",
        4_000,
        14_000,
        vec![],
    );
    bash.tool_call_id = "call-1".into();
    let mut agent_model = span(
        "t3:m1",
        SpanKind::Provider,
        "model #1",
        20_000,
        22_000,
        vec![],
    );
    agent_model.task_id = "t3".into();
    let mut agent_bash = span(
        "t3:b1",
        SpanKind::Tool,
        "bash git diff",
        22_000,
        40_000,
        vec![],
    );
    agent_bash.task_id = "t3".into();
    agent_bash.tool_call_id = "call-3".into();
    let mut agent = span(
        "t3",
        SpanKind::Subagent,
        "agent t3",
        20_000,
        41_000,
        vec![agent_model, agent_bash],
    );
    agent.task_id = "t3".into();
    let mut spawn = span(
        "s1",
        SpanKind::Tool,
        "agent_run",
        19_000,
        41_000,
        vec![agent],
    );
    spawn.tool_call_id = "call-2".into();
    let turn = span(
        "turn-1",
        SpanKind::Turn,
        "user",
        0,
        48_000,
        vec![model, bash, spawn],
    );
    Trace {
        session_id: "session".into(),
        turns: vec![turn],
    }
}
