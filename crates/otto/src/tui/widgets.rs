//! Shared rendering for interactive TUI surfaces.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, Row as TableRow, Table, TableState, Wrap,
};
use unicode_width::UnicodeWidthStr;

use super::input::hints;
use super::interaction::{Bar, Focus, SegmentKind, State, Timeline, View};
use super::layout::{escape_plain_text, escape_single_line_text};
use super::render::queued_banner_style;

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const ERROR: Color = Color::LightRed;
const SELECTED: Style = Style::new()
    .bg(Color::DarkGray)
    .add_modifier(Modifier::BOLD);

/// Narrowest panel, border included, that draws the timeline bars.
const TIMELINE_MIN_PANEL: u16 = 80;

/// Truncates `text` to `width` display columns with an ellipsis, or pads it.
fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    let overflow = UnicodeWidthStr::width(text) > width;
    let limit = if overflow {
        width.saturating_sub(1)
    } else {
        width
    };
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > limit {
            break;
        }
        out.push(ch);
        used += w;
    }
    if overflow {
        out.push('…');
        used += 1;
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

/// Second marks along the time axis, one every `step` seconds, where the step
/// is the smallest of a fixed set that leaves 8 columns between marks.
fn axis_line(timeline: &Timeline, width: usize) -> String {
    let total = (timeline.end_ms - timeline.start_ms).max(1) as f64;
    let mut line = vec![' '; width];
    let step = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1800, 3600]
        .into_iter()
        .find(|step| (*step as f64 * 1000.0 / total) * width as f64 >= 8.0)
        .unwrap_or(3600);
    let mut second = 0;
    while (second * 1000) as f64 <= total {
        let column = ((second * 1000) as f64 / total * width as f64) as usize;
        for (i, ch) in format!("{second}s").chars().enumerate() {
            if let Some(cell) = line.get_mut(column + i) {
                *cell = ch;
            }
        }
        second += step;
    }
    line.into_iter().collect()
}

/// One bar of `width` cells; a later segment overwrites an earlier one, and
/// a segment shorter than a cell still fills one.
fn bar_spans(timeline: &Timeline, bar: &Bar, width: usize) -> Vec<Span<'static>> {
    let total = (timeline.end_ms - timeline.start_ms).max(1) as f64;
    let base = if bar.failed {
        Style::default().fg(Color::Red)
    } else if bar.estimated {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };
    (0..width)
        .map(|cell| {
            let from = timeline.start_ms as f64 + total * cell as f64 / width as f64;
            let to = timeline.start_ms as f64 + total * (cell + 1) as f64 / width as f64;
            let kind = bar
                .segments
                .iter()
                .rev()
                .find(|s| (s.start_ms as f64) < to && (s.end_ms as f64) > from)
                .map(|s| s.kind);
            let (glyph, style) = match kind {
                None => (' ', base),
                Some(SegmentKind::Waiting) => ('░', base),
                Some(SegmentKind::RetryWait) => ('▒', base),
                Some(SegmentKind::ApprovalWait) => ('◇', base.fg(Color::Yellow)),
                Some(SegmentKind::Output) => ('█', base),
            };
            Span::styled(glyph.to_string(), style)
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub page_rows: u16,
    pub max_scroll: u16,
    pub cursor: Option<(u16, u16)>,
    pub rows: Option<Rect>,
    pub document: Option<Rect>,
    pub fields: Vec<(String, Rect)>,
    pub actions: Vec<(String, Rect)>,
}

pub(crate) fn draw_surface(frame: &mut Frame, area: Rect, view: &View, state: &State) -> Geometry {
    let header_height = if area.height < 12 && view.notice.is_none() {
        2
    } else {
        3
    }
    .min(area.height);
    let footer_wanted =
        1 + u16::from(
            view.actions
                .iter()
                .any(|a| !a.enabled && a.reason.is_some()),
        ) + u16::from(state.pending)
            + 2; // border, one horizontal action row, and generic key hints
    let footer_height = footer_wanted.min(area.height.saturating_sub(header_height).max(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(1),
            Constraint::Length(footer_height),
        ])
        .split(area);

    let title = if view.description.is_empty() {
        escape_single_line_text(&view.title)
    } else {
        format!(
            "{}  {}",
            escape_single_line_text(&view.title),
            escape_single_line_text(&view.description)
        )
    };
    let mut title_line = vec![Span::styled(
        title,
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    if let Some(error) = state.error.as_ref().or(view.error.as_ref()) {
        title_line.push(Span::styled(
            format!("  ! {error}"),
            Style::default().fg(ERROR),
        ));
    } else if view.loading {
        title_line.push(Span::styled("  Loading…", Style::default().fg(MUTED)));
    }
    let mut header = vec![Line::from(title_line)];
    if let Some(notice) = &view.notice {
        header.push(Line::from(Span::styled(
            escape_single_line_text(notice),
            queued_banner_style(),
        )));
    }
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(ACCENT)),
        ),
        chunks[0],
    );

    let body = chunks[1];
    let has_rows = !view.rows.is_empty();
    let selected_detail = view
        .rows
        .iter()
        .find(|row| state.selected_row.as_deref() == Some(row.id.as_str()))
        .map(|row| row.detail.as_str())
        .filter(|detail| !detail.is_empty());
    let document = if view.document.is_empty() {
        selected_detail.unwrap_or_default()
    } else {
        view.document.as_str()
    };
    let has_detail = !document.is_empty() || !view.fields.is_empty();
    let mut geometry = Geometry::default();

    // A timeline needs every row and its axis visible; the detail takes the rest.
    let rows_constraint = if view.timeline.is_some() {
        Constraint::Length(
            (view.rows.len() as u16 + 1)
                .min(body.height.saturating_sub(5))
                .max(1),
        )
    } else {
        Constraint::Percentage(35)
    };
    let (rows_area, detail_area) = if has_rows && has_detail && body.height >= 5 {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([rows_constraint, Constraint::Min(1)])
            .split(body);
        (Some(parts[0]), Some(parts[1]))
    } else if has_rows || (view.collection && !has_detail) {
        (Some(body), None)
    } else if has_detail {
        (None, Some(body))
    } else {
        (None, None)
    };

    if let Some(rect) = rows_area {
        geometry.rows = Some(rect);
        let selected = view
            .rows
            .iter()
            .position(|r| state.selected_row.as_deref() == Some(r.id.as_str()));
        if !view.columns.is_empty() {
            let headers = TableRow::new(
                view.columns
                    .iter()
                    .map(|column| escape_single_line_text(column)),
            );
            let rows = view.rows.iter().map(|row| {
                TableRow::new(row.cells.iter().map(|cell| escape_single_line_text(cell)))
            });
            let widths = view
                .columns
                .iter()
                .map(|_| Constraint::Min(8))
                .collect::<Vec<_>>();
            let focused = state.focus == Focus::Rows;
            let header_style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
            frame.render_stateful_widget(
                Table::new(rows, widths)
                    .header(headers.style(header_style))
                    .row_highlight_style(SELECTED)
                    .highlight_symbol(if focused { "› " } else { "  " }),
                rect,
                &mut TableState::default().with_selected(selected),
            );
        } else {
            // Bars need a panel of at least TIMELINE_MIN_PANEL columns and a
            // row for the axis; narrower panels show the tree only.
            let wide = view
                .timeline
                .as_ref()
                .filter(|_| rect.width + 2 >= TIMELINE_MIN_PANEL && rect.height > 1);
            let label_width = usize::from(rect.width.saturating_sub(2)) * 2 / 5;
            let bar_width =
                usize::from(rect.width.saturating_sub(2)).saturating_sub(label_width + 1);
            let rect = match wide {
                Some(timeline) => {
                    let axis = format!(
                        "  {} {}",
                        " ".repeat(label_width),
                        axis_line(timeline, bar_width)
                    );
                    frame.render_widget(
                        Paragraph::new(axis).style(Style::default().fg(MUTED)),
                        Rect { height: 1, ..rect },
                    );
                    Rect {
                        y: rect.y + 1,
                        height: rect.height - 1,
                        ..rect
                    }
                }
                None => rect,
            };
            geometry.rows = Some(rect);
            let items: Vec<ListItem> = view
                .rows
                .iter()
                .map(|row| {
                    let selected = state.selected_row.as_deref() == Some(row.id.as_str());
                    let focused = selected && state.focus == Focus::Rows;
                    let mut spans = vec![Span::styled(
                        if focused { "› " } else { "  " },
                        if focused {
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    )];
                    if let Some(timeline) = wide {
                        let label = row.cells.first().map_or("", String::as_str);
                        spans.push(Span::raw(fit(&escape_single_line_text(label), label_width)));
                        spans.push(Span::raw(" "));
                        if let Some(bar) = timeline.bars.get(&row.id) {
                            spans.extend(bar_spans(timeline, bar, bar_width));
                        }
                    } else {
                        for (i, cell) in row.cells.iter().enumerate() {
                            if i > 0 {
                                spans.push(Span::styled("  ", Style::default().fg(MUTED)));
                            }
                            spans.push(Span::raw(escape_single_line_text(cell)));
                        }
                    }
                    let mut lines = vec![Line::from(spans)];
                    if selected && !row.detail.is_empty() && view.document != row.detail {
                        let preview =
                            escape_single_line_text(row.detail.lines().next().unwrap_or(""));
                        lines.push(Line::from(Span::styled(
                            format!("   {preview}"),
                            Style::default().fg(MUTED),
                        )));
                    }
                    ListItem::new(lines).style(if selected { SELECTED } else { Style::default() })
                })
                .collect();
            frame.render_stateful_widget(
                List::new(items).highlight_style(SELECTED),
                rect,
                &mut ListState::default().with_selected(selected),
            );
        }
        geometry.page_rows = rect.height.max(1);
    }

    if let Some(rect) = detail_area {
        let content = rect;
        let sections = match (document.is_empty(), view.fields.is_empty()) {
            (false, false) if content.height >= 4 => Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(55), Constraint::Min(1)])
                .split(content)
                .to_vec(),
            _ => vec![content],
        };
        let mut index = 0;
        if !document.is_empty() {
            let rect = sections[index];
            index += 1;
            geometry.document = Some(rect);
            let visible = rect.height.saturating_sub(2).max(1);
            geometry.page_rows = visible;
            let document = escape_plain_text(document);
            let paragraph = Paragraph::new(document).wrap(Wrap { trim: false });
            let total_rows = paragraph
                .line_count(rect.width.saturating_sub(2))
                .min(u16::MAX as usize) as u16;
            geometry.max_scroll = total_rows.saturating_sub(visible);
            if state.focus == Focus::Rows {
                geometry.page_rows = rows_area.map_or(visible, |rows| rows.height.max(1));
            }
            frame.render_widget(
                paragraph
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Document")
                            .border_style(Style::default().fg(MUTED)),
                    )
                    .scroll((state.scroll.min(geometry.max_scroll), 0)),
                rect,
            );
        }
        if !view.fields.is_empty() {
            let rect = sections[index];
            let row_height = (rect.height / view.fields.len().max(1) as u16).max(1);
            let mut lines = Vec::new();
            for (i, field) in view.fields.iter().enumerate() {
                let field_rect = Rect {
                    y: rect.y.saturating_add(row_height.saturating_mul(i as u16)),
                    height: if i + 1 == view.fields.len() {
                        rect.bottom().saturating_sub(
                            rect.y.saturating_add(row_height.saturating_mul(i as u16)),
                        )
                    } else {
                        row_height
                    },
                    ..rect
                };
                geometry.fields.push((field.id.clone(), field_rect));
                if state.focus == Focus::Field(field.id.clone()) {
                    geometry.page_rows = field_rect.height.max(1);
                }
                let raw_value = state.drafts.get(&field.id).unwrap_or(&field.value);
                let value = escape_plain_text(raw_value);
                let focused = state.focus == Focus::Field(field.id.clone());
                let label = Line::from(Span::styled(
                    format!(
                        "{}{}",
                        if focused { "› " } else { "  " },
                        escape_single_line_text(&field.label)
                    ),
                    Style::default()
                        .fg(if focused { ACCENT } else { MUTED })
                        .add_modifier(Modifier::BOLD),
                ));
                lines.push(label);
                lines.extend(value.lines().map(|line| Line::raw(format!("  {line}"))));
                if let Focus::Field(id) = &state.focus
                    && id == &field.id
                {
                    let cursor = state.field_cursor(id);
                    let prefix = escape_plain_text(&raw_value[..cursor.min(raw_value.len())]);
                    let cursor_line = prefix.matches('\n').count() as u16;
                    let col = UnicodeWidthStr::width(prefix.rsplit('\n').next().unwrap_or(""))
                        .min(u16::MAX as usize) as u16;
                    geometry.cursor = Some((
                        field_rect.x.saturating_add(2 + col),
                        field_rect
                            .y
                            .saturating_add(1 + cursor_line)
                            .min(field_rect.bottom().saturating_sub(1)),
                    ));
                }
            }
            frame.render_widget(
                Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
                rect,
            );
        }
    }

    if !has_rows && !has_detail {
        frame.render_widget(
            Paragraph::new(if view.loading {
                "Loading…"
            } else {
                "No items"
            })
            .style(Style::default().fg(MUTED)),
            body,
        );
    }

    let footer = chunks[2];
    let mut footer_lines = Vec::new();
    let mut action_widgets = Vec::new();
    if !view.actions.is_empty() {
        let action_row_y = footer.y.saturating_add(1);
        let count = view.actions.len().min(u16::MAX as usize) as u16;
        for (i, action) in view.actions.iter().enumerate() {
            let i = i.min(u16::MAX as usize) as u16;
            let x = footer.x + footer.width.saturating_mul(i) / count;
            let next_x = footer.x + footer.width.saturating_mul(i.saturating_add(1)) / count;
            let rect = Rect {
                x,
                y: action_row_y,
                width: next_x.saturating_sub(x),
                height: 1,
            };
            geometry.actions.push((action.id.clone(), rect));
            let selected = state.focus == Focus::Actions
                && state.selected_action.as_deref() == Some(action.id.as_str());
            let label = match action.shortcut {
                Some(key) => format!(
                    "{}[{}] {}",
                    if selected { "› " } else { "  " },
                    key,
                    action.label
                ),
                None => format!("{}{}", if selected { "› " } else { "  " }, action.label),
            };
            let style = if !action.enabled {
                Style::default().fg(MUTED)
            } else if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ACCENT)
            };
            action_widgets.push((rect, escape_single_line_text(&label), style));
        }
        footer_lines.push(Line::default());
    }
    if let Some(reason) = view
        .actions
        .iter()
        .find(|a| !a.enabled && a.reason.is_some())
        .and_then(|a| a.reason.as_deref())
    {
        footer_lines.push(Line::from(Span::styled(
            escape_single_line_text(reason),
            Style::default().fg(MUTED),
        )));
    }
    if state.pending {
        footer_lines.push(Line::from(Span::styled(
            "Working…",
            Style::default().fg(MUTED),
        )));
    }
    let hint = escape_single_line_text(&hints(view, state));
    if !hint.is_empty() {
        footer_lines.push(Line::from(Span::styled(hint, Style::default().fg(MUTED))));
    }
    frame.render_widget(
        Paragraph::new(Text::from(footer_lines)).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(MUTED)),
        ),
        footer,
    );
    for (rect, label, style) in action_widgets {
        frame.render_widget(Paragraph::new(label).style(style), rect);
    }
    geometry
}

#[cfg(test)]
mod tests;
