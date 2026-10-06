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
use super::interaction::{Focus, State, View};
use super::layout::{escape_plain_text, escape_single_line_text};

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const ERROR: Color = Color::LightRed;
const SELECTED: Style = Style::new()
    .bg(Color::DarkGray)
    .add_modifier(Modifier::BOLD);

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
    let header_height = if area.height < 12 { 2 } else { 3 }.min(area.height);
    let footer_wanted = view.actions.len().max(1) as u16
        + u16::from(
            view.actions
                .iter()
                .any(|a| !a.enabled && a.reason.is_some()),
        )
        + u16::from(state.pending)
        + 2; // borders and generic key hints
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
    frame.render_widget(
        Paragraph::new(Line::from(title_line)).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(ACCENT)),
        ),
        chunks[0],
    );

    let body = chunks[1];
    let has_collection = !view.rows.is_empty();
    let has_detail = !view.document.is_empty() || !view.fields.is_empty();
    let mut geometry = Geometry::default();
    let (rows_area, detail_area) = if has_collection && has_detail && body.height >= 5 {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(35), Constraint::Min(1)])
            .split(body);
        (Some(parts[0]), Some(parts[1]))
    } else if has_collection {
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
                    for (i, cell) in row.cells.iter().enumerate() {
                        if i > 0 {
                            spans.push(Span::styled("  ", Style::default().fg(MUTED)));
                        }
                        spans.push(Span::raw(escape_single_line_text(cell)));
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
        let sections = match (view.document.is_empty(), view.fields.is_empty()) {
            (false, false) if content.height >= 4 => Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(55), Constraint::Min(1)])
                .split(content)
                .to_vec(),
            _ => vec![content],
        };
        let mut index = 0;
        if !view.document.is_empty() {
            let rect = sections[index];
            index += 1;
            geometry.document = Some(rect);
            let visible = rect.height.saturating_sub(2).max(1);
            geometry.page_rows = visible;
            let document = escape_plain_text(&view.document);
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
                            .title(if state.focus == Focus::Document {
                                "› Document"
                            } else {
                                "Document"
                            })
                            .border_style(Style::default().fg(if state.focus == Focus::Document {
                                ACCENT
                            } else {
                                MUTED
                            })),
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

    if !has_collection && !has_detail && !view.loading {
        frame.render_widget(
            Paragraph::new("No items").style(Style::default().fg(MUTED)),
            body,
        );
    }

    let footer = chunks[2];
    let mut footer_lines = Vec::new();
    let action_row_y = footer.y.saturating_add(1);
    for (i, action) in view.actions.iter().enumerate() {
        let y = action_row_y.saturating_add(i as u16);
        let rect = Rect {
            x: footer.x,
            y,
            width: footer.width,
            height: 1,
        };
        geometry.actions.push((action.id.clone(), rect));
        let selected = state.focus == Focus::Actions
            && state.selected_action.as_deref() == Some(action.id.as_str());
        let label = match action.shortcut {
            Some(key) => format!("› [{}] {}", key, action.label),
            None if selected => format!("› {}", action.label),
            None => format!("  {}", action.label),
        };
        footer_lines.push(Line::from(Span::styled(
            escape_single_line_text(&label),
            if !action.enabled {
                Style::default().fg(MUTED)
            } else if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ACCENT)
            },
        )));
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
    geometry
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::interaction::{Action, Field, Row};

    #[test]
    fn geometry_exposes_collection_detail_field_and_action_targets() {
        let view = View {
            columns: vec!["Record".into()],
            rows: vec![Row {
                id: "one".into(),
                cells: vec!["one".into()],
                detail: String::new(),
            }],
            document: "Selected document ".repeat(20),
            fields: vec![Field {
                id: "name".into(),
                label: "Name".into(),
                value: "Draft".into(),
                ..Field::default()
            }],
            actions: vec![Action {
                id: "save".into(),
                label: "Save".into(),
                enabled: true,
                ..Action::default()
            }],
            ..View::default()
        };
        let state = State::new(&view);
        let backend = TestBackend::new(70, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut geometry = Geometry::default();
        terminal
            .draw(|frame| geometry = draw_surface(frame, frame.area(), &view, &state))
            .expect("draw");
        assert!(geometry.rows.is_some());
        assert!(geometry.document.is_some());
        assert!(geometry.max_scroll > 0);
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(screen.contains("Record"));
        assert_eq!(
            geometry
                .fields
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["name"]
        );
        assert_eq!(
            geometry
                .actions
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["save"]
        );
    }

    #[test]
    fn state_error_keeps_field_draft_and_actions_rendered() {
        let view = View {
            fields: vec![Field {
                id: "name".into(),
                label: "Name".into(),
                value: "Server".into(),
                ..Field::default()
            }],
            actions: vec![Action {
                id: "retry".into(),
                label: "Retry".into(),
                enabled: true,
                ..Action::default()
            }],
            ..View::default()
        };
        let mut state = State::new(&view);
        state.drafts.insert("name".into(), "Local draft".into());
        state.error = Some("Save failed".into());
        let backend = TestBackend::new(70, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                draw_surface(frame, frame.area(), &view, &state);
            })
            .expect("draw");
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(screen.contains("Save failed"));
        assert!(screen.contains("Local draft"));
        assert!(screen.contains("Retry"));
    }

    #[test]
    fn terminal_control_characters_are_rendered_as_visible_escapes() {
        let view = View {
            title: "Review\x1b]52;c;payload\x07".into(),
            document: "Candidate\x1b[2J body".into(),
            fields: vec![Field {
                id: "name".into(),
                label: "Name\x1b".into(),
                value: "Text\x1b[31m".into(),
                ..Field::default()
            }],
            actions: vec![Action {
                id: "save".into(),
                label: "Save\x1b".into(),
                enabled: true,
                ..Action::default()
            }],
            ..View::default()
        };
        let state = State::new(&view);
        let backend = TestBackend::new(70, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                draw_surface(frame, frame.area(), &view, &state);
            })
            .expect("draw");
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(!screen.chars().any(char::is_control), "{screen:?}");
        assert!(screen.contains("\\x1b"), "{screen}");
        assert!(screen.contains("\\x07"), "{screen}");
    }

    #[test]
    fn compact_terminal_keeps_action_control_inside_frame() {
        let view = View {
            actions: vec![Action {
                id: "save".into(),
                label: "Save".into(),
                enabled: true,
                ..Action::default()
            }],
            ..View::default()
        };
        let state = State::new(&view);
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("terminal");
        let mut geometry = Geometry::default();
        terminal
            .draw(|frame| geometry = draw_surface(frame, frame.area(), &view, &state))
            .expect("draw");
        assert_eq!(geometry.actions.len(), 1);
        assert!(geometry.actions[0].1.bottom() <= 8);
    }
}
