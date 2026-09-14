//! Turns [`super::app::App`] state into ratatui widgets. Port of the `View`
//! half of `internal/tui/model.go` plus `internal/tui/{resume,archive,
//! profile,session,sandbox}.go`'s picker/overlay rendering.
//!
//! ponytail: Go gives `/resume`, `/archive`, `/model`, `/session`,
//! `/sandbox`, `/tasks`, and `/task` each their own full-screen Bubble Tea
//! view. Only `/resume`, `/archive`, and `/model`'s profile choice are
//! genuinely a *selection*; the rest are a fixed block of text with no
//! interaction, so those render as ordinary transcript entries (see
//! [`super::app`]'s module doc) drawn by the same transcript paragraph as
//! everything else. `/resume`, `/archive`, and `/model` share one
//! [`super::app::Picker`] overlay instead of three near-identical Bubble Tea
//! screens. Upgrade path: give any of these a dedicated layout if a user
//! reports the shared one as confusing.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::UnicodeWidthChar;

use super::app::App;
use super::commands::{SLASH_COMMANDS, SlashCommand};
use super::entries::{Entry, EntryKind};
use super::layout::{
    INPUT_BOX_THRESHOLD, MIN_TERMINAL_HEIGHT, MIN_TERMINAL_WIDTH, escape_plain_text,
    escape_single_line_text, footer_workspace, format_context_percentage, format_token_count,
};
use super::markdown;

/// Draws one frame. Port of `Model.View`.
pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width < MIN_TERMINAL_WIDTH || area.height < MIN_TERMINAL_HEIGHT {
        let message = format!(
            "Terminal too small: resize to at least {MIN_TERMINAL_WIDTH}x{MIN_TERMINAL_HEIGHT}."
        );
        frame.render_widget(Paragraph::new(message).alignment(Alignment::Center), area);
        return;
    }

    let composer_height = composer_height(app, area.width);
    let suggestions = app.suggestions();
    // Go's `calculateLayout` clamps the panel the same way: it may take every
    // row the composer and footer leave except one, which the transcript keeps.
    let suggestion_height =
        (suggestions.len() as u16).min(area.height.saturating_sub(composer_height + 2));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(suggestion_height),
            Constraint::Length(composer_height),
        ])
        .split(area);

    draw_transcript(frame, app, chunks[0]);
    draw_footer(frame, app, chunks[1]);
    draw_suggestions(frame, app, &suggestions, chunks[2]);
    draw_composer(frame, app, chunks[3]);

    if app.show_help {
        draw_help(frame, area);
    } else if let Some(picker) = &app.picker {
        draw_picker(frame, area, picker);
    }
}

/// Port of the editor-height growth Go's `handleCtrlC`/layout code reference
/// via `previousEditorHeight`: a one-line composer grows to fit wrapped
/// input up to [`INPUT_BOX_THRESHOLD`] lines before it stops growing and
/// scrolls instead.
fn composer_height(app: &App, width: u16) -> u16 {
    // `width - 2` is the box's inner width, so this sizes the box from
    // exactly the rows [`draw_composer`] will put in it.
    let (lines, _, _) = composer_lines(&app.input, app.cursor, width.saturating_sub(2));
    (lines.len() as u16).clamp(1, INPUT_BOX_THRESHOLD) + 2
}

fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = transcript_lines(&app.entries, app.show_details);
    if let Some(elapsed) = app.thinking() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(thinking_line(elapsed));
    }

    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total_lines = paragraph.line_count(area.width) as u16;
    let bottom = total_lines.saturating_sub(area.height);
    // The scroll keys need the bottom this layout produced to turn
    // "following" into an absolute offset; nothing outside the renderer
    // knows how the entries wrap at this width.
    app.max_scroll.set(bottom);
    let scroll = app.scroll.map_or(bottom, |top| top.min(bottom));
    frame.render_widget(paragraph.scroll((scroll, 0)), area);
}

/// The whole transcript, one blank line between entries so a prompt, a
/// reply, and a tool block read as separate blocks instead of one run of
/// text.
fn transcript_lines(entries: &[Entry], details: bool) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in entries {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(entry_lines(entry, details));
    }
    lines
}

/// Spinner frames, in order. Same braille set Go's Bubble Tea spinner uses.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long one [`SPINNER_FRAMES`] frame is held. [`super::drive_turn`]
/// redraws on this interval for as long as a turn runs.
pub(super) const SPINNER_FRAME: Duration = Duration::from_millis(100);

/// The line shown under the transcript while a turn is in flight. A turn
/// streams nothing between the prompt and the model's first token, so this
/// is the only thing that distinguishes waiting from a hung terminal.
fn thinking_line(elapsed: Duration) -> Line<'static> {
    let frame = elapsed.as_millis() / SPINNER_FRAME.as_millis();
    Line::styled(
        format!(
            "{} Thinking… {}s",
            SPINNER_FRAMES[frame as usize % SPINNER_FRAMES.len()],
            elapsed.as_secs()
        ),
        Style::default().fg(Color::Magenta),
    )
}

/// What a user entry's lines are prefixed with. The prompt is the one piece
/// of transcript text the person reading the screen wrote themselves, so it
/// carries a marker the model's output never has.
const USER_PREFIX: &str = "> ";

/// Renders one transcript entry. Assistant/system/compaction/error text is
/// markdown; tool call/result bodies are plain, pre-escaped text (Go never
/// runs tool output through the markdown renderer either), and so is a user
/// prompt, which is typed as literal text rather than authored as markdown.
///
/// ponytail: the prefix is part of the line's text, so `Paragraph::wrap`
/// puts no marker on the continuation rows of a prompt wider than the
/// terminal. Upgrade path: wrap user text here (as `composer_lines` already
/// does for the composer) and prefix every row if that is reported as
/// confusing.
fn entry_lines(entry: &Entry, details: bool) -> Vec<Line<'static>> {
    match entry.kind {
        Some(EntryKind::User) => escape_plain_text(&entry.raw)
            .lines()
            .map(|line| {
                Line::styled(
                    format!("{USER_PREFIX}{line}"),
                    Style::default().fg(Color::Green),
                )
            })
            .collect(),
        Some(EntryKind::Tool) => tool_lines(entry, details),
        _ => markdown::render(&entry.raw).lines,
    }
}

/// How much of a tool call's arguments or result a folded line shows. The
/// cut is by character count rather than terminal width because folding
/// exists to keep a call to one or two rows: a `Paragraph` wraps anything
/// longer back into the block of text the fold removed.
const TOOL_PREVIEW_LIMIT: usize = 64;

/// Renders one tool call. Folded (the default) it is the call plus a
/// one-line result, so a long `bash` output cannot push the reply that
/// follows it off the screen; `Ctrl+O` ([`App::show_details`]) shows the
/// call id, the arguments, and the output in full.
fn tool_lines(entry: &Entry, details: bool) -> Vec<Line<'static>> {
    let output_style = if entry.tool_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };

    if details {
        let mut lines = vec![Line::styled(
            format!("[tool] {} ({})", entry.tool_name, entry.tool_call_id),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if !entry.tool_args.is_empty() {
            lines.extend(plain_lines(&entry.tool_args, Style::default()));
        }
        if entry.tool_done {
            lines.extend(plain_lines(&entry.tool_output, output_style));
        }
        return lines;
    }

    let mut header = vec![Span::styled(
        format!("[tool] {}", entry.tool_name),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if !entry.tool_args.is_empty() {
        header.push(Span::styled(
            format!(" {}", preview(&entry.tool_args)),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    let mut lines = vec![Line::from(header)];
    if entry.tool_done {
        let style = if entry.tool_error {
            output_style
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        lines.push(Line::styled(tool_summary(entry), style));
    }
    lines
}

/// The folded result line: the first line of the output and how many more
/// there are, or that there was none. A call still running has no result
/// and gets no line at all.
fn tool_summary(entry: &Entry) -> String {
    if entry.tool_output.trim().is_empty() {
        return "  \u{2192} (no output)".to_string();
    }
    let mut summary = format!("  \u{2192} {}", preview(&entry.tool_output));
    match entry.tool_output.lines().count().saturating_sub(1) {
        0 => {}
        1 => summary.push_str(" (+1 line)"),
        more => summary.push_str(&format!(" (+{more} lines)")),
    }
    summary
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

/// Pre-escaped text as one transcript line per line of `text`. A `Line`
/// holding an embedded newline renders as one row, so splitting here is
/// what keeps expanded tool output readable.
fn plain_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    escape_plain_text(text)
        .split('\n')
        .map(|line| Line::styled(line.to_string(), style))
        .collect()
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        Paragraph::new(footer_text(app)).style(Style::default().add_modifier(Modifier::DIM)),
        area,
    );
}

fn footer_text(app: &App) -> String {
    let profile = escape_single_line_text(&app.info.profile);
    let model = escape_single_line_text(&app.info.model);
    let profile_model = match (profile.is_empty(), model.is_empty()) {
        (true, true) => "unknown/unknown".to_string(),
        _ => format!("{profile}/{model}").trim_matches('/').to_string(),
    };
    let mut text = format!(
        "{profile_model} | {} | {} | tokens {}/{}",
        app.info.sandbox.badge(),
        escape_single_line_text(&footer_workspace(&app.info.workspace)),
        format_token_count(app.usage.input_tokens),
        format_token_count(app.usage.output_tokens)
    );
    if app.usage.cached_input_tokens > 0 {
        text.push_str(&format!(
            " (cached {})",
            format_token_count(app.usage.cached_input_tokens)
        ));
    }
    if app.info.context_input_tokens_pending {
        text.push_str(" | ctx ?%");
    } else if app.info.context_input_tokens_present && app.info.context_window > 0 {
        text.push_str(&format!(
            " | ctx {}",
            format_context_percentage(app.info.context_input_tokens, app.info.context_window)
        ));
    }
    if !app.info.session_id.is_empty() {
        text.push_str(" | ");
        text.push_str(&escape_single_line_text(&app.info.session_id));
    }
    match &app.status {
        Some(status) => format!("{} | {text}", escape_single_line_text(status)),
        None => text,
    }
}

/// The command list drawn directly above the composer while the value being
/// typed is a command prefix, with [`super::app::App::suggestion`]'s row
/// highlighted. Port of `renderCommandSuggestions`.
///
/// A [`List`] rather than a [`Paragraph`] so that ratatui's own
/// [`ListState`] scrolls the selected row into view when the match list is
/// longer than the rows [`draw`] could give the panel.
fn draw_suggestions(frame: &mut Frame, app: &App, suggestions: &[SlashCommand], area: Rect) {
    if area.height == 0 || suggestions.is_empty() {
        return;
    }
    let items: Vec<ListItem> = suggestions
        .iter()
        .map(|command| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<12}", command.name),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    command.description,
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]))
        })
        .collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state =
        ListState::default().with_selected(Some(app.suggestion.min(suggestions.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Lays the composer value out into the rows the box will show, and reports
/// the row/column the caret sits at.
///
/// The composer breaks lines itself rather than handing the value to
/// `Paragraph::wrap`, because the caret has to land on the same grid the
/// text does: `Wrap` breaks on word boundaries, which no arithmetic over
/// the value's prefix can reproduce. Breaking is by display column (a CJK
/// character takes two, a combining mark none) at an explicit `\n` or when
/// the next character would not fit, so every returned line is at most
/// `width` columns wide and `Paragraph` never re-wraps it.
///
/// The caret occupies one column, so it wraps to the next row when it would
/// not fit either; that is also what makes the box grow a row once the value
/// exactly fills the last one.
fn composer_lines(input: &[char], cursor: usize, width: u16) -> (Vec<String>, u16, u16) {
    let width = width.max(1) as usize;
    let cursor = cursor.min(input.len());
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut column = 0usize;
    let (mut caret_row, mut caret_column) = (0u16, 0u16);

    for index in 0..=input.len() {
        if index == cursor {
            if column + 1 > width {
                lines.push(std::mem::take(&mut line));
                column = 0;
            }
            caret_row = lines.len() as u16;
            caret_column = column as u16;
        }
        let Some(&ch) = input.get(index) else { break };
        if ch == '\n' {
            lines.push(std::mem::take(&mut line));
            column = 0;
            continue;
        }
        let ch_width = ch.width().unwrap_or(0);
        if column + ch_width > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            column = 0;
        }
        line.push(ch);
        column += ch_width;
    }
    lines.push(line);

    (lines, caret_row, caret_column)
}

fn draw_composer(frame: &mut Frame, app: &App, area: Rect) {
    let title = if app.busy() {
        "Working (Esc to cancel)"
    } else {
        "Otto"
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (lines, caret_row, caret_column) = composer_lines(&app.input, app.cursor, inner.width);
    // Once the value is taller than the box stopped growing at
    // (`INPUT_BOX_THRESHOLD`), the box scrolls to keep the caret's row on
    // screen instead of pinning the first rows and losing what is being typed.
    let scroll = caret_row.saturating_sub(inner.height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(lines.into_iter().map(Line::raw).collect::<Vec<_>>()).scroll((scroll, 0)),
        inner,
    );

    if !app.busy() {
        frame.set_cursor_position((inner.x + caret_column, inner.y + caret_row - scroll));
    }
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(70, 70, area);
    let items: Vec<ListItem> = SLASH_COMMANDS
        .iter()
        .map(|command| ListItem::new(format!("{:<12} {}", command.name, command.description)))
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Help (Esc to close)"),
    );
    frame.render_widget(list, popup);
}

fn draw_picker(frame: &mut Frame, area: Rect, picker: &super::app::Picker) {
    let popup = centered_rect(80, 70, area);
    let items: Vec<ListItem> = picker
        .rows
        .iter()
        .map(|row| ListItem::new(row.label.clone()))
        .collect();
    // Stateful so that ratatui windows the list around the selection, the way
    // Go's `resumeVisibleRange` does: a picker lists up to
    // `PICKER_LIST_LIMIT` sessions, more than the popup can show.
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(picker.kind.title()),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

/// A centered `percent_x` by `percent_y` rectangle within `area`. Standard
/// ratatui popup-centering helper (from the project's own examples), not a
/// Go port: `internal/tui`'s overlays are Bubble Tea sub-models with their
/// own full-screen layout, which ratatui has no equivalent for.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// Port of `internal/tui/responsive_test.go` and `clarity_test.go`'s
/// terminal-size/wrapping cases, against ratatui's own render loop rather
/// than Go's `View()` string plumbing.
///
/// Go's `View()` returns a raw string with no automatic clipping: every
/// hand-rolled renderer (`renderToolBlock`, `renderUserBlock`,
/// `emptyTranscriptHint`, `renderCompactionBlock`, `renderFooter`'s
/// width-based field-dropping, `resumeVisibleRange`'s manual windowing, ...)
/// has to clip and wrap itself via `lipgloss.MaxWidth`/`MaxHeight`,
/// `wrapAndClip`, and `fitToBounds`, and `responsive_test.go`/
/// `clarity_test.go` exist to guard those hand-rolled functions against
/// overflow bugs. Ratatui instead renders each widget into a `Rect`/`Buffer`
/// that the framework itself always clips to (see this module's own doc
/// comment above [`draw`], and `layout.rs`'s: "ratatui's own `Paragraph::wrap`
/// and `Layout` constraints already do line-wrapping and area-fitting").
/// None of those Go functions exist here to test.
///
/// What *does* port over is the one static guard both sides share
/// ([`MIN_TERMINAL_WIDTH`]/[`MIN_TERMINAL_HEIGHT`], below) and a no-panic
/// guarantee at extreme sizes, which is what's left here.
///
/// Skipped as Bubble Tea plumbing (Go source, not ported):
/// - `TestToolSummaryUsesLeadingStatesAndDecodedBashCommand`,
///   `TestToolSummaryWithoutPreviewPadsToWidth`,
///   `TestToolArgumentPreviewExtractsHumanReadableSummary`,
///   `TestExpandedToolWrapsIndentedDetailsWithoutDroppingTail`,
///   `TestIndentedToolSummaryFitsTerminalAnd120CellLimit`,
///   `TestAssistantTurnShowsOttoOnceAcrossTool`,
///   `TestAssistantTitleJoinsFirstTextWithoutBlankLine`,
///   `TestEmptyTranscriptHintIncludesLogo`,
///   `TestUserBlockFillsBandAndKeepsRailAcrossThemes`,
///   `TestAssistantProseMaxWidthButCompactionUsesAvailableWidth` — all
///   exercise Go's hand-rolled string-building renderers listed above, which
///   ratatui's widget model replaces wholesale.
/// - `TestHelpOverlayAtMinimumTerminalShowsEveryControlWithinBounds` — Go's
///   help overlay hard-codes a keybinding list; [`draw_help`] here is a
///   generic list built from [`SLASH_COMMANDS`] with no keybinding text at
///   all (this frontend's overlays are deliberately simpler, see
///   `super::app`'s module doc "Not ported" note), so there is no matching
///   content to assert on without adding production content nobody asked
///   for.
/// - `TestLongSessionOverlayAndFooterStayWithinBounds` — ratatui clips the
///   fixed footer to its `Rect`; the session overlay itself is not ported.
/// - `TestCompactionResponsiveCollapsedCheckpointStaysWithinBounds` — tests
///   `renderCompactionBlock`; compaction entries here fall through the
///   generic [`markdown::render`] path in [`entry_lines`], which has no
///   compaction-specific layout to test.
/// - `TestResumePickerResizeClampsSelectionAndRestoresTranscriptOnClose` —
///   tests Go's manual `resumeVisibleRange` windowing; [`draw_picker`]
///   renders into a ratatui `List`, which scrolls its own selection into
///   view with no manual clamping logic to port.
#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use otto_core::agent::Event;
    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend};

    use super::*;
    use crate::cli::testutil;
    use crate::tui::app::{Picker, PickerKind, PickerRow};

    async fn app_fixture() -> (tempfile::TempDir, tempfile::TempDir, App) {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let app = App::new(&controller);
        (workspace, sessions, app)
    }

    fn rendered(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        format!("{}", terminal.backend())
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The frame index and the elapsed count both come from one `Duration`,
    /// so they cannot disagree about how long the turn has been running.
    #[test]
    fn the_thinking_frame_advances_with_elapsed_time() {
        let at = |ms| line_text(&thinking_line(Duration::from_millis(ms)));

        assert_eq!(at(0), "⠋ Thinking… 0s");
        assert_eq!(at(100), "⠙ Thinking… 0s");
        assert_eq!(at(1_000), "⠋ Thinking… 1s");
    }

    /// A turn streams nothing until the model's first token, so without this
    /// line the transcript sits unchanged and the terminal looks hung.
    #[tokio::test]
    async fn a_running_turn_shows_the_thinking_line_under_the_transcript() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.entries.push(Entry {
            kind: Some(EntryKind::User),
            raw: "hello otto".to_string(),
            ..Default::default()
        });

        let idle = rendered(&app, 40, 10);
        app.start_turn();
        let busy = rendered(&app, 40, 10);
        app.end_turn();

        assert!(!idle.contains("Thinking"), "idle transcript:\n{idle}");
        assert!(busy.contains("Thinking"), "running transcript:\n{busy}");
        assert!(!rendered(&app, 40, 10).contains("Thinking"));
    }

    /// A user entry and the reply that follows it rendered as adjacent,
    /// identically styled lines, so there was nothing on screen to tell the
    /// prompt from the model's answer.
    #[test]
    fn a_user_entry_is_marked_and_separated_from_the_entry_after_it() {
        let entries = vec![
            crate::tui::entries::Entry {
                kind: Some(EntryKind::User),
                raw: "hello otto".to_string(),
                ..Default::default()
            },
            crate::tui::entries::Entry {
                kind: Some(EntryKind::Assistant),
                raw: "the reply".to_string(),
                ..Default::default()
            },
        ];

        let lines: Vec<String> = transcript_lines(&entries, false)
            .iter()
            .map(line_text)
            .collect();

        assert_eq!(lines, vec!["> hello otto", "", "the reply"]);
    }

    /// Every line of a multi-line prompt carries the marker, so a pasted
    /// block cannot be read as part of the reply.
    #[test]
    fn every_line_of_a_multi_line_prompt_is_marked() {
        let entries = vec![crate::tui::entries::Entry {
            kind: Some(EntryKind::User),
            raw: "first\nsecond".to_string(),
            ..Default::default()
        }];

        let lines: Vec<String> = transcript_lines(&entries, false)
            .iter()
            .map(line_text)
            .collect();

        assert_eq!(lines, vec!["> first", "> second"]);
    }

    fn tool_entry() -> Entry {
        Entry {
            kind: Some(EntryKind::Tool),
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            tool_args: r#"{"command":"ls -la"}"#.to_string(),
            tool_output: "total 12\na.txt\nb.txt".to_string(),
            tool_done: true,
            ..Default::default()
        }
    }

    /// The reported bad experience: a finished tool call printed its whole
    /// output into the transcript, so one `bash` call pushed the reply that
    /// followed it off the screen. Folded it is the call and a one-line
    /// result; `Ctrl+O` is what shows the call id, the arguments, and the
    /// output in full.
    #[test]
    fn a_tool_call_folds_to_a_summary_until_details_are_shown() {
        let entry = tool_entry();

        let folded: Vec<String> = entry_lines(&entry, false).iter().map(line_text).collect();
        assert_eq!(
            folded,
            vec![
                r#"[tool] bash {"command":"ls -la"}"#,
                "  \u{2192} total 12 (+2 lines)",
            ]
        );

        let expanded: Vec<String> = entry_lines(&entry, true).iter().map(line_text).collect();
        assert_eq!(
            expanded,
            vec![
                "[tool] bash (call-1)",
                r#"{"command":"ls -la"}"#,
                "total 12",
                "a.txt",
                "b.txt",
            ]
        );
    }

    /// Each folded line stays one line: neither a long argument blob nor a
    /// long first output line may wrap back into the wall of text folding
    /// exists to avoid.
    #[test]
    fn a_folded_tool_summary_cuts_long_arguments_and_output() {
        let entry = Entry {
            tool_args: format!(r#"{{"command":"{}"}}"#, "x".repeat(200)),
            tool_output: format!("{}\ntail", "y".repeat(200)),
            ..tool_entry()
        };

        let folded: Vec<String> = entry_lines(&entry, false).iter().map(line_text).collect();

        let header_prefix = "[tool] bash ".chars().count();
        assert!(
            folded[0].chars().count() <= header_prefix + TOOL_PREVIEW_LIMIT + 1,
            "{:?}",
            folded[0]
        );
        assert!(folded[0].ends_with('\u{2026}'), "{:?}", folded[0]);
        assert!(folded[1].ends_with("\u{2026} (+1 line)"), "{:?}", folded[1]);
    }

    /// A call still running has no result to summarize; a finished one with
    /// no output says so rather than leaving a bare header, and a failure
    /// carries its text in red.
    #[test]
    fn a_folded_tool_call_shows_its_state_while_running_and_on_failure() {
        let running = Entry {
            tool_done: false,
            ..tool_entry()
        };
        assert_eq!(entry_lines(&running, false).len(), 1);

        let empty = Entry {
            tool_output: String::new(),
            ..tool_entry()
        };
        assert_eq!(
            line_text(&entry_lines(&empty, false)[1]),
            "  \u{2192} (no output)"
        );

        let failed = Entry {
            tool_output: "boom".to_string(),
            tool_error: true,
            ..tool_entry()
        };
        let lines = entry_lines(&failed, false);
        assert_eq!(line_text(&lines[1]), "  \u{2192} boom");
        assert_eq!(lines[1].style.fg, Some(Color::Red));
    }

    #[tokio::test]
    async fn picker_clears_the_transcript_beneath_it() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.push_system(
            (0..40)
                .map(|_| "X".repeat(100))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        app.picker = Some(Picker {
            kind: PickerKind::Resume,
            rows: vec![PickerRow {
                label: "session".to_string(),
                value: "path".to_string(),
            }],
            selected: 0,
        });

        let width = 100;
        let height = 30;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
        let popup = centered_rect(80, 70, Rect::new(0, 0, width, height));

        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((popup.x + 1, popup.y + 2))
                .expect("popup cell")
                .symbol(),
            " "
        );
    }

    /// Port of the guard half of Go's `calculateLayout`/`smallTerminalView`:
    /// below the static minimum on either axis, the frame is just the
    /// resize message (Go's dynamic second guard, triggered when a
    /// computed transcript height is `<= 0` even above the static minimum,
    /// has no Rust equivalent since this layout is purely static — see
    /// [`composer_height`]).
    #[tokio::test]
    async fn narrower_or_shorter_than_the_minimum_shows_the_resize_message() {
        let (_workspace, _sessions, app) = app_fixture().await;

        let narrow = rendered(&app, MIN_TERMINAL_WIDTH - 1, MIN_TERMINAL_HEIGHT);
        let short = rendered(&app, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT - 1);

        assert!(narrow.contains("Terminal too small"));
        assert!(short.contains("Terminal too small"));
    }

    #[tokio::test]
    async fn exactly_the_minimum_size_shows_the_normal_layout() {
        let (_workspace, _sessions, app) = app_fixture().await;

        let content = rendered(&app, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT);

        assert!(!content.contains("Terminal too small"));
    }

    #[tokio::test]
    async fn footer_shows_runtime_and_session_status() {
        let (_workspace, _sessions, app) = app_fixture().await;
        let session_id = app.info.session_id.clone();

        let content = rendered(&app, 160, MIN_TERMINAL_HEIGHT);

        assert!(content.contains("alpha/gpt-alpha"), "{content}");
        assert!(content.contains("no-bash"), "{content}");
        assert!(content.contains("tokens 0/0"), "{content}");
        assert!(content.contains(&session_id), "{content}");

        let narrow = rendered(&app, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT);
        assert!(narrow.contains("alpha/gpt-alpha"), "{narrow}");
        assert!(narrow.contains("no-bash"), "{narrow}");
    }

    /// No-panic smoke test at Go's exact
    /// `TestVerySmallTerminalViewsStayWithinBounds` sizes. Go asserts every
    /// rendered line stays within bounds; ratatui's `Buffer` makes that
    /// structurally true (see this test module's doc comment above), so
    /// what is left to check is that drawing at these sizes does not panic.
    #[tokio::test]
    async fn draw_does_not_panic_at_go_s_extreme_terminal_sizes() {
        let (_workspace, _sessions, app) = app_fixture().await;

        for (width, height) in [(1u16, 1u16), (2, 1), (10, 3), (39, 7)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| draw(frame, &app))
                .expect("draw at extreme size");
        }
    }

    /// Diluted port of `TestUserBlockWrapsWideCharactersWithinAvailableWidth`
    /// and `TestIndentedToolSummaryFitsTerminalAnd120CellLimit` (its intent,
    /// not Go's cell-accounting): confirms Otto's own transcript
    /// text — a run of wide (CJK/emoji) characters and a long unbroken
    /// ASCII token with no wrap points — feeds into `Paragraph::wrap`
    /// without panicking at a narrow width. This does not re-test ratatui's
    /// own wrapping algorithm, only that Otto's text reaches it intact.
    #[tokio::test]
    async fn wide_characters_and_unbroken_tokens_wrap_without_panicking() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.push_system("测试测试测试测试测试测试测试测试测试测试😀😀😀😀😀😀😀😀😀😀😀😀");
        app.push_system("x".repeat(200));

        let backend = TestBackend::new(MIN_TERMINAL_WIDTH, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw with wide/unbroken content");
    }

    #[tokio::test]
    async fn manual_scroll_moves_up_from_the_bottom() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.push_system(
            (0..20)
                .map(|line| format!("line {line:02}"))
                .collect::<Vec<_>>()
                .join("\n\n"),
        );

        assert!(rendered(&app, MIN_TERMINAL_WIDTH, 12).contains("line 19"));
        app.scroll = Some(3);
        let scrolled = rendered(&app, MIN_TERMINAL_WIDTH, 12);
        assert!(!scrolled.contains("line 19"), "{scrolled}");
    }

    /// A manual scroll is an absolute top offset, so streamed output
    /// appended below it leaves the rows being read exactly where they are
    /// instead of pushing them off the top.
    #[tokio::test]
    async fn a_scrolled_view_stays_put_while_output_streams_in() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.push_system(
            (0..20)
                .map(|line| format!("line {line:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        // Lays out `max_scroll`, which the first scroll key reads.
        let _ = rendered(&app, MIN_TERMINAL_WIDTH, 12);
        app.handle_scroll_key(&KeyCode::PageUp.into());
        let before = rendered(&app, MIN_TERMINAL_WIDTH, 12);
        assert!(before.contains("line 05"), "{before}");

        for index in 0..10 {
            app.apply_event(Event::TextDelta {
                text: format!("streamed {index}\n"),
            });
        }

        let after = rendered(&app, MIN_TERMINAL_WIDTH, 12);
        assert!(after.contains("line 05"), "{after}");
        assert!(!after.contains("streamed 9"), "{after}");
    }

    /// Port of the view half of Go's `TestCommandSuggestionsMatchPrefix` in
    /// `internal/tui/completion_test.go`: a `/` prefix lists the matching
    /// commands with their descriptions and leaves the others out.
    #[tokio::test]
    async fn a_slash_prefix_lists_matching_commands_with_descriptions() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.input = "/s".chars().collect();
        app.cursor = app.input.len();

        let content = rendered(&app, 100, 20);

        assert!(content.contains("show session details"), "{content}");
        assert!(content.contains("/sandbox"), "{content}");
        assert!(!content.contains("show help"), "{content}");
    }

    /// Every row holding at least one reversed-video cell, as text: the rows
    /// a list is marking as selected.
    fn highlighted_rows(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer();
        (0..height)
            .filter_map(|y| {
                let row: Vec<&ratatui::buffer::Cell> =
                    (0..width).filter_map(|x| buffer.cell((x, y))).collect();
                row.iter()
                    .any(|cell| cell.style().add_modifier.contains(Modifier::REVERSED))
                    .then(|| row.iter().map(|cell| cell.symbol()).collect::<String>())
            })
            .collect()
    }

    /// The panel marks which row Tab or Enter would take, the way
    /// [`draw_picker`] marks its own selection.
    #[tokio::test]
    async fn the_selected_suggestion_row_is_highlighted() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.input = "/s".chars().collect();
        app.cursor = app.input.len();
        app.suggestion = 1;

        let highlighted = highlighted_rows(&app, 100, 20);

        assert_eq!(highlighted.len(), 1, "{highlighted:?}");
        assert!(highlighted[0].starts_with("/sandbox"), "{highlighted:?}");
    }

    /// Go's `resumeVisibleRange` windows the session list around its cursor.
    /// A picker holds up to `PICKER_LIST_LIMIT` (50) rows, more than a popup
    /// ever shows, so a selection below the fold has to scroll into view.
    #[tokio::test]
    async fn a_picker_scrolls_a_selection_below_the_fold_into_view() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.picker = Some(Picker {
            kind: PickerKind::Resume,
            rows: (0..50)
                .map(|index| PickerRow {
                    label: format!("session {index:02}"),
                    value: format!("path-{index}"),
                })
                .collect(),
            selected: 40,
        });

        let highlighted = highlighted_rows(&app, 100, 30);

        assert_eq!(highlighted.len(), 1, "{highlighted:?}");
        assert!(highlighted[0].contains("session 40"), "{highlighted:?}");
    }

    #[tokio::test]
    async fn ordinary_input_and_an_open_overlay_show_no_suggestions() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.input = "hello".chars().collect();
        app.cursor = app.input.len();
        let ordinary = rendered(&app, 100, 20);
        assert!(!ordinary.contains("show session details"), "{ordinary}");

        app.input = "/s".chars().collect();
        app.cursor = app.input.len();
        app.picker = Some(Picker {
            kind: PickerKind::Resume,
            rows: Vec::new(),
            selected: 0,
        });
        let overlaid = rendered(&app, 100, 20);
        assert!(!overlaid.contains("show session details"), "{overlaid}");
    }

    /// Where the terminal caret ends up after one frame.
    fn cursor(app: &App, width: u16, height: u16) -> (u16, u16) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let position = terminal
            .backend_mut()
            .get_cursor_position()
            .expect("cursor position");
        (position.x, position.y)
    }

    /// The caret sits after the last *column* the value occupies, not after
    /// its last `char`: a CJK character takes two cells.
    #[tokio::test]
    async fn the_caret_follows_the_display_width_of_wide_characters() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.input = "测试ab".chars().collect();
        app.cursor = app.input.len();

        let (x, y) = cursor(&app, 100, 20);

        assert_eq!((x, y), (1 + 6, 20 - 2));
    }

    /// A hard line break moves the caret to the next composer row.
    #[tokio::test]
    async fn the_caret_follows_an_embedded_newline() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.input = "ab\ncd".chars().collect();
        app.cursor = app.input.len();

        let (x, y) = cursor(&app, 100, 20);

        assert_eq!((x, y), (1 + 2, 20 - 2));
    }

    /// Past [`INPUT_BOX_THRESHOLD`] rows the composer stops growing, so it
    /// has to scroll: the tail of the value and the caret both stay inside
    /// the box instead of the first rows being pinned and the caret
    /// wandering onto the border.
    #[tokio::test]
    async fn a_value_taller_than_the_composer_scrolls_its_tail_into_view() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.input = format!("{}END", "x".repeat(98 * 13)).chars().collect();
        app.cursor = app.input.len();

        let height = 30;
        let content = rendered(&app, 100, height);
        let (x, y) = cursor(&app, 100, height);

        assert!(content.contains("END"), "{content}");
        // The box is `INPUT_BOX_THRESHOLD` text rows plus two borders, so
        // its last text row is the second-to-last row of the frame.
        assert_eq!((x, y), (1 + 3, height - 2));
    }
}
