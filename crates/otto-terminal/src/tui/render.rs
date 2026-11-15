//! Terminal shell composition; shared widgets render feature surfaces.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{App, TurnStatus};
use super::commands::Completion;
use super::layout::{
    INPUT_BOX_THRESHOLD, MIN_TERMINAL_HEIGHT, MIN_TERMINAL_WIDTH, SIDE_MARGIN,
    escape_single_line_text, footer_workspace, format_cache_hit_rate, format_context_percentage,
    format_token_count,
};
use super::transcript;
use otto::deadline::round_to_seconds;
use otto::subagent::format::one_line;
use otto::subagent::tasks::{Task, TaskStatus};

/// Draws one frame.
pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let active = app.active_view().zip(app.surfaces.top());
    if area.width < MIN_TERMINAL_WIDTH || area.height < MIN_TERMINAL_HEIGHT {
        frame.render_widget(
            Paragraph::new(format!(
                "Terminal too small: resize to at least {MIN_TERMINAL_WIDTH}x{MIN_TERMINAL_HEIGHT}."
            ))
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    if app.observer.active {
        app.observer.draw(frame, area, app.connected);
    } else {
        draw_shell(frame, app, area);
    }
    if let Some((view, surface)) = active {
        let panel = surface_panel_area(area);
        frame.render_widget(Clear, panel);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan))
                .style(Style::default().bg(Color::Black)),
            panel,
        );
        let inner = Rect {
            x: panel.x.saturating_add(1),
            y: panel.y.saturating_add(1),
            width: panel.width.saturating_sub(2),
            height: panel.height.saturating_sub(2),
        };
        let geometry = super::widgets::draw_surface(frame, inner, &view, &surface.state);
        *app.geometry.borrow_mut() = geometry;
    }
}

fn draw_shell(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < MIN_TERMINAL_WIDTH || area.height < MIN_TERMINAL_HEIGHT {
        frame.render_widget(
            Paragraph::new(format!(
                "Terminal too small: resize to at least {MIN_TERMINAL_WIDTH}x{MIN_TERMINAL_HEIGHT}."
            ))
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let content_area = side_margin(area);
    let panel_tasks = visible_panel_tasks(&app.tasks);
    let plan = plan_line(&app.info.plan);
    let composer_banner_height = composer_banner_lines(app).len() as u16;
    let (composer_height, panel_height) = composer_and_panel_height(
        app,
        content_area.width,
        content_area.height,
        panel_row_count(panel_tasks.len()) + u16::from(plan.is_some()),
        composer_banner_height,
    );
    let suggestions = app.suggestions();
    let suggestion_height = (suggestions.len() as u16).min(
        content_area
            .height
            .saturating_sub(composer_height + panel_height + composer_banner_height + 2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(suggestion_height),
            Constraint::Length(composer_banner_height),
            Constraint::Length(composer_height),
            Constraint::Length(panel_height),
            Constraint::Length(1),
        ])
        .split(content_area);
    draw_transcript(frame, app, chunks[0]);
    draw_suggestions(frame, app, &suggestions, chunks[1]);
    draw_composer_banner(frame, app, chunks[2]);
    draw_composer(frame, app, chunks[3]);
    let mut panel_area = chunks[4];
    if let Some(plan) = plan
        && panel_area.height > 0
    {
        frame.render_widget(
            Paragraph::new(Line::styled(
                plan,
                Style::default().add_modifier(Modifier::DIM),
            )),
            Rect {
                height: 1,
                ..panel_area
            },
        );
        panel_area.y += 1;
        panel_area.height -= 1;
    }
    draw_panel(frame, &panel_tasks, panel_area);
    draw_footer(frame, app, chunks[5]);
    if let Some(selection) = &app.selection {
        selection.highlight(frame.buffer_mut());
    }
}

fn surface_panel_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).clamp(1, 88);
    let wanted_height = (area.height * 7 / 10).max(8);
    let height = area.height.saturating_sub(2).min(wanted_height).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
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
    composer_banner_rows: u16,
) -> (u16, u16) {
    let budget = height.saturating_sub(
        1 /* transcript */ + composer_banner_rows + 1, /* status line */
    );
    let natural_composer = composer_height(app, width, COMPOSER_MIN_INNER_ROWS);
    if let Some(spare) = budget.checked_sub(natural_composer) {
        return (natural_composer, panel_rows_wanted.min(spare));
    }
    let shrunk_composer = composer_height(app, width, 1).min(budget).max(3);
    (shrunk_composer, 0)
}

const STARTUP_LOGO: &str = "     ____  __  __\n    / __ \\/ /_/ /____\n   / /_/ / __/ __/ __ \\\n   \\____/\\__/\\__/\\____/";

fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width as usize;
    let height = area.height as usize;
    let mut cache = app.transcript_cache.borrow_mut();
    cache.sync(&app.entries, app.show_details, width);
    // An empty transcript shows the logo in place of the cached rows.
    let logo: Vec<Line<'static>> = if cache.row_count() == 0 {
        STARTUP_LOGO
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect()
    } else {
        Vec::new()
    };
    let head_rows = logo.len().max(cache.row_count());
    // Rows after the transcript, rendered fresh each frame because they come
    // from transient state. Pending input is a projection, never a persisted
    // transcript entry. Keep every accepted item visible rather than only the
    // newest recall target.
    let mut trailing: Vec<Line<'static>> = Vec::new();
    let pending = app
        .pending_steers
        .iter()
        .map(|message| {
            let label = if message.status == otto_core::agent::steering::Status::Delivered {
                "Sent to current turn"
            } else {
                "Queued for current turn"
            };
            format!("[{label}] {}", message.text)
        })
        .chain(
            app.queued_input
                .iter()
                .map(|text| format!("[Queued next input] {text}")),
        );
    for text in pending {
        trailing.push(Line::default());
        trailing.extend(transcript::lines(
            &[super::entries::Entry {
                kind: Some(super::entries::EntryKind::User),
                raw: text,
                ..Default::default()
            }],
            false,
            width,
        ));
    }
    if let Some(status) = app.thinking() {
        if head_rows > 0 || !trailing.is_empty() {
            trailing.push(Line::default());
        }
        trailing.push(thinking_line(&status));
    }

    let total_rows = (head_rows + trailing.len()).min(u16::MAX as usize) as u16;
    let bottom = total_rows.saturating_sub(area.height);
    // The scroll keys need the bottom this layout produced to turn
    // "following" into an absolute offset; nothing outside the renderer
    // knows how the entries wrap at this width.
    app.max_scroll.set(bottom);
    let top = app.scroll.map_or(bottom, |top| top.min(bottom)) as usize;
    let window_end = top + height;
    // Only the visible rows are cloned out of the cache.
    let mut window = cache.rows(top..window_end);
    window.extend(logo.into_iter().skip(top).take(height));
    let trailing_needed = window_end.saturating_sub(head_rows.max(top));
    window.extend(
        trailing
            .into_iter()
            .skip(top.saturating_sub(head_rows))
            .take(trailing_needed),
    );
    // No `Wrap`: `transcript` already broke every row to `area.width` so it
    // could keep a gutter marker on each one, and re-wrapping here would
    // split those rows again and lose the alignment.
    frame.render_widget(Paragraph::new(Text::from(window)), area);
}

/// Spinner frames, in order.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long one [`SPINNER_FRAMES`] frame is held in both TUI modes.
pub(super) const SPINNER_FRAME: Duration = Duration::from_millis(100);

/// Refresh against time, without replaying stale frames after a slow operation.
pub(super) fn spinner_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(SPINNER_FRAME);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

fn spinner_frame(elapsed: Duration) -> &'static str {
    let frame = elapsed.as_millis() / SPINNER_FRAME.as_millis();
    SPINNER_FRAMES[(frame % SPINNER_FRAMES.len() as u128) as usize]
}

/// Only animated or time-dependent surfaces need timer-driven redraws.
pub(super) fn needs_animation(app: &App) -> bool {
    app.busy() || app.has_surface() || needs_task_clock(&app.tasks)
}

/// The line shown under the transcript while a turn is in flight: the phase
/// (waiting for the model, reasoning, running a tool, a provider retry).
/// The spinner stays animated, but only tool records display execution time.
fn thinking_line(status: &TurnStatus) -> Line<'static> {
    Line::styled(
        format!("{} {}", spinner_frame(status.turn_elapsed), status.phase),
        Style::default().fg(Color::Magenta),
    )
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        Paragraph::new(footer_text(app)).style(Style::default().add_modifier(Modifier::DIM)),
        area,
    );
}

/// Status and contextual observer discovery precede the session fields.
/// The footer is one row and
/// ratatui clips whatever does not fit, so the fields a reader cannot do
/// without come first and the ones that describe the environment come last:
/// the model id identifies the session, the approval mode and the token
/// totals are what a turn changes, and the context percentage warns before a
/// compaction. Only the tail is lost on a narrow terminal.
fn footer_text(app: &App) -> String {
    let model = escape_single_line_text(&app.info.model);
    let model_label = if model.is_empty() {
        "unknown".to_string()
    } else {
        model
    };
    let mut text = format!(
        "{model_label} | {} | tokens {}/{}",
        if app.info.approval_settings.policy == otto_core::approval::ApprovalPolicy::Never {
            "NEVER"
        } else {
            app.info.approval_mode.label()
        },
        format_token_count(app.usage.input_tokens),
        format_token_count(app.usage.output_tokens)
    );
    if app.cache_breakdown_complete == Some(true) && app.usage.cached_input_tokens > 0 {
        text.push_str(&format!(
            " (cached {}",
            format_token_count(app.usage.cached_input_tokens)
        ));
        if let Some(rate) =
            format_cache_hit_rate(app.usage.cached_input_tokens, app.usage.input_tokens)
        {
            text.push_str(&format!(", {rate}"));
        }
        text.push(')');
    }
    if app.info.context_input_tokens_pending {
        text.push_str(" | ctx ?%");
    } else if app.info.context_input_tokens_present && app.info.context_window > 0 {
        text.push_str(&format!(
            " | ctx {}",
            format_context_percentage(app.info.context_input_tokens, app.info.context_window)
        ));
    }
    let thinking = if app.info.thinking.is_empty() {
        "default".to_string()
    } else {
        escape_single_line_text(&app.info.thinking)
    };
    text.push_str(&format!(
        " | {} | {thinking} | {}",
        app.info.sandbox.status_summary(),
        escape_single_line_text(&footer_workspace(&app.info.workspace))
    ));
    if !app.info.session_id.is_empty() {
        text.push_str(" | ");
        text.push_str(&escape_single_line_text(&app.info.session_id));
    }
    if !app.observer.active
        && app
            .tasks
            .iter()
            .any(|task| matches!(task.status, TaskStatus::Queued | TaskStatus::Running))
    {
        text = format!(
            "Press {} to view sub-agent observer | {text}",
            app.observer.bindings.entry_label
        );
    }
    match &app.status {
        Some(status) => format!("{} | {text}", escape_single_line_text(status)),
        None => text,
    }
}

const QUEUED_BACKGROUND: Color = Color::DarkGray;

/// The command list drawn above the queued banner and composer while the value
/// being typed is a command prefix, with [`super::app::App::suggestion`]'s row
/// highlighted.
///
/// A [`List`] rather than a [`Paragraph`] so that ratatui's own [`ListState`]
/// scrolls the selected row into view when the match list is longer than the
/// rows [`draw`] could give the panel.
fn draw_suggestions(frame: &mut Frame, app: &App, suggestions: &[Completion], area: Rect) {
    if area.height == 0 || suggestions.is_empty() {
        return;
    }
    let items: Vec<ListItem> = suggestions
        .iter()
        .map(|command| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<12}", command.replacement),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    &command.description,
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

pub(super) fn queued_banner_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .bg(QUEUED_BACKGROUND)
        .add_modifier(Modifier::BOLD)
}

fn composer_banner_lines(app: &App) -> Vec<Line<'static>> {
    let style = queued_banner_style();
    let mut lines: Vec<_> = app
        .composer_notices
        .iter()
        .map(|text| ("◆ UPDATED  ", text))
        .chain(
            app.queued_input
                .as_ref()
                .map(|text| ("◆ QUEUED NEXT INPUT  ", text)),
        )
        .chain(
            app.queued_steer
                .as_ref()
                .map(|steer| ("◆ QUEUED FOR CURRENT TURN  ", &steer.text)),
        )
        .map(|(label, text)| {
            Line::from(vec![
                Span::styled(label, style),
                Span::styled(escape_single_line_text(text), style),
            ])
            .style(style)
        })
        .collect();
    if app.new_messages && app.scroll.is_some() {
        lines.insert(
            0,
            Line::styled("◆ NEW MESSAGES BELOW · Ctrl+End to jump to bottom", style),
        );
    }
    lines
}

/// Draws operation feedback and queued text in a fixed, high-contrast band
/// immediately above the composer. Unlike transcript entries, these rows cannot
/// scroll out of view.
fn draw_composer_banner(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(composer_banner_lines(app)).style(queued_banner_style()),
        area,
    );
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
    let (title, style) = if app.busy() && app.queued_steer.is_some() {
        (
            "Queued for current turn · Ctrl+U recall to edit · Esc delivers now",
            Style::default().fg(Color::Cyan),
        )
    } else if app.busy() && app.queued_input.is_some() {
        (
            "Queued next input · Ctrl+U withdraw · Esc cancels turn",
            Style::default().fg(Color::Cyan),
        )
    } else if app.busy() {
        // Attached, typed text is posted as the next turn.
        let title = if app.attached {
            "Working — Enter queues the next turn · Esc cancels turn"
        } else {
            "Working — Enter queues for this turn · Esc cancels turn"
        };
        (title, Style::default().fg(Color::Magenta))
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
/// The one-line plan summary above the sub-agent panel, `Plan 2/5 · <step in
/// progress>`, or `None` with no plan.
pub(crate) fn plan_line(steps: &[otto_core::session::PlanStep]) -> Option<String> {
    use otto_core::session::PlanStatus;
    if steps.is_empty() {
        return None;
    }
    let completed = steps
        .iter()
        .filter(|step| step.status == PlanStatus::Completed)
        .count();
    let mut line = format!("Plan {completed}/{}", steps.len());
    if let Some(step) = steps
        .iter()
        .find(|step| step.status == PlanStatus::InProgress)
    {
        line.push_str(" · ");
        line.push_str(&step.text);
    }
    Some(line)
}

fn panel_row_count(visible_count: usize) -> u16 {
    visible_count.min(4) as u16
}

/// Whether the sub-agent panel needs a periodic redraw to keep its elapsed
/// times current: true iff at least one task is queued or running. Both
/// event loops use this to redraw on the spinner cadence while tasks remain.
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
            let elapsed = now
                .signed_duration_since(started)
                .to_std()
                .unwrap_or_default();
            spinner_frame(elapsed)
        }
        (TaskStatus::Running, None) => SPINNER_FRAMES[0],
        _ => "○",
    }
}

/// `now - started_at` for a running task, `now - created_at` for a queued
/// one, using the shared duration rounding policy.
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

#[cfg(test)]
mod spinner_tests;
#[cfg(test)]
mod tests;
