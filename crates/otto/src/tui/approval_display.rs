//! TUI-only approval projection. Stored messages and approval decisions are untouched.
use otto_core::approval::{ApplicationState, Request, State};
use ratatui::{style::Style, text::Line};
use serde_json::Value;

use super::{entries::Entry, gutter, layout::escape_plain_text};

/// An approval has one display identity, even when its decision/application changes.
pub(super) fn upsert(entries: &mut Vec<Entry>, request: &Request) {
    if let Some(entry) = entries.iter_mut().find(|entry| {
        entry
            .approval
            .as_ref()
            .is_some_and(|previous| previous.id == request.id)
    }) {
        if entry
            .approval
            .as_ref()
            .is_some_and(|previous| previous.revision > request.revision)
        {
            return;
        }
        entry.approval = Some(request.clone());
    } else {
        entries.push(Entry {
            id: format!("approval:{}", request.id),
            kind: Some(super::entries::EntryKind::System),
            approval: Some(request.clone()),
            ..Entry::default()
        });
    }
}

/// Keep live projections on same-session reload, never across a session switch.
/// History contains messages, not approval events: this is not persistent replay.
pub(super) fn retain(entries: &mut Vec<Entry>, previous: &[Entry]) {
    // Match non-approval neighbors in transcript order. Live text rows have
    // temporary IDs; their content is a fallback until history supplies an ID.
    // Operation identity survives tool completion, retries and output changes.
    let mut cursor = 0;
    let anchors: Vec<_> = previous
        .iter()
        .map(|entry| {
            if entry.approval.is_some() {
                return None;
            }
            let remaining = &entries[cursor..];
            let matched = remaining
                .iter()
                .position(|candidate| {
                    candidate.kind == entry.kind
                        && ((entry.operation_id.is_some()
                            && candidate.operation_id == entry.operation_id)
                            || (!entry.id.is_empty()
                                && candidate.id == entry.id
                                && candidate.raw == entry.raw))
                })
                .or_else(|| {
                    remaining.iter().position(|candidate| {
                        candidate.kind == entry.kind
                            && candidate.approval.is_none()
                            && !entry.raw.is_empty()
                            && candidate.raw == entry.raw
                    })
                })?;
            cursor += matched + 1;
            Some(cursor - 1)
        })
        .collect();
    let mut retained = Vec::new();
    let mut predecessor = None;
    for (index, entry) in previous.iter().enumerate() {
        if let Some(anchor) = anchors[index] {
            predecessor = Some(anchor);
        }
        if entry.approval.is_some() {
            let position = predecessor.map(|anchor| anchor + 1).unwrap_or_else(|| {
                anchors[index + 1..]
                    .iter()
                    .flatten()
                    .next()
                    .copied()
                    .unwrap_or(entries.len())
            });
            retained.push((position, entry.clone()));
        }
    }
    // Offsets account for earlier notices sharing the same neighboring row.
    for (offset, (position, entry)) in retained.into_iter().enumerate() {
        entries.insert(position + offset, entry);
    }
}

pub(super) fn lines(request: &Request, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    field(&mut rows, "Approval", &request.title);
    field(&mut rows, "ID", &request.id);
    let state = match request.state {
        State::Pending if !request.is_human_pending() => "reviewing",
        State::Pending => "pending",
        State::Approved => "approved",
        State::Rejected => "rejected",
        State::Withdrawn => "withdrawn",
        State::Expired => "expired",
        State::Superseded => "superseded",
    };
    field(&mut rows, "State", state);
    field(
        &mut rows,
        "Application",
        match request.application {
            ApplicationState::NotRequired => "not required",
            ApplicationState::NotStarted => "not started",
            ApplicationState::Applying => "applying",
            ApplicationState::Applied => "applied",
            ApplicationState::Failed => "failed",
            ApplicationState::Unknown => "unknown",
        },
    );
    field(&mut rows, "Reason", &request.reason);
    if let Some(decision) = &request.decision {
        field(&mut rows, "Decision source", &decision.transport);
        field(&mut rows, "Feedback", &decision.feedback);
    }
    field(&mut rows, "Application error", &request.application_error);
    if !request.subject.details.is_empty() {
        rows.push(Line::from("Request details:"));
        // Tool approval specs serialize JSON objects/arrays into Subject.details.
        // Decode at that boundary only. Plain strings (including shell/regex
        // backslashes and JSON string literals) must never be unescaped heuristically.
        match serde_json::from_str::<Value>(&request.subject.details) {
            Ok(value @ (Value::Object(_) | Value::Array(_))) => detail(&mut rows, &value, 1),
            _ => literal(&mut rows, &request.subject.details, 1),
        }
    }
    gutter::block(&rows, gutter::SYSTEM_MARK, Style::default(), width)
}

fn field(rows: &mut Vec<Line<'static>>, label: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    rows.push(Line::from(format!("{label}:")));
    literal(rows, value, 1);
}

fn detail(rows: &mut Vec<Line<'static>>, value: &Value, depth: usize) {
    match value {
        Value::Object(fields) if !fields.is_empty() => {
            for (key, value) in fields {
                if value.as_str() == Some("") || value.is_null() {
                    continue;
                }
                literal(rows, &format!("{key}:"), depth);
                detail(rows, value, depth + 1);
            }
        }
        Value::Array(values) if !values.is_empty() => {
            for (index, value) in values.iter().enumerate() {
                literal(rows, &format!("[{index}]:"), depth);
                detail(rows, value, depth + 1);
            }
        }
        Value::String(text) => literal(rows, text, depth),
        _ => literal(rows, &value.to_string(), depth),
    }
}

fn literal(rows: &mut Vec<Line<'static>>, text: &str, depth: usize) {
    // Normalize real layout controls, not the two characters backslash+n/t/r.
    // Expand tabs ourselves: terminal spans must not rely on terminal tab stops.
    let text = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ");
    let safe = escape_plain_text(&text);
    let indent = "  ".repeat(depth.min(12));
    rows.extend(
        safe.split('\n')
            .map(|line| Line::from(format!("{indent}{line}"))),
    );
}
