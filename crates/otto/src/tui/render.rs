//! Turns [`super::app::App`] state into ratatui widgets.
//!
//! ponytail: only `/resume`, `/archive`, and `/model`'s profile choice are
//! genuinely a *selection*; the rest are a fixed block of text with no
//! interaction, so those render as ordinary transcript entries (see
//! [`super::app`]'s module doc) drawn by the same transcript paragraph as
//! everything else. `/resume`, `/archive`, and `/model` share one
//! [`super::app::Picker`] overlay rather than three near-identical dedicated
//! screens. Upgrade path: give any of these a dedicated layout if a user
//! reports the shared one as confusing.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{App, ApprovalDialog, TurnStatus};
use super::commands::{SLASH_COMMANDS, SlashCommand};
use super::entries::Entry;
use super::layout::{
    INPUT_BOX_THRESHOLD, MIN_TERMINAL_HEIGHT, MIN_TERMINAL_WIDTH, SIDE_MARGIN, escape_plain_text,
    escape_single_line_text, footer_workspace, format_context_percentage, format_token_count,
};
use super::transcript;
use crate::subagent::format::{one_line, round_to_seconds};
use crate::subagent::tasks::{Task, TaskStatus};

/// Draws one frame.
pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width < MIN_TERMINAL_WIDTH || area.height < MIN_TERMINAL_HEIGHT {
        let message = format!(
            "Terminal too small: resize to at least {MIN_TERMINAL_WIDTH}x{MIN_TERMINAL_HEIGHT}."
        );
        frame.render_widget(Paragraph::new(message).alignment(Alignment::Center), area);
        return;
    }

    let content_area = side_margin(area);
    let panel_tasks = visible_panel_tasks(&app.tasks);
    let (composer_height, panel_height) = composer_and_panel_height(
        app,
        content_area.width,
        content_area.height,
        panel_row_count(panel_tasks.len()),
    );
    let suggestions = app.suggestions();
    // The suggestion list may take every row the composer, the panel, and
    // the status line leave, except the one the transcript keeps.
    let suggestion_height = (suggestions.len() as u16).min(
        content_area
            .height
            .saturating_sub(composer_height + panel_height + 2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(suggestion_height),
            Constraint::Length(composer_height),
            Constraint::Length(panel_height),
            Constraint::Length(1),
        ])
        .split(content_area);

    draw_transcript(frame, app, chunks[0]);
    draw_suggestions(frame, app, &suggestions, chunks[1]);
    draw_composer(frame, app, chunks[2]);
    draw_panel(frame, &panel_tasks, chunks[3]);
    draw_footer(frame, app, chunks[4]);

    if !app.busy()
        && let Some(approval) = &app.approval
    {
        draw_approval(frame, area, approval);
    } else if app.show_help {
        draw_help(frame, area);
    } else if let Some(view) = &app.context {
        draw_context(frame, area, view);
    } else if let Some(view) = &app.agents {
        draw_agents(frame, area, view);
    } else if let Some(picker) = &app.picker {
        draw_picker(frame, area, picker);
    }

    // Last, over whatever ended up on screen: the selection is a property of
    // the drawn cells, not of any one widget.
    if let Some(selection) = &app.selection {
        selection.highlight(frame.buffer_mut());
    }
}

fn side_margin(area: Rect) -> Rect {
    if area.width <= SIDE_MARGIN.saturating_mul(2) {
        return area;
    }
    Rect {
        x: area.x + SIDE_MARGIN,
        width: area.width - SIDE_MARGIN * 2,
        ..area
    }
}

/// The composer's inner-row floor with an empty or short value. Wrapped input
/// grows it up to [`INPUT_BOX_THRESHOLD`] as normal.
const COMPOSER_MIN_INNER_ROWS: u16 = 1;

/// The composer box's total height (inner rows plus the two border rows) for
/// a given inner-row floor. Grows to fit wrapped input up to
/// [`INPUT_BOX_THRESHOLD`] lines before it stops growing and scrolls instead.
fn composer_height(app: &App, width: u16, min_inner_rows: u16) -> u16 {
    // `width - 2` is the box's inner width, so this sizes the box from
    // exactly the rows [`draw_composer`] will put in it.
    let (lines, _, _) = composer_lines(&app.input, app.cursor, width.saturating_sub(2));
    (lines.len() as u16).clamp(min_inner_rows, INPUT_BOX_THRESHOLD) + 2
}

/// Splits the height the transcript's guaranteed row and the status line's
/// fixed row leave between the composer and the sub-agent panel, returning
/// `(composer_height, panel_height)`.
///
/// Priority order when space is short: the panel is dropped first (down to
/// 0 rows, from `panel_rows_wanted`); only once it has reached 0 does the
/// composer's empty-input floor shrink from [`COMPOSER_MIN_INNER_ROWS`]
/// toward 1 inner row. `draw` already refuses to lay out below
/// [`MIN_TERMINAL_HEIGHT`], so `height` here is always at least that.
fn composer_and_panel_height(
    app: &App,
    width: u16,
    height: u16,
    panel_rows_wanted: u16,
) -> (u16, u16) {
    let budget = height.saturating_sub(1 /* transcript */ + 1 /* status line */);
    let natural_composer = composer_height(app, width, COMPOSER_MIN_INNER_ROWS);
    if let Some(spare) = budget.checked_sub(natural_composer) {
        return (natural_composer, panel_rows_wanted.min(spare));
    }
    let shrunk_composer = composer_height(app, width, 1).min(budget).max(3);
    (shrunk_composer, 0)
}

const STARTUP_LOGO: &str = "     ____  __  __\n    / __ \\/ /_/ /____\n   / /_/ / __/ __/ __ \\\n   \\____/\\__/\\__/\\____/";

fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = transcript::lines(&app.entries, app.show_details, area.width as usize);
    if let Some(queued) = &app.queued_input {
        let queued = Entry {
            kind: Some(super::entries::EntryKind::User),
            raw: format!("Queued for current turn: {queued}"),
            ..Entry::default()
        };
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(transcript::lines(
            &[queued],
            app.show_details,
            area.width as usize,
        ));
    }
    if lines.is_empty() {
        lines.extend(
            STARTUP_LOGO
                .lines()
                .map(|line| Line::from(line.to_string())),
        );
    }
    if let Some(status) = app.thinking() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(thinking_line(&status));
    }

    // No `Wrap`: `transcript` already broke every row to `area.width` so it
    // could keep a gutter marker on each one, and re-wrapping here would
    // split those rows again and lose the alignment.
    let paragraph = Paragraph::new(Text::from(lines));
    let total_lines = paragraph.line_count(area.width) as u16;
    let bottom = total_lines.saturating_sub(area.height);
    // The scroll keys need the bottom this layout produced to turn
    // "following" into an absolute offset; nothing outside the renderer
    // knows how the entries wrap at this width.
    app.max_scroll.set(bottom);
    let scroll = app.scroll.map_or(bottom, |top| top.min(bottom));
    frame.render_widget(paragraph.scroll((scroll, 0)), area);
}

/// Spinner frames, in order.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long one [`SPINNER_FRAMES`] frame is held. [`super::drive_turn`]
/// redraws on this interval for as long as a turn runs.
pub(super) const SPINNER_FRAME: Duration = Duration::from_millis(100);

/// How often the idle loop (`super::run_app`) redraws just to keep the
/// sub-agent panel's elapsed times current. Only runs while
/// [`needs_task_clock`] is true; during a turn `drive_turn`'s existing
/// [`SPINNER_FRAME`] cadence already covers it.
pub(super) const TASKS_PANEL_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// The line shown under the transcript while a turn is in flight: the phase
/// (waiting for the model, reasoning, running a tool, a provider retry) and
/// how long the phase and the turn have lasted, so a slow step is
/// distinguishable from a hung terminal.
fn thinking_line(status: &TurnStatus) -> Line<'static> {
    let frame = status.turn_elapsed.as_millis() / SPINNER_FRAME.as_millis();
    Line::styled(
        format!(
            "{} {}",
            SPINNER_FRAMES[frame as usize % SPINNER_FRAMES.len()],
            otto_core::wire::transcript::status_line(
                &status.phase,
                status.phase_elapsed.as_secs(),
                status.turn_elapsed.as_secs()
            )
        ),
        Style::default().fg(Color::Magenta),
    )
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
    let thinking = if app.info.thinking.is_empty() {
        "default".to_string()
    } else {
        escape_single_line_text(&app.info.thinking)
    };
    let mut text = format!(
        "{profile_model} | think {thinking} | {} | {} | tokens {}/{}",
        app.info.sandbox.summary(),
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
/// highlighted.
///
/// A [`List`] rather than a [`Paragraph`] so that ratatui's own [`ListState`]
/// scrolls the selected row into view when the match list is longer than the
/// rows [`draw`] could give the panel.
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
    let (title, style) = if app.busy() && app.queued_input.is_some() {
        (
            "Queued for next checkpoint · Ctrl+U withdraw · Esc cancels turn",
            Style::default().fg(Color::Cyan),
        )
    } else if app.busy() {
        (
            "Working — Enter queues for this turn · Esc cancels turn",
            Style::default().fg(Color::Magenta),
        )
    } else {
        ("Otto", Style::default())
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(title, style));
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

    frame.set_cursor_position((inner.x + caret_column, inner.y + caret_row - scroll));
}

/// Sub-agent tasks the panel shows: queued or running, in registry order.
/// A finished task (succeeded, failed, canceled) never appears here.
fn visible_panel_tasks(tasks: &[Task]) -> Vec<&Task> {
    tasks
        .iter()
        .filter(|task| matches!(task.status, TaskStatus::Queued | TaskStatus::Running))
        .collect()
}

/// The panel's row count for `visible_count` eligible tasks: 0 with none,
/// otherwise up to 4 (the 4th row becomes a "+N more" summary once there are
/// more than 4).
fn panel_row_count(visible_count: usize) -> u16 {
    visible_count.min(4) as u16
}

/// Whether the sub-agent panel needs a periodic redraw to keep its elapsed
/// times current: true iff at least one task is queued or running. Drives
/// the idle-loop 1-second timer in [`super::run_app`]; with nothing queued
/// or running, no timer runs.
pub(crate) fn needs_task_clock(tasks: &[Task]) -> bool {
    !visible_panel_tasks(tasks).is_empty()
}

/// The sub-agent panel below the composer: one row per queued/running task
/// in registry order, capped at 4 rows with a "+N more" row once there are
/// more. Reads only the `tasks` snapshot [`App::refresh_tasks`] already
/// took, so drawing performs no lock or query.
fn draw_panel(frame: &mut Frame, tasks: &[&Task], area: Rect) {
    if area.height == 0 || tasks.is_empty() {
        return;
    }
    let now = chrono::Utc::now();
    let shown = tasks.len().min(4);
    let overflow = tasks.len() > 4;
    let rows = if overflow { 3 } else { shown };
    let columns = panel_columns(&tasks[..rows]);
    let mut lines: Vec<Line<'static>> = tasks[..rows]
        .iter()
        .map(|task| panel_row_line(task, now, area.width, columns))
        .collect();
    if overflow {
        lines.push(Line::styled(
            format!("+{} more", tasks.len() - rows),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

#[derive(Clone, Copy)]
struct PanelColumns {
    name: usize,
    model: usize,
}

fn panel_columns(tasks: &[&Task]) -> PanelColumns {
    PanelColumns {
        name: tasks
            .iter()
            .map(|task| UnicodeWidthStr::width(panel_name(task)))
            .max()
            .unwrap_or_default(),
        model: tasks
            .iter()
            .map(|task| UnicodeWidthStr::width(panel_model(task).as_str()))
            .max()
            .unwrap_or_default(),
    }
}

fn panel_name(task: &Task) -> &str {
    [task.name.as_str(), task.agent.as_str()]
        .into_iter()
        .find(|label| !label.is_empty())
        .unwrap_or("default")
}

fn panel_model(task: &Task) -> String {
    if task.model.is_empty() {
        "?".to_string()
    } else {
        escape_single_line_text(&task.model)
    }
}

/// One panel row: status, task id, name, model, live input/output tokens,
/// elapsed time, and description. Columns expand to their longest visible
/// value; only the trailing description is clipped to the terminal width.
fn panel_row_line(
    task: &Task,
    now: chrono::DateTime<chrono::Utc>,
    width: u16,
    columns: PanelColumns,
) -> Line<'static> {
    let marker = panel_marker(task, now);
    let name = panel_name(task);
    let model = panel_model(task);
    let (input_tokens, output_tokens) = if task.usage_present {
        (
            format_token_count(task.usage.input_tokens),
            format_token_count(task.usage.output_tokens),
        )
    } else {
        ("-".to_string(), "-".to_string())
    };
    let elapsed = panel_elapsed(task, now);
    let prefix = format!(
        "{marker} {} {} {} in:{} out:{} {}  ",
        fit_column(&task.id, 4, Alignment::Left),
        fit_column(name, columns.name, Alignment::Left),
        fit_column(&model, columns.model, Alignment::Left),
        fit_column(&input_tokens, 6, Alignment::Right),
        fit_column(&output_tokens, 6, Alignment::Right),
        fit_column(&elapsed, 6, Alignment::Right),
    );
    let remaining = (width as usize).saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
    let description = truncate_to_width(
        &escape_single_line_text(&one_line(&task.description)),
        remaining,
    );
    Line::raw(format!("{prefix}{description}"))
}

/// A running task's marker animates through [`SPINNER_FRAMES`] on the same
/// cadence as the transcript's thinking line; a queued task gets a fixed,
/// visually distinct marker instead.
fn panel_marker(task: &Task, now: chrono::DateTime<chrono::Utc>) -> &'static str {
    match (task.status, task.started_at) {
        (TaskStatus::Running, Some(started)) => {
            let elapsed_ms = now.signed_duration_since(started).num_milliseconds().max(0) as u128;
            let frame = (elapsed_ms / SPINNER_FRAME.as_millis()) as usize;
            SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]
        }
        (TaskStatus::Running, None) => SPINNER_FRAMES[0],
        _ => "○",
    }
}

/// `now - started_at` for a running task, `now - created_at` for a queued
/// one, rounded the same way [`super::agents_view`]'s duration column is.
fn panel_elapsed(task: &Task, now: chrono::DateTime<chrono::Utc>) -> String {
    let start = match task.status {
        TaskStatus::Running => task.started_at,
        _ => task.created_at,
    };
    match start {
        Some(start) => round_to_seconds(now.signed_duration_since(start)),
        None => "0s".to_string(),
    }
}

/// Truncates `value` to at most `max_width` display columns, breaking
/// between characters rather than mid-character. No ellipsis: the panel row
/// is meant to be scanned, not read in full, and ratatui would clip an
/// over-width line the same way regardless.
fn truncate_to_width(value: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0usize;
    for ch in value.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out
}

fn fit_column(value: &str, width: usize, alignment: Alignment) -> String {
    let padding = " ".repeat(width.saturating_sub(UnicodeWidthStr::width(value)));
    match alignment {
        Alignment::Right => format!("{padding}{value}"),
        _ => format!("{value}{padding}"),
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
    // Stateful so that ratatui windows the list around the selection: a picker
    // lists up to `PICKER_LIST_LIMIT` sessions, more than the popup can show.
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

fn draw_approval(frame: &mut Frame, area: Rect, approval: &ApprovalDialog) {
    let popup = centered_rect_sized(76, 8, area);
    frame.render_widget(Clear, popup);
    let inner_width = popup.width.saturating_sub(4) as usize;
    let mut lines = vec![
        Line::from(Span::styled(
            "Run elevated Bash?",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("This command would run outside the sandbox."),
    ];
    if !approval.command.is_empty() {
        lines.push(label_value_line(
            "Command: ",
            &approval.command,
            inner_width,
        ));
    }
    if !approval.justification.is_empty() {
        lines.push(label_value_line(
            "Reason: ",
            &approval.justification,
            inner_width,
        ));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("y = yes", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" · "),
        Span::styled("n/Esc = no", Style::default().add_modifier(Modifier::BOLD)),
    ]));

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title("Approval"));
    frame.render_widget(paragraph, popup);
}

fn label_value_line<'a>(label: &'static str, value: &str, width: usize) -> Line<'a> {
    let label_width = UnicodeWidthStr::width(label);
    let value_width = width.saturating_sub(label_width);
    Line::from(vec![
        Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(ellipsis_single_line(value, value_width)),
    ])
}

fn ellipsis_single_line(value: &str, max_width: usize) -> String {
    let escaped = escape_single_line_text(value);
    if UnicodeWidthStr::width(escaped.as_str()) <= max_width {
        return escaped;
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let keep = max_width - 3;
    let mut out = String::new();
    let mut width = 0;
    for ch in escaped.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > keep {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push_str("...");
    out
}

/// The `/context` overlay: the section list, or one item's full text over it.
fn draw_context(frame: &mut Frame, area: Rect, view: &super::context_view::ContextView) {
    let popup = centered_rect(80, 70, area);
    frame.render_widget(Clear, popup);
    if let Some(text) = &view.text {
        let paragraph = Paragraph::new(text.text.as_str())
            .wrap(Wrap { trim: false })
            .scroll((text.scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("{} (esc to go back)", text.title)),
            );
        frame.render_widget(paragraph, popup);
        return;
    }
    let items: Vec<ListItem> = view
        .rows()
        .into_iter()
        .map(|row| ListItem::new(view.row_label(row)))
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(view.header()))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(view.selected));
    frame.render_stateful_widget(list, popup, &mut state);
}

/// The `/agents` overlay: the task list, narrowed to the terminal width by
/// `Table`'s own percentage columns, or one task's detail pane over it.
fn draw_agents(frame: &mut Frame, area: Rect, view: &super::agents_view::AgentsView) {
    let popup = centered_rect(96, 90, area);
    frame.render_widget(Clear, popup);
    match &view.detail {
        Some(detail) => draw_agent_detail(frame, popup, detail),
        None => draw_agent_list(frame, popup, view),
    }
}

fn draw_agent_list(frame: &mut Frame, area: Rect, view: &super::agents_view::AgentsView) {
    use super::agents_view::{COLUMN_HEADERS, columns};

    let now = chrono::Utc::now();
    let header =
        Row::new(COLUMN_HEADERS.to_vec()).style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = view
        .rows
        .iter()
        .map(|row| Row::new(columns(row, now).to_vec()))
        .collect();
    let widths = [
        Constraint::Percentage(9),
        Constraint::Percentage(9),
        Constraint::Percentage(22),
        Constraint::Percentage(9),
        Constraint::Percentage(9),
        Constraint::Percentage(13),
        Constraint::Percentage(7),
        Constraint::Percentage(6),
        Constraint::Percentage(6),
        Constraint::Percentage(10),
    ];
    let title = format!(
        "{}  (up/down move, s status, w workspace, enter open, esc close)",
        view.header()
    );
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state =
        TableState::default().with_selected((!view.rows.is_empty()).then_some(view.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

/// The prompt, result or error, and the child transcript (reusing
/// [`transcript::lines`], the same renderer the main transcript pane uses),
/// as one scrollable paragraph.
fn draw_agent_detail(frame: &mut Frame, area: Rect, detail: &super::agents_view::Detail) {
    let row = &detail.row;
    let agent = if row.agent.is_empty() {
        "default"
    } else {
        &row.agent
    };
    let mut lines: Vec<Line<'static>> = vec![Line::from(format!(
        "{} · {agent} · {}",
        row.task_id, row.status
    ))];
    if !row.description.is_empty() {
        lines.push(Line::from(format!(
            "Description: {}",
            escape_plain_text(&row.description)
        )));
    }
    lines.push(Line::default());
    lines.push(Line::from("Prompt:"));
    lines.extend(
        escape_plain_text(&row.prompt)
            .split('\n')
            .map(|line| Line::from(line.to_string()))
            .collect::<Vec<_>>(),
    );
    if !row.error.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from("Error:"));
        lines.extend(
            escape_plain_text(&row.error)
                .split('\n')
                .map(|line| Line::from(line.to_string()))
                .collect::<Vec<_>>(),
        );
    } else if !row.result.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from("Result:"));
        lines.extend(
            escape_plain_text(&row.result)
                .split('\n')
                .map(|line| Line::from(line.to_string()))
                .collect::<Vec<_>>(),
        );
    }
    lines.push(Line::default());
    if detail.transcript_missing {
        lines.push(Line::from("(no child transcript)"));
    } else {
        lines.extend(transcript::lines(
            &detail.entries,
            false,
            area.width.saturating_sub(2) as usize,
        ));
    }
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((detail.scroll, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("{} (esc to go back)", row.task_id)),
        );
    frame.render_widget(paragraph, area);
}

/// A centered `percent_x` by `percent_y` rectangle within `area`. Standard
/// ratatui popup-centering helper, from the project's own examples.
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

fn centered_rect_sized(percent_x: u16, height: u16, area: Rect) -> Rect {
    let height = height.min(area.height);
    let top = area.height.saturating_sub(height) / 2;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top),
            Constraint::Length(height),
            Constraint::Min(0),
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

/// Terminal-size and wrapping cases, against ratatui's own render loop.
///
/// Ratatui renders each widget into a `Rect`/`Buffer` that the framework itself
/// always clips to (see this module's own doc comment above [`draw`], and
/// `layout.rs`'s: "ratatui's own `Paragraph::wrap` and `Layout` constraints
/// already do line-wrapping and area-fitting").
///
/// What is left to test is the one static guard
/// ([`MIN_TERMINAL_WIDTH`]/[`MIN_TERMINAL_HEIGHT`], below) and a no-panic
/// guarantee at extreme sizes.
#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use otto_core::agent::Event;
    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend};

    use super::*;
    use crate::cli::testutil;
    use crate::tui::app::{Picker, PickerKind, PickerRow};
    use crate::tui::entries::{Entry, EntryKind};

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

    /// The drawn rows as plain text, without `TestBackend`'s `Display`
    /// quoting and without the layout's side margin, so a test can assert on
    /// the gutter a reader actually sees.
    fn screen_rows(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                let row: String = (0..width)
                    .filter_map(|x| buffer.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .collect();
                row.trim_end()
                    .chars()
                    .skip(SIDE_MARGIN as usize)
                    .collect::<String>()
            })
            .collect()
    }

    #[tokio::test]
    async fn empty_startup_transcript_shows_the_otto_logo() {
        let (_workspace, _sessions, app) = app_fixture().await;

        let screen = screen_rows(&app, 80, 20).join("\n");

        assert!(screen.contains("____  __  __"), "{screen}");
        assert!(screen.contains("/ __ \\/ /_/ /____"), "{screen}");
        assert!(screen.contains("/ /_/ / __/ __/ __ \\"), "{screen}");
        assert!(screen.contains("\\____/\\__/\\__/\\____/"), "{screen}");
    }

    /// The drawn frame's buffer, for the tests that assert on colour rather
    /// than on glyphs.
    fn drawn(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        terminal.backend().buffer().clone()
    }

    /// The first row whose leftmost transcript cell carries the prompt band.
    fn banded_row(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> Option<u16> {
        (0..height).find(|y| {
            (SIDE_MARGIN..width - SIDE_MARGIN).any(|x| {
                buffer
                    .cell((x, *y))
                    .is_some_and(|cell| cell.bg == transcript::PROMPT_BACKGROUND)
            })
        })
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The frame index and the turn count both come from one `Duration`, so
    /// they cannot disagree about how long the turn has been running.
    #[test]
    fn the_status_line_names_the_phase_and_advances_with_elapsed_time() {
        let at = |phase_ms, turn_ms| {
            line_text(&thinking_line(&TurnStatus {
                phase: "reasoning".into(),
                phase_elapsed: Duration::from_millis(phase_ms),
                turn_elapsed: Duration::from_millis(turn_ms),
            }))
        };

        assert_eq!(at(0, 0), "⠋ reasoning · 0s · turn 0s");
        assert_eq!(at(0, 100), "⠙ reasoning · 0s · turn 0s");
        assert_eq!(at(1_000, 5_000), "⠋ reasoning · 1s · turn 5s");
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

        let idle = rendered(&app, 50, 10);
        app.start_turn();
        let busy = rendered(&app, 50, 10);
        app.apply_event(Event::ToolCallStarted {
            operation_id: otto_core::model::OperationId::new("op_test").expect("operation id"),
            attempt: 1,
            tool_name: "bash".into(),
            tool_call_id: "c1".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        });
        let running = rendered(&app, 50, 10);
        app.end_turn();

        assert!(!idle.contains("waiting"), "idle transcript:\n{idle}");
        assert!(
            busy.contains("waiting for model · 0s · turn 0s"),
            "running transcript:\n{busy}"
        );
        assert!(
            running.contains(r#"running bash {"command":"ls"}"#),
            "tool transcript:\n{running}"
        );
        assert!(!rendered(&app, 50, 10).contains("turn"));
    }

    #[tokio::test]
    async fn committed_queued_input_draws_in_transcript_not_composer() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.start_turn();
        app.queued_input = Some("follow up".to_string());

        let screen = rendered(&app, 72, 12);

        assert!(
            screen.contains("❯ Queued for current turn: follow up"),
            "{screen}"
        );
        assert!(
            screen.contains("Queued for next checkpoint · Ctrl+U withdraw"),
            "{screen}"
        );
        assert!(
            !screen.contains("│follow up"),
            "queued text should not remain in the composer:\n{screen}"
        );
    }

    /// The drawn frame, not the row builder: a prompt, the reply after it,
    /// and a tool call each reach the screen under their own marker, so a
    /// turn boundary is visible in the rendering the reader actually sees.
    /// `transcript`'s own tests cover the row rules.
    #[tokio::test]
    async fn a_drawn_turn_shows_a_prompt_a_reply_and_a_tool_call_apart() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.entries = vec![
            Entry {
                kind: Some(EntryKind::User),
                raw: "hello otto".to_string(),
                ..Default::default()
            },
            Entry {
                kind: Some(EntryKind::Tool),
                tool_call_id: "call-1".to_string(),
                tool_name: "bash".to_string(),
                tool_args: r#"{"command":"ls"}"#.to_string(),
                tool_output: "a.txt".to_string(),
                tool_done: true,
                ..Default::default()
            },
            Entry {
                kind: Some(EntryKind::Assistant),
                raw: "the reply".to_string(),
                ..Default::default()
            },
        ];

        let screen = rendered(&app, 60, 12);

        assert!(screen.contains("❯ hello otto"), "{screen}");
        assert!(screen.contains("\u{23fa} bash"), "{screen}");
        assert!(screen.contains("\u{23bf} a.txt"), "{screen}");
        assert!(screen.contains("\u{23fa} the reply"), "{screen}");
    }

    /// A prompt too wide for the terminal keeps one marker and an aligned
    /// indent on the rows the wrap produced, which is what `Paragraph::wrap`
    /// used to lose entirely.
    #[tokio::test]
    async fn a_wrapped_prompt_is_marked_once_and_stays_aligned() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.entries = vec![Entry {
            kind: Some(EntryKind::User),
            raw: "alpha bravo charlie delta echo foxtrot golf".to_string(),
            ..Default::default()
        }];

        let rows = screen_rows(&app, 44, 10);
        let prompt: Vec<&String> = rows.iter().filter(|row| !row.is_empty()).take(2).collect();

        assert_eq!(prompt.len(), 2, "{rows:?}");
        assert!(prompt[0].starts_with("❯ "), "{rows:?}");
        assert!(prompt[1].starts_with("  "), "{rows:?}");
        assert!(!prompt[1].trim_start().starts_with('\u{276f}'), "{rows:?}");
    }

    /// The band is the thing a reader scrolls back to find, so it has to
    /// survive the drawn frame: `Paragraph` colours only the cells a span
    /// covers, and it stops at the transcript's own width, never running
    /// into the side margin.
    #[tokio::test]
    async fn a_drawn_prompt_is_banded_across_the_content_width() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.entries = vec![Entry {
            kind: Some(EntryKind::User),
            raw: "hi".to_string(),
            ..Default::default()
        }];
        let (width, height) = (40u16, 10u16);

        let buffer = drawn(&app, width, height);
        let row = banded_row(&buffer, width, height).expect("the prompt row is on screen");
        for x in SIDE_MARGIN..width - SIDE_MARGIN {
            let cell = buffer.cell((x, row)).expect("cell in bounds");
            assert_eq!(
                cell.bg,
                transcript::PROMPT_BACKGROUND,
                "column {x} is not banded"
            );
        }
        for x in [0, width - 1] {
            let cell = buffer.cell((x, row)).expect("cell in bounds");
            assert_ne!(
                cell.bg,
                transcript::PROMPT_BACKGROUND,
                "the band ran into the margin"
            );
        }
    }

    /// The same band under wide glyphs.
    ///
    /// The walk steps by display width, as [`super::super::selection`] does:
    /// `ratatui` writes a wide character into the first of the cells it
    /// covers and its frame diff never sends the rest, so `TestBackend` holds
    /// a default-coloured cell where a terminal paints the glyph's own
    /// background. Reading those cells back would assert on an artifact of
    /// the test backend rather than on what the reader sees.
    #[tokio::test]
    async fn a_drawn_cjk_prompt_is_banded_too() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.entries = vec![Entry {
            kind: Some(EntryKind::User),
            raw: "集群数量极多".to_string(),
            ..Default::default()
        }];
        let (width, height) = (44u16, 12u16);

        let buffer = drawn(&app, width, height);
        let row = banded_row(&buffer, width, height).expect("the prompt row is on screen");
        let mut x = SIDE_MARGIN;
        while x < width - SIDE_MARGIN {
            let cell = buffer.cell((x, row)).expect("cell in bounds");
            assert_eq!(
                cell.bg,
                transcript::PROMPT_BACKGROUND,
                "column {x} breaks the band"
            );
            x += cell.symbol().width().max(1) as u16;
        }
    }

    #[tokio::test]
    async fn approval_dialog_is_drawn_with_clear_choices_and_truncates_long_commands() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        let long_command = format!("git push {}", "very-long-ref-name-".repeat(12));
        app.approval = Some(ApprovalDialog {
            id: "approval-1".to_string(),
            command: long_command.clone(),
            justification: "publish the reviewed branch".to_string(),
        });

        let screen = rendered(&app, 80, 16);

        assert!(screen.contains("Run elevated Bash?"), "{screen}");
        assert!(screen.contains("outside the sandbox"), "{screen}");
        assert!(screen.contains("Command:"), "{screen}");
        assert!(screen.contains("git push"), "{screen}");
        assert!(screen.contains("..."), "{screen}");
        assert!(!screen.contains(&long_command), "{screen}");
        assert!(
            screen.contains("Reason: publish the reviewed branch"),
            "{screen}"
        );
        assert!(screen.contains("y = yes"), "{screen}");
        assert!(screen.contains("n/Esc = no"), "{screen}");
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

    #[tokio::test]
    async fn the_context_overlay_shows_the_header_sections_and_item_text() {
        use crate::tui::context_view::ContextView;
        use otto_core::agent::context_report::{
            ContextItem, ContextReport, ContextSection, SectionKind,
        };

        let (_workspace, _sessions, mut app) = app_fixture().await;
        let mut view = ContextView::new(ContextReport {
            model: "gpt-5".into(),
            context_window: 0,
            compaction_threshold: 0,
            estimated_total: 1_200,
            reported_input_tokens: None,
            sections: vec![ContextSection {
                kind: SectionKind::SystemPrompt,
                tokens: 1_200,
                items: vec![ContextItem {
                    label: "Base".into(),
                    tokens: 1_200,
                    text: "You are Otto.\nsecond line".into(),
                }],
            }],
        });
        app.context = Some(view.clone());

        let screen = screen_rows(&app, 120, 30).join("\n");
        assert!(
            screen.contains("Context  gpt-5 · ~1.2k tokens (estimate)"),
            "{screen}"
        );
        assert!(screen.contains("System prompt"), "{screen}");

        view.handle_key(KeyCode::Enter);
        view.handle_key(KeyCode::Down);
        view.handle_key(KeyCode::Enter);
        app.context = Some(view);
        let screen = screen_rows(&app, 120, 30).join("\n");
        assert!(screen.contains("Base · ~1.2k tokens"), "{screen}");
        assert!(screen.contains("You are Otto."), "{screen}");
        assert!(screen.contains("second line"), "{screen}");
    }

    /// A controller whose builder carries a task recorder, mirroring
    /// `cli::repl_commands::tests::controller_with_task_recorder`.
    async fn controller_with_task_recorder(
        workspace: &std::path::Path,
        sessions: &std::path::Path,
        store: std::sync::Arc<crate::subagent::record::Store>,
    ) -> crate::app::Controller {
        let mut builder = testutil::builder(workspace, sessions);
        builder.shared_mut().task_recorder = Some(store);
        let runtime = testutil::initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        let info = builder.runtime_info(&runtime);
        crate::app::Controller::new(builder, true, session, runner, info)
    }

    /// The spec's TUI acceptance case: the modal lists rows, narrowed to an
    /// 80-column terminal, and opening a row shows its detail pane.
    #[tokio::test]
    async fn the_agents_overlay_lists_rows_and_fits_eighty_columns() {
        use crate::subagent::record::{self, TaskContext};
        use crate::subagent::tasks::{Task, TaskStatus};
        use crate::tui::agents_view::AgentsView;

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = std::sync::Arc::new(record::Store::open_in_memory().expect("store"));
        let workspace_path = workspace.path().to_str().expect("utf8 path").to_string();
        record::Recorder::upsert(
            &*store,
            &TaskContext {
                parent_session: "s1".into(),
                parent_session_path: "/sessions/s1.jsonl".into(),
                workspace: workspace_path,
                pid: 4_294_967_294,
                process_started_at: "2026-09-25T10:00:00Z".into(),
            },
            &Task {
                id: "t1".into(),
                description: "review the diff".into(),
                status: TaskStatus::Succeeded,
                created_at: Some(chrono::Utc::now()),
                ..Task::default()
            },
        );
        let controller = controller_with_task_recorder(
            workspace.path(),
            sessions.path(),
            std::sync::Arc::clone(&store),
        )
        .await;
        let mut app = App::new(&controller);
        app.agents = Some(AgentsView::open(&controller));

        // At 80 columns the description column is narrower than the full
        // text; `Table` truncates it rather than panicking or overflowing.
        let screen = screen_rows(&app, 80, 24).join("\n");
        assert!(screen.contains("succeed"), "{screen}");
        assert!(screen.contains("review t"), "{screen}");

        app.agents
            .as_mut()
            .expect("open")
            .handle_key(KeyCode::Enter, &controller);
        let screen = screen_rows(&app, 80, 24).join("\n");
        assert!(screen.contains("t1"), "{screen}");
        assert!(screen.contains("no child transcript"), "{screen}");
    }

    /// Below the static minimum on either axis, the frame is just the resize
    /// message; this layout is purely static (see [`composer_height`]), so
    /// there is no second, dynamic guard.
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
    async fn normal_layout_leaves_side_margins() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.push_system("hello from otto");
        app.input = "draft".chars().collect();
        app.cursor = app.input.len();

        let width = 80;
        let height = 20;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
        let buffer = terminal.backend().buffer();

        for y in 0..height {
            assert_eq!(
                buffer.cell((0, y)).expect("left margin").symbol(),
                " ",
                "row {y} should leave the left edge empty"
            );
            assert_eq!(
                buffer.cell((width - 1, y)).expect("right margin").symbol(),
                " ",
                "row {y} should leave the right edge empty"
            );
        }
    }

    #[tokio::test]
    async fn footer_shows_runtime_and_session_status() {
        let (_workspace, _sessions, app) = app_fixture().await;
        let session_id = app.info.session_id.clone();

        let content = rendered(&app, 160, MIN_TERMINAL_HEIGHT);

        assert!(content.contains("alpha/gpt-alpha"), "{content}");
        assert!(content.contains("think default"), "{content}");
        assert!(
            content.contains("bash disabled · sandbox unavailable"),
            "{content}"
        );
        assert!(content.contains("tokens 0/0"), "{content}");
        assert!(content.contains(&session_id), "{content}");

        let narrow = rendered(&app, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT);
        assert!(narrow.contains("alpha/gpt-alpha"), "{narrow}");
        assert!(narrow.contains("think default"), "{narrow}");
    }

    /// A running task, ready to drop into `app.tasks` for the panel tests
    /// below.
    fn running_task(id: &str, name: &str, description: &str) -> Task {
        Task {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            status: TaskStatus::Running,
            created_at: Some(chrono::Utc::now()),
            started_at: Some(chrono::Utc::now()),
            ..Task::default()
        }
    }

    /// Layout order top to bottom: transcript, suggestions, composer,
    /// sub-agent panel, status line. With one running task the panel takes
    /// one row, so the composer's bottom border, the panel row, and the
    /// status line are the frame's last three rows, in that order.
    #[tokio::test]
    async fn the_panel_sits_between_the_composer_and_the_last_row_status_line() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.tasks = vec![running_task("t3", "review-auth", "check the callback")];

        let height = 20;
        let rows = screen_rows(&app, 100, height);
        assert_eq!(rows.len(), height as usize);

        let composer_border_row = &rows[(height - 3) as usize];
        let panel_row = &rows[(height - 2) as usize];
        let status_row = &rows[(height - 1) as usize];

        assert!(composer_border_row.contains('─'), "{composer_border_row:?}");
        assert!(
            !composer_border_row.contains("t3"),
            "{composer_border_row:?}"
        );
        assert!(panel_row.contains("t3"), "{panel_row:?}");
        assert!(panel_row.contains("review-auth"), "{panel_row:?}");
        assert!(status_row.contains("alpha/gpt-alpha"), "{status_row:?}");
    }

    /// The composer's empty-input floor is 1 inner row, so the box (with its
    /// two border rows) is 3 rows tall.
    #[tokio::test]
    async fn empty_composer_is_three_rows_tall() {
        let (_workspace, _sessions, app) = app_fixture().await;
        assert_eq!(composer_height(&app, 80, COMPOSER_MIN_INNER_ROWS), 3);
    }

    /// With no queued or running task the panel takes no rows at all, and a
    /// finished task never appears in it.
    #[tokio::test]
    async fn a_finished_task_leaves_no_panel_row() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.tasks = vec![Task {
            id: "t9".into(),
            status: TaskStatus::Succeeded,
            ..Task::default()
        }];

        let height = 20;
        let rows = screen_rows(&app, 100, height);

        // No panel row means the composer's bottom border sits directly
        // above the status line.
        let composer_border_row = &rows[(height - 2) as usize];
        assert!(composer_border_row.contains('─'), "{composer_border_row:?}");
        assert!(!rows.join("\n").contains("t9"));
    }

    /// More than 4 queued/running tasks show only the first 3 and a
    /// `+N more` row for the rest.
    #[tokio::test]
    async fn five_running_tasks_show_three_rows_and_a_more_row() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.tasks = (0..5)
            .map(|i| running_task(&format!("t{i}"), &format!("agent{i}"), "working"))
            .collect();

        let screen = screen_rows(&app, 100, 30).join("\n");

        for i in 0..3 {
            assert!(screen.contains(&format!("t{i}")), "{screen}");
        }
        for i in 3..5 {
            assert!(!screen.contains(&format!("t{i}")), "{screen}");
        }
        assert!(screen.contains("+2 more"), "{screen}");
    }

    /// At the minimum terminal size (40x8) with four running tasks, the rows
    /// are transcript 1, composer 3, panel 3, status 1: the panel gives up
    /// rows before the composer shrinks below its 1-row minimum.
    #[tokio::test]
    async fn the_minimum_size_keeps_the_transcript_row_and_the_status_line() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.tasks = (1..=4)
            .map(|i| running_task(&format!("t{i}"), "review", "check the diff"))
            .collect();

        let width = MIN_TERMINAL_WIDTH;
        let height = MIN_TERMINAL_HEIGHT;
        let rows = screen_rows(&app, width, height);
        assert_eq!(rows.len(), height as usize);

        // Row 0 is the transcript; the composer's titled top border is row 1
        // and its bottom border row 3, so it kept 1 inner row.
        assert!(rows[1].contains("Otto"), "{rows:?}");
        assert!(
            rows[3].contains('─') && !rows[3].contains("Otto"),
            "{rows:?}"
        );
        assert!(rows[4].contains("t1"), "{rows:?}");
        assert!(rows[5].contains("t2"), "{rows:?}");
        assert!(rows[6].contains("t3"), "{rows:?}");
        assert!(!rows.join("\n").contains("t4"), "{rows:?}");
        // The footer text is longer than this width, but it still starts
        // with the profile/model field, so this confirms the status line
        // landed on the frame's last row rather than being pushed off it.
        assert!(rows[7].contains("alpha/gpt-alpha"), "{rows:?}");
    }

    /// An unnamed task is labeled by its agent, and by `default` when it has
    /// no agent either.
    #[test]
    fn an_unnamed_panel_row_falls_back_to_the_agent() {
        let text = |task: &Task| -> String {
            panel_row_line(task, chrono::Utc::now(), 80, panel_columns(&[task]))
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        let mut task = running_task("t1", "", "check the diff");
        task.agent = "reviewer".into();
        assert!(text(&task).contains("reviewer ?"), "{}", text(&task));
        task.agent.clear();
        assert!(text(&task).contains("default ?"), "{}", text(&task));
    }

    #[test]
    fn the_panel_shows_the_model_and_live_input_output_tokens() {
        let mut task = running_task("t1", "review", "check the diff");
        task.model = "gpt-5.5".into();
        task.usage_present = true;
        task.usage.input_tokens = 12_310;
        task.usage.output_tokens = 842;

        let line = line_text(&panel_row_line(
            &task,
            chrono::Utc::now(),
            100,
            panel_columns(&[&task]),
        ));

        assert!(line.contains("gpt-5.5 in: 12.3k out:   842"), "{line}");
    }

    #[test]
    fn panel_columns_align_across_different_name_lengths() {
        let mut short = running_task("t1", "a", "first description");
        short.model = "gpt-5.5".into();
        short.usage_present = true;
        short.usage.input_tokens = 10;
        short.usage.output_tokens = 2;
        let mut long = running_task("t2", "much-longer-name", "second description");
        long.model = "openai-compatible/model-alpha".into();
        long.usage_present = true;
        long.usage.input_tokens = 12_310;
        long.usage.output_tokens = 842;

        let columns = panel_columns(&[&short, &long]);
        let short = line_text(&panel_row_line(&short, chrono::Utc::now(), 120, columns));
        let long = line_text(&panel_row_line(&long, chrono::Utc::now(), 120, columns));

        assert!(long.contains("much-longer-name"), "{long}");
        assert!(long.contains("openai-compatible/model-alpha"), "{long}");
        assert_eq!(
            short.find("gpt-5.5"),
            long.find("openai-compatible/model-alpha")
        );
        assert_eq!(short.find("in:"), long.find("in:"));
        assert_eq!(short.find("out:"), long.find("out:"));
        assert_eq!(
            short.find("first description"),
            long.find("second description")
        );
    }

    /// The sub-agent panel needs the idle loop's 1-second redraw timer iff at
    /// least one task is queued or running; a registry with nothing pending,
    /// or only finished tasks, needs no timer.
    #[test]
    fn needs_task_clock_only_with_a_queued_or_running_task() {
        assert!(!needs_task_clock(&[]));
        assert!(!needs_task_clock(&[Task {
            status: TaskStatus::Succeeded,
            ..Task::default()
        }]));
        assert!(needs_task_clock(&[Task {
            status: TaskStatus::Queued,
            ..Task::default()
        }]));
        assert!(needs_task_clock(&[Task {
            status: TaskStatus::Running,
            ..Task::default()
        }]));
    }

    /// No-panic smoke test at extreme terminal sizes. Ratatui's `Buffer` makes
    /// staying within bounds structurally true (see this test module's doc
    /// comment above), so what is left to check is that drawing at these sizes
    /// does not panic.
    #[tokio::test]
    async fn draw_does_not_panic_at_extreme_terminal_sizes() {
        let (_workspace, _sessions, app) = app_fixture().await;

        for (width, height) in [(1u16, 1u16), (2, 1), (10, 3), (39, 7)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| draw(frame, &app))
                .expect("draw at extreme size");
        }
    }

    /// Confirms Otto's own transcript text — a run of wide (CJK/emoji)
    /// characters and a long unbroken ASCII token with no wrap points — feeds
    /// into `Paragraph::wrap` without panicking at a narrow width. This does
    /// not re-test ratatui's own wrapping algorithm, only that Otto's text
    /// reaches it intact.
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

    /// The view half of command completion: a `/` prefix lists the matching
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
        assert!(
            highlighted[0].trim_start().starts_with("/sandbox"),
            "{highlighted:?}"
        );
    }

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

        // One line of input fills the composer's 1-row minimum, so the box is
        // 3 rows (1 inner + 2 borders) and the caret sits on the
        // first inner row, `composer height` rows above the bottom of the
        // frame (the status line takes the last row; there is no panel row).
        assert_eq!((x, y), (SIDE_MARGIN + 1 + 6, 20 - 3));
    }

    /// A hard line break moves the caret to the next composer row.
    #[tokio::test]
    async fn the_caret_follows_an_embedded_newline() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.input = "ab\ncd".chars().collect();
        app.cursor = app.input.len();

        let (x, y) = cursor(&app, 100, 20);

        // Two lines grow the composer to 4 rows including borders; the caret
        // follows to the second inner row.
        assert_eq!((x, y), (SIDE_MARGIN + 1 + 2, 20 - 4 + 1));
    }

    /// Past [`INPUT_BOX_THRESHOLD`] rows the composer stops growing, so it
    /// has to scroll: the tail of the value and the caret both stay inside
    /// the box instead of the first rows being pinned and the caret
    /// wandering onto the border.
    #[tokio::test]
    async fn a_value_taller_than_the_composer_scrolls_its_tail_into_view() {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.input = format!("{}END", "x".repeat(96 * 13)).chars().collect();
        app.cursor = app.input.len();

        let height = 30;
        let content = rendered(&app, 100, height);
        let (x, y) = cursor(&app, 100, height);

        assert!(content.contains("END"), "{content}");
        // The box is `INPUT_BOX_THRESHOLD` text rows plus two borders. With
        // no panel row, the composer's bottom border sits directly above the
        // status line (the frame's last row), so its last text row is 3 rows
        // above the bottom of the frame.
        assert_eq!((x, y), (SIDE_MARGIN + 1 + 3, height - 3));
    }
}
