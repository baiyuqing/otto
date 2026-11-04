//! Read-only, session-scoped child transcript presentation. No task controls.
use std::cell::{Cell, RefCell};

use crossterm::event::{KeyCode, KeyEvent};

pub(crate) mod bindings;
use crate::app::tasks::{ObserverEntryKind, ObserverSnapshot};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::{
    entries::{Entry, EntryKind},
    layout::escape_single_line_text,
    transcript,
};
use crate::subagent::record::TaskRow;

#[derive(Default)]
pub(super) struct Observer {
    pub active: bool,
    pub bindings: bindings::Bindings,
    pub panes: Vec<Pane>,
    pub error: Option<String>,
    truncated: bool,
    focused: usize,
    scope: (String, String),
    geometry: RefCell<Vec<(usize, Rect)>>,
}

pub(super) struct Pane {
    pub row: TaskRow,
    entries: Vec<Entry>,
    pub loaded: bool,
    pub missing: bool,
    /// None follows the tail; Some pins an absolute rendered history row.
    scroll: Option<u16>,
    max_scroll: Cell<u16>,
    page_rows: Cell<u16>,
}

impl Observer {
    pub fn reset_scope(&mut self, workspace: &str, session: &str) {
        let scope = (workspace.to_owned(), session.to_owned());
        if self.scope != scope {
            let active = self.active;
            let bindings = self.bindings.clone();
            *self = Self {
                active,
                bindings,
                scope,
                ..Self::default()
            };
        }
    }

    pub fn rows(&mut self, rows: Vec<TaskRow>) {
        for row in rows {
            if row.parent_session != self.scope.1 || row.workspace != self.scope.0 {
                continue;
            }
            if let Some(pane) = self
                .panes
                .iter_mut()
                .find(|pane| pane.row.task_id == row.task_id)
            {
                pane.row = row;
            } else {
                self.panes.push(Pane {
                    row,
                    entries: Vec::new(),
                    loaded: false,
                    missing: false,
                    scroll: None,
                    max_scroll: Cell::new(0),
                    page_rows: Cell::new(1),
                });
            }
        }
        // Retain completed panes, including rows omitted by a later live listing.
        self.error = None;
    }

    pub fn snapshot(&mut self, snapshot: ObserverSnapshot) {
        self.truncated = snapshot.truncated;
        for task in snapshot.tasks {
            if task.row.workspace != self.scope.0 || task.row.parent_session != self.scope.1 {
                continue;
            }
            let id = task.row.task_id.clone();
            self.rows(vec![task.row]);
            let Some(pane) = self.panes.iter_mut().find(|pane| pane.row.task_id == id) else {
                continue;
            };
            pane.entries = task
                .entries
                .into_iter()
                .filter_map(|entry| {
                    let kind = match entry.kind {
                        ObserverEntryKind::Assistant => EntryKind::Assistant,
                        ObserverEntryKind::Tool => EntryKind::Tool,
                        ObserverEntryKind::Error => EntryKind::Error,
                        ObserverEntryKind::System => return None,
                    };
                    Some(Entry {
                        kind: Some(kind),
                        raw: entry.text,
                        tool_name: escape_single_line_text(&entry.tool_name),
                        tool_args: entry.tool_args,
                        tool_output: entry.tool_output,
                        tool_done: entry.tool_done,
                        tool_error: entry.tool_error,
                        ..Entry::default()
                    })
                })
                .collect();
            pane.loaded = true;
            pane.missing = task.missing;
        }
        self.error = None;
    }

    /// Called only after approval/feature surfaces have had first refusal.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if self.bindings.toggle(key) {
            self.active = !self.active;
            return true;
        }
        if self.bindings.open(key) {
            self.active = true;
            return true;
        }
        if !self.active {
            return false;
        }
        if self.bindings.back(key) {
            self.active = false;
            return true;
        }
        if !self.panes.is_empty() {
            match key.code {
                KeyCode::Tab => self.focused = (self.focused + 1) % self.panes.len(),
                KeyCode::BackTab => {
                    self.focused = (self.focused + self.panes.len() - 1) % self.panes.len()
                }
                KeyCode::Up => self.scroll_by(-1),
                KeyCode::Down => self.scroll_by(1),
                KeyCode::PageUp => {
                    self.scroll_by(-(self.panes[self.focused].page_rows.get() as i32))
                }
                KeyCode::PageDown => {
                    self.scroll_by(self.panes[self.focused].page_rows.get() as i32)
                }
                KeyCode::Home => self.panes[self.focused].scroll = Some(0),
                KeyCode::End => self.panes[self.focused].scroll = None,
                _ => {}
            }
        }
        true // Never forward typing, submission or task-control keys to the composer.
    }

    pub fn focus_at(&mut self, col: u16, row: u16) {
        let found = self
            .geometry
            .borrow()
            .iter()
            .find(|(_, rect)| rect.contains(Position::new(col, row)))
            .map(|(index, _)| *index);
        if let Some(index) = found {
            self.focused = index;
        }
    }

    pub fn scroll_by(&mut self, amount: i32) {
        if let Some(pane) = self.panes.get_mut(self.focused) {
            let max = pane.max_scroll.get();
            let next = pane
                .scroll
                .unwrap_or(max)
                .saturating_add_signed(amount.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
                .min(max);
            pane.scroll = if amount > 0 && next == max {
                None
            } else {
                Some(next)
            };
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, connected: bool) {
        frame.render_widget(Clear, area);
        self.geometry.borrow_mut().clear();
        let header = Rect {
            height: 1.min(area.height),
            ..area
        };
        frame.render_widget(
            Paragraph::new(format!("Sub-agents · read-only · {}", self.bindings.label)),
            header,
        );
        let footer = Rect {
            y: area.bottom().saturating_sub(1),
            height: 1.min(area.height),
            ..area
        };
        let notice_rows = u16::from(self.truncated);
        if self.truncated {
            let notice = Rect {
                y: area.y.saturating_add(1),
                height: 1,
                ..area
            };
            frame.render_widget(
                Paragraph::new("Only recent sub-agents shown; use /agents for older tasks"),
                notice,
            );
        }
        let capacity = (area.height.saturating_sub(2 + notice_rows) / 5).max(1) as usize;
        let page = self.focused / capacity;
        let start = page * capacity;
        let count = self.panes.len().saturating_sub(start).min(capacity);
        let pages = self.panes.len().div_ceil(capacity).max(1);
        frame.render_widget(
            Paragraph::new(format!(
                "Tab/Shift+Tab: panes/pages · PgUp/PgDn: scroll · Home/End · {}/{pages}",
                page + 1
            )),
            footer,
        );
        let content = Rect {
            y: area.y.saturating_add(1 + notice_rows),
            height: area.height.saturating_sub(2 + notice_rows),
            ..area
        };
        if count == 0 {
            let back = &self.bindings.back_label;
            let notice = if !connected {
                format!("Disconnected: return to main ({back}) and reconnect to observe children.")
            } else if let Some(error) = &self.error {
                format!(
                    "Observer unavailable: {}. Return with {back}; reopen to retry.",
                    escape_single_line_text(error)
                )
            } else {
                format!(
                    "No sub-agents. Delegate a task from the main conversation; {back} returns to your draft."
                )
            };
            frame.render_widget(Paragraph::new(notice), content);
            return;
        }
        for (slot, pane) in self.panes.iter().enumerate().skip(start).take(count) {
            let relative = (slot - start) as u16;
            let top = content.height as u32 * relative as u32 / count as u32;
            let bottom = content.height as u32 * (relative + 1) as u32 / count as u32;
            let rect = Rect {
                y: content.y.saturating_add(top as u16),
                height: (bottom - top) as u16,
                ..content
            };
            self.geometry.borrow_mut().push((slot, rect));
            let status = if !connected {
                "disconnected"
            } else {
                &pane.row.status
            };
            let title = escape_single_line_text(&format!(
                "{} · {} · {}",
                pane.row.task_id, status, pane.row.description
            ));
            let block = Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(if slot == self.focused {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let mut lines = transcript::lines(&pane.entries, true, inner.width as usize);
            if lines.is_empty() {
                let notice = if pane.missing {
                    format!(
                        "Child history unavailable. Return with {}; reopen to retry.",
                        self.bindings.back_label
                    )
                } else if !pane.loaded {
                    "Loading child history…".into()
                } else {
                    "Waiting for visible child output…".into()
                };
                lines.push(notice.into());
            }
            if let Some(error) = &self.error {
                lines.push(
                    format!(
                        "Observer unavailable: {} · {} to return",
                        escape_single_line_text(error),
                        self.bindings.back_label
                    )
                    .into(),
                );
            }
            let max = lines
                .len()
                .saturating_sub(inner.height as usize)
                .min(u16::MAX as usize) as u16;
            pane.max_scroll.set(max);
            pane.page_rows.set(inner.height.max(1));
            let top = pane.scroll.unwrap_or(max).min(max);
            frame.render_widget(Paragraph::new(lines).scroll((top, 0)), inner);
        }
    }
}

#[cfg(test)]
mod tests;
