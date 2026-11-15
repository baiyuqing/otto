use crossterm::event::KeyCode;
use otto_core::agent::Event;
use otto_core::model::Usage;
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};

use super::*;
use crate::tui::entries::{Entry, EntryKind};
use crate::tui::wire_frame;
use otto::fixtures;

async fn app_fixture() -> (tempfile::TempDir, tempfile::TempDir, App) {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;
    let app = App::new(&crate::tui::app::Backend::Local(&controller));
    (workspace, sessions, app)
}

fn rendered(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| draw(frame, app)).expect("draw");
    format!("{}", terminal.backend())
}

fn approval() -> otto_core::approval::Request {
    otto_core::approval::Request {
        options: Vec::new(),
        schema_version: 1,
        id: "approval-1".into(),
        revision: 1,
        kind: otto_core::approval::Kind::Confirmation,
        authority: otto_core::approval::Authority::Consent,
        scope: otto_core::approval::Scope {
            workspace: "/work".into(),
            session_id: "session".into(),
            task_id: String::new(),
            run_id: String::new(),
        },
        presentation_origin: String::new(),
        subject: otto_core::approval::Subject {
            tool_call_id: String::new(),
            source: "conversation".into(),
            id: "plan".into(),
            version: "1".into(),
            details: "Review the implementation plan".into(),
        },
        title: "Approve implementation".into(),
        reason: "Before editing".into(),
        persistent: false,
        blocking: true,
        created_at: 1,
        expires_at: None,
        state: otto_core::approval::State::Pending,
        decision: None,
        application: otto_core::approval::ApplicationState::NotRequired,
        application_error: String::new(),
        review: None,
    }
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

/// The drawn rows above the status line, joined.
///
/// The status line is the one row whose content a test cannot predict: it
/// shows the workspace's last path component and the session id, which a
/// temporary-directory fixture draws at random. A two-character task id
/// like `t9` therefore occurs there by chance, so an "this id does not
/// appear" assertion may only scan the rows above it. The panel is drawn
/// within that range, so the assertion still catches a panel row.
fn above_status_line(rows: &[String]) -> String {
    rows[..rows.len().saturating_sub(1)].join("\n")
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

mod composer_and_scroll;
mod layout;
mod panels;
mod pinned_rows;
mod queued_input;
mod status_line;
mod task_panel;
mod transcript_bands;
mod transcript_window;
