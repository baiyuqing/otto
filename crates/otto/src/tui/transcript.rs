//! Transcript entries -> gutter-marked, pre-wrapped `ratatui` lines.
//!
//! Split out of [`super::render`] so the rules that decide what a turn looks
//! like are a pure function of (entries, details, width): no `App`, no frame,
//! no terminal. [`render_cache::RenderCache`] keeps the rows of unchanged
//! entries between frames. See [`super::gutter`] for why wrapping happens
//! here instead of in `Paragraph::wrap`.

use otto_core::model::{EffectCertainty, OperationDisposition};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::entries::{Entry, EntryKind};
use super::gutter::{self, BULLET_MARK, RESULT_MARK, SYSTEM_MARK};
use super::layout::{escape_plain_text, escape_single_line_text};
use super::markdown;
use crate::cli::repl::USER_MARK;

pub(crate) mod render_cache;

/// How much of a tool call's arguments, or of one result line, a folded call
/// shows. The cut is by character count rather than terminal width because
/// folding exists to keep a call short: the gutter would otherwise wrap a
/// long line back into the block of text the fold removed.
const TOOL_PREVIEW_LIMIT: usize = 64;

/// The band behind a prompt, which is what a reader scrolling back looks for
/// to find where a turn started.
///
/// An ANSI colour, like every other colour the transcript uses, so it follows
/// the terminal's own theme instead of fixing an RGB value that only suits
/// one: bright black is a step off the background in a dark theme and a grey
/// in a light one, and the prompt's own green stays legible on both.
pub(super) const PROMPT_BACKGROUND: Color = Color::DarkGray;

/// The whole transcript, one blank line between entries so a prompt, a reply,
/// and a tool call read as separate blocks instead of one run of text.
pub(crate) fn lines(entries: &[Entry], details: bool, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in entries {
        let rendered = entry_lines(entry, details, width);
        if rendered.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(rendered);
    }
    lines
}

/// Renders one entry under its gutter.
///
/// Every entry is one block under one marker: the marker starts it and an
/// aligned indent continues it, so a wrapped prompt and a multi-paragraph
/// reply each read as a single unit. A prompt is literal text the reader
/// typed and keeps its own line breaks; assistant and system text is
/// markdown; a tool call is a header plus its result.
pub(super) fn entry_lines(entry: &Entry, details: bool, width: usize) -> Vec<Line<'static>> {
    if let Some(request) = &entry.approval {
        return super::approval_display::lines(request, width);
    }
    if entry.kind == Some(EntryKind::LocalCommand) {
        let style = Style::default().add_modifier(Modifier::DIM);
        let mut text = vec![Line::from(escape_plain_text(&entry.raw))];
        if details {
            text.extend(
                escape_plain_text(&format!(
                    "Parameters: {}\n{}",
                    entry.tool_args, entry.tool_output
                ))
                .lines()
                .map(|line| Line::from(line.to_owned())),
            );
        }
        return gutter::block(&text, "›_", style, width);
    }
    if entry.kind == Some(EntryKind::Tool) {
        return tool_lines(entry, details, width);
    }
    if entry.raw.is_empty() {
        return Vec::new();
    }
    match entry.kind {
        Some(EntryKind::User) => {
            let style = Style::default()
                .fg(Color::Green)
                .bg(PROMPT_BACKGROUND)
                .add_modifier(Modifier::BOLD);
            let mut text: Vec<Line<'static>> = escape_plain_text(&entry.raw)
                .split('\n')
                .map(|line| Line::from(Span::styled(line.to_string(), style)))
                .collect();
            if !entry.input_label.is_empty() {
                text.push(Line::from(Span::styled(
                    escape_plain_text(&entry.input_label),
                    style.add_modifier(Modifier::DIM),
                )));
            }
            // Padded, so the band runs the width of the transcript rather
            // than stopping where the typed text happens to end.
            gutter::pad_rows(gutter::block(&text, USER_MARK, style, width), style, width)
        }
        Some(EntryKind::Reasoning) => {
            let dim = Style::default().add_modifier(Modifier::DIM);
            let mut text = markdown::render(&entry.raw).lines;
            for span in text.iter_mut().flat_map(|line| line.spans.iter_mut()) {
                span.style = span.style.add_modifier(Modifier::DIM);
            }
            gutter::block(&text, SYSTEM_MARK, dim, width)
        }
        kind => {
            let (mark, style) = match kind {
                Some(EntryKind::Error) => (BULLET_MARK, Style::default().fg(Color::Red)),
                Some(EntryKind::Compaction) | Some(EntryKind::System) => {
                    (SYSTEM_MARK, Style::default().add_modifier(Modifier::DIM))
                }
                _ => (BULLET_MARK, Style::default()),
            };
            gutter::block(&markdown::render(&entry.raw).lines, mark, style, width)
        }
    }
}

/// Renders one tool call: a bulleted header, then its result indented under
/// it. Folded (the default) shows a bounded action/target/status summary;
/// raw output is hidden, so a long `bash` output cannot push the
/// reply that follows it off the screen; `Ctrl+O` ([`super::app::App`]'s
/// details flag) shows the call id, the arguments, and the output in full.
fn tool_outcome_label(entry: &Entry) -> Option<&'static str> {
    match entry.effect_certainty? {
        EffectCertainty::Unknown => Some("Outcome unknown — check effects before retrying"),
        EffectCertainty::NotStarted => Some("Did not run"),
        EffectCertainty::KnownNoEffect => Some("Known no effect"),
        EffectCertainty::Completed => match entry.disposition? {
            OperationDisposition::Succeeded => Some("Completed"),
            OperationDisposition::Error => Some("Failed with a known result"),
            _ => None,
        },
    }
}

fn is_file_diff(entry: &Entry) -> bool {
    !entry.tool_error
        && matches!(entry.tool_name.as_str(), "edit" | "write")
        && entry
            .tool_output
            .lines()
            .next()
            .is_some_and(|line| line.starts_with("edited ") || line.starts_with("wrote "))
        && entry
            .tool_output
            .lines()
            .skip(1)
            .any(|line| line.starts_with("@@ "))
}

fn diff_line_style(line: &str, fallback: Style) -> Style {
    if line.starts_with("@@ ") {
        Style::default().fg(Color::Cyan)
    } else if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else {
        fallback
    }
}

/// Stable display abbreviation only; reducers always compare the full identity.
fn compact_invocation_id(id: &str) -> String {
    let hash = id.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    format!("{:012x}", hash & 0xffffffffffff)
}

fn tool_lines(entry: &Entry, details: bool, width: usize) -> Vec<Line<'static>> {
    let args = otto_core::wire::transcript::format_args_compact(
        otto_core::wire::events::tool_args_raw(&entry.tool_args).as_deref(),
    );
    let unknown = entry.effect_certainty == Some(EffectCertainty::Unknown);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let marker_style = if unknown {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if entry.tool_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Cyan)
    };
    let result_style = if unknown {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if entry.tool_error {
        Style::default().fg(Color::Red)
    } else {
        dim
    };

    let mut header = vec![Span::styled(
        entry.tool_name.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if details {
        header.push(Span::styled(
            format!(" ({}, provider: {})", entry.id, entry.tool_call_id),
            dim,
        ));
    }
    if !details {
        header.push(Span::styled(
            format!(" [{}]", compact_invocation_id(&entry.id)),
            dim,
        ));
    }
    let mut lines = gutter::block(&[Line::from(header)], BULLET_MARK, marker_style, width);

    let indent = gutter::continuation(RESULT_MARK);
    let push_result = |lines: &mut Vec<Line<'static>>, text: String, first: bool, style: Style| {
        let mark = if first { RESULT_MARK } else { indent.as_str() };
        lines.extend(gutter::block(
            &[Line::from(Span::styled(text, style))],
            mark,
            result_style,
            width,
        ));
    };

    if details {
        if entry.disposition == Some(OperationDisposition::Cancelled) {
            push_result(&mut lines, "cancelled".into(), false, result_style);
        }
        if let Some(elapsed) = entry.execution_elapsed(std::time::Instant::now()) {
            push_result(
                &mut lines,
                format!("execution {:.1}s", elapsed.as_secs_f64()),
                false,
                result_style,
            );
        }
        if !entry.tool_args.is_empty() {
            for line in escape_plain_text(&args).split('\n') {
                push_result(&mut lines, line.to_string(), false, result_style);
            }
        }
        if entry.tool_done {
            if let Some(label) = tool_outcome_label(entry) {
                push_result(&mut lines, label.to_string(), true, result_style);
            }
            let diff = is_file_diff(entry);
            for (index, line) in entry.tool_output.split('\n').enumerate() {
                push_result(
                    &mut lines,
                    escape_plain_text(line),
                    tool_outcome_label(entry).is_none() && index == 0,
                    if diff {
                        diff_line_style(line, result_style)
                    } else {
                        result_style
                    },
                );
            }
        }
        return lines;
    }

    let summary = compact_tool_summary(entry);
    // The header already names the tool; summary carries target/status/result.
    push_result(&mut lines, summary, true, result_style);
    lines
}

/// Deterministic presentation only: never invokes a model or alters tool data.
fn compact_tool_summary(entry: &Entry) -> String {
    let args = serde_json::from_str::<serde_json::Value>(&entry.tool_args).unwrap_or_default();
    // Only known target fields are eligible: no arbitrary args or shell text.
    let field = match entry.tool_name.as_str() {
        "read" | "write" | "edit" | "ls" | "find" | "grep" => Some("path"),
        "web_fetch" => Some("url"),
        "web_search" => Some("query"),
        _ => None,
    };
    let target = field
        .and_then(|key| args.get(key))
        .and_then(|v| v.as_str())
        .map(preview)
        .unwrap_or_default();
    let status = if !entry.tool_done {
        "running"
    } else if entry.disposition == Some(OperationDisposition::Cancelled) {
        "cancelled"
    } else if entry.effect_certainty == Some(EffectCertainty::Unknown) {
        "effect unknown"
    } else if entry.tool_error {
        "error"
    } else {
        "completed"
    };
    let result = if !entry.tool_done {
        String::new()
    } else if entry.tool_error {
        let error = preview(
            entry
                .tool_output
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("tool failed; open details"),
        );
        match tool_outcome_label(entry) {
            Some(label) => format!("{} · {error}", preview(label)),
            None => error,
        }
    } else if let Some(label) = tool_outcome_label(entry) {
        preview(label)
    } else if entry.tool_output.trim().is_empty() {
        "no output".into()
    } else {
        format!("{} lines", entry.tool_output.lines().count())
    };
    let elapsed = entry
        .execution_elapsed(std::time::Instant::now())
        .map(|duration| format!("{:.1}s", duration.as_secs_f64()))
        .unwrap_or_default();
    [target, status.into(), elapsed, result]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The first line of `text`, control-escaped and cut to
/// [`TOOL_PREVIEW_LIMIT`] characters.
fn preview(text: &str) -> String {
    let line = escape_single_line_text(text.lines().next().unwrap_or_default());
    let mut cut: String = line.chars().take(TOOL_PREVIEW_LIMIT).collect();
    if line.chars().count() > TOOL_PREVIEW_LIMIT {
        cut.push('\u{2026}');
    }
    cut
}

#[cfg(test)]
mod tests;
