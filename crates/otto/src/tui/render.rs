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

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use super::app::App;
use super::commands::SLASH_COMMANDS;
use super::entries::EntryKind;
use super::layout::{
    INPUT_BOX_THRESHOLD, MIN_TERMINAL_HEIGHT, MIN_TERMINAL_WIDTH, escape_plain_text,
    footer_workspace, format_context_percentage, format_token_count,
};
use super::markdown;

const DEFAULT_CONTEXT_WINDOW: i64 = 200_000;

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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(composer_height),
        ])
        .split(area);

    draw_transcript(frame, app, chunks[0]);
    draw_footer(frame, app, chunks[1]);
    draw_composer(frame, app, chunks[2]);

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
    let text: String = app.input.iter().collect();
    let wrap_width = width.saturating_sub(2).max(1);
    let wrapped = Paragraph::new(text).wrap(Wrap { trim: false });
    let lines = wrapped.line_count(wrap_width).max(1) as u16;
    lines.clamp(1, INPUT_BOX_THRESHOLD) + 2
}

fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in &app.entries {
        lines.extend(entry_lines(entry));
    }

    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total_lines = paragraph.line_count(area.width) as u16;
    let scroll = match app.scroll {
        Some(offset) => offset,
        None => total_lines.saturating_sub(area.height),
    };
    frame.render_widget(paragraph.scroll((scroll, 0)), area);
}

/// Renders one transcript entry. User/assistant/system/compaction/error text
/// is markdown; tool call/result bodies are plain, pre-escaped text (Go
/// never runs tool output through the markdown renderer either).
fn entry_lines(entry: &super::entries::Entry) -> Vec<Line<'static>> {
    match entry.kind {
        Some(EntryKind::Tool) => {
            let mut lines = vec![Line::from(Span::styled(
                format!("[tool] {} ({})", entry.tool_name, entry.tool_call_id),
                Style::default().add_modifier(Modifier::BOLD),
            ))];
            if !entry.tool_args.is_empty() {
                lines.push(Line::raw(escape_plain_text(&entry.tool_args)));
            }
            if entry.tool_done {
                let style = if entry.tool_error {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default()
                };
                lines.push(Line::styled(escape_plain_text(&entry.tool_output), style));
            }
            lines
        }
        _ => markdown::render(&entry.raw).lines,
    }
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let percentage = format_context_percentage(app.usage.input_tokens, DEFAULT_CONTEXT_WINDOW);
    let tokens = format_token_count(app.usage.input_tokens + app.usage.output_tokens);
    let workspace = footer_workspace(&app.workspace);
    let mut text = format!("{workspace}  {tokens} tokens ({percentage})");
    if let Some(status) = &app.status {
        text = format!("{status}  |  {text}");
    }
    frame.render_widget(
        Paragraph::new(text).style(Style::default().add_modifier(Modifier::DIM)),
        area,
    );
}

fn draw_composer(frame: &mut Frame, app: &App, area: Rect) {
    let text: String = app.input.iter().collect();
    let title = if app.busy {
        "Working (Esc to cancel)"
    } else {
        "Otto"
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);

    if !app.busy {
        let prefix: String = app.input[..app.cursor].iter().collect();
        let wrap_width = inner.width.max(1) as usize;
        let cursor_line = (prefix.chars().count() / wrap_width) as u16;
        let cursor_col = (prefix.chars().count() % wrap_width) as u16;
        frame.set_cursor_position((inner.x + cursor_col, inner.y + cursor_line));
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
        .enumerate()
        .map(|(index, row)| {
            let style = if index == picker.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(Line::styled(row.label.clone(), style))
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(picker.kind.title()),
    );
    frame.render_widget(list, popup);
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
/// - `TestLongSessionOverlayAndFooterStayWithinBounds` — tests
///   `renderFooter`'s width-based field-dropping (e.g. dropping "Profile:"
///   under narrow widths). [`draw_footer`] here is a single fixed-format
///   line with no width-based field omission: a real (not just untested)
///   gap versus Go, noted for the record rather than silently skipped.
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::cli::testutil;

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
}
