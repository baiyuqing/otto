use super::*;

#[test]
fn unavailable_recovered_task_is_terminal_not_live() {
    assert!(terminal_status("unavailable"));
    assert!(!terminal_status("running"));
}
use crate::tui::{
    app::{App, Backend},
    attach::Remote,
};
use crossterm::event::KeyModifiers;
use otto::subagent::tasks::observed::{
    ObserverEntry, ObserverEntryKind, ObserverSnapshot, ObserverTask,
};
use ratatui::{Terminal, backend::TestBackend};
use tokio_util::sync::CancellationToken;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn row(id: &str) -> TaskRow {
    TaskRow {
        task_id: id.into(),
        parent_session: "parent".into(),
        workspace: "/work".into(),
        description: format!("description {id}"),
        status: "running".into(),
        ..Default::default()
    }
}
fn output(text: &str) -> ObserverEntry {
    ObserverEntry {
        kind: ObserverEntryKind::Assistant,
        text: text.into(),
        ..Default::default()
    }
}
fn snapshot(id: &str, text: &str) -> ObserverSnapshot {
    ObserverSnapshot {
        tasks: vec![ObserverTask {
            row: row(id),
            entries: vec![output(text)],
            missing: false,
        }],
        ..Default::default()
    }
}
fn observer() -> Observer {
    let mut observer = Observer::default();
    observer.reset_scope("/work", "parent");
    observer.active = true;
    observer
}
fn screen(observer: &Observer, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, height)).unwrap();
    terminal
        .draw(|frame| observer.draw(frame, frame.area(), true))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..100)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn observer_renders_live_assistant_tools_status_and_description_not_internal_entries() {
    let mut observer = observer();
    let mut snapshot = snapshot("child-a", "visible streaming text");
    snapshot.tasks[0].entries.push(ObserverEntry {
        kind: ObserverEntryKind::Tool,
        tool_name: "bash".into(),
        tool_args: "{\"command\":\"echo ok\"}".into(),
        tool_output: "visible result".into(),
        tool_done: true,
        ..Default::default()
    });
    snapshot.tasks[0].entries.push(ObserverEntry {
        kind: ObserverEntryKind::System,
        text: "hidden internal prompt".into(),
        ..Default::default()
    });
    observer.snapshot(snapshot);
    let text = screen(&observer, 22);
    for expected in [
        "Sub-agents",
        "child-a",
        "running",
        "description child-a",
        "visible streaming text",
        "bash",
        "visible result",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(!text.contains("hidden internal"));
    assert!(text.contains("F6: toggle"));
    assert!(text.contains("Enter: toggle"));
}

#[test]
fn observer_tool_title_escapes_terminal_sequences_and_newlines_at_conversion() {
    let mut observer = observer();
    let mut snapshot = snapshot("a", "");
    snapshot.tasks[0].entries.push(ObserverEntry {
        kind: ObserverEntryKind::Tool,
        tool_name: "tool\x1b[31m\x1b]0;title\x07\nnext".into(),
        tool_done: true,
        ..Default::default()
    });
    observer.snapshot(snapshot);
    let expected = "tool\\x1b[31m\\x1b]0;title\\x07\\x0anext";
    assert_eq!(observer.panes[0].entries[1].tool_name, expected);
    let rendered = transcript::lines(&observer.panes[0].entries, true, 98);
    for span in rendered.iter().flat_map(|line| &line.spans) {
        assert!(!span.content.chars().any(char::is_control));
    }
    let text = screen(&observer, 12);
    assert!(text.contains(expected), "{text}");
    assert!(!text.contains('\x1b'));
    assert!(!text.contains('\x07'));
}

#[test]
fn observer_scroll_is_independent_pinned_across_snapshots_and_end_follows() {
    let mut observer = observer();
    // Separate Markdown paragraphs: soft newlines collapse into one short paragraph.
    let lines = (0..50)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    observer.snapshot(snapshot("a", &lines));
    screen(&observer, 12);
    assert!(observer.panes[0].max_scroll.get() > observer.panes[0].page_rows.get());
    observer.key(key(KeyCode::Home));
    assert_eq!(observer.panes[0].scroll, Some(0));
    let pinned = screen(&observer, 12);
    assert!(pinned.contains("line 0"));
    assert!(!pinned.contains("line 49"));
    observer.snapshot(snapshot("a", &format!("{lines}\n\nnew streaming line")));
    assert_eq!(observer.panes[0].scroll, Some(0));
    let updated = screen(&observer, 12);
    assert_eq!(updated, pinned);
    assert!(!updated.contains("new streaming line"));
    observer.key(key(KeyCode::End));
    assert_eq!(observer.panes[0].scroll, None);
    assert!(screen(&observer, 12).contains("new streaming line"));
    observer.scroll_by(-1);
    assert!(observer.panes[0].scroll.is_some());
    observer.rows(vec![row("b")]);
    observer.key(key(KeyCode::Tab));
    assert_eq!(observer.panes[1].scroll, None);
    assert!(observer.panes[0].scroll.is_some());
}

#[test]
fn observer_paginates_stacked_panes_focuses_click_and_retains_completed() {
    let mut observer = observer();
    observer.rows(vec![row("a"), row("b"), row("c")]);
    let first = screen(&observer, 12);
    assert!(first.contains("▾ a"), "{first}");
    assert!(first.contains("▾ b"), "{first}");
    assert!(!first.contains("▾ c"), "{first}");
    observer.focus_at(3, 8);
    assert_eq!(observer.focused, 1);
    observer.key(key(KeyCode::Tab));
    assert!(screen(&observer, 12).contains("▾ c"));
    observer.key(key(KeyCode::BackTab));
    assert_eq!(observer.focused, 1);
    let mut completed = row("a");
    completed.status = "completed".into();
    observer.rows(vec![completed]);
    observer.rows(Vec::new());
    assert_eq!(observer.panes.len(), 3);
    observer.focused = 0;
    let rendered = screen(&observer, 12);
    assert!(rendered.contains("a · completed"), "{rendered}");
}

#[test]
fn observer_terminal_panes_collapse_by_default_and_manual_override_survives_refresh() {
    let mut observer = observer();
    let mut completed = row("done");
    completed.status = "succeeded".into();
    observer.rows(vec![row("live"), completed]);
    assert!(!observer.is_collapsed(0));
    assert!(observer.is_collapsed(1));
    observer.focused = 1;
    observer.key(key(KeyCode::Enter));
    assert!(!observer.is_collapsed(1));
    let mut refreshed = row("done");
    refreshed.status = "failed".into();
    observer.rows(vec![refreshed]);
    assert!(!observer.is_collapsed(1));
    observer.focused = 0;
    observer.key(key(KeyCode::Enter));
    let mut terminal = row("live");
    terminal.status = "interrupted".into();
    observer.rows(vec![terminal]);
    assert!(observer.is_collapsed(0));
    observer.focused = 1;
    for height in [1, 2, 3, 5, 12] {
        let rendered = screen(&observer, height);
        assert!(!rendered.is_empty());
        let geometry = observer.geometry.borrow();
        assert!(geometry.iter().all(|(_, rect)| rect.height >= 1));
        assert!(geometry.iter().all(|(_, rect)| rect.bottom() <= height));
    }
    let rendered = screen(&observer, 12);
    assert!(rendered.contains("done"), "{rendered}");
    let geometry = observer.geometry.borrow();
    assert!(geometry.iter().all(|(_, rect)| rect.height >= 1));
    assert!(geometry.iter().all(|(_, rect)| rect.bottom() <= 11));
    drop(geometry);
    assert!(
        rendered.contains("succeeded") || rendered.contains("failed"),
        "{rendered}"
    );
}

#[test]
fn observer_scope_reset_discards_old_children_and_rejects_foreign_snapshot() {
    let mut observer = observer();
    observer.snapshot(snapshot("a", "secret old child"));
    observer.reset_scope("/work", "different-parent");
    assert!(observer.panes.is_empty());
    observer.snapshot(snapshot("a", "stale reply"));
    assert!(observer.panes.is_empty());
    assert!(screen(&observer, 12).contains("No sub-agents"));
}

#[test]
fn observer_empty_error_missing_and_truncation_have_recovery() {
    let mut observer = observer();
    assert!(screen(&observer, 12).contains("No sub-agents"));
    observer.error = Some("server unavailable".into());
    assert!(screen(&observer, 12).contains("reopen to retry"));
    let mut snapshot = snapshot("a", "");
    snapshot.tasks[0].missing = true;
    snapshot.truncated = true;
    observer.snapshot(snapshot);
    let text = screen(&observer, 12);
    assert!(text.contains("Child history unavailable"));
    assert!(text.contains("Only recent sub-agents shown"));
}

#[test]
fn observer_keys_paste_mouse_preserve_composer_cursor_main_scroll_and_no_commands() {
    let remote = Remote::offline(Vec::new());
    let backend = Backend::Attach(&remote);
    let mut app = App::new(&backend);
    let cancel = CancellationToken::new();
    app.composer.set_input("intact draft");
    app.composer.cursor = 3;
    app.scroll = Some(7);
    app.handle_key(key(KeyCode::F(6)), &backend, &cancel);
    for code in [
        KeyCode::Char('x'),
        KeyCode::Enter,
        KeyCode::Backspace,
        KeyCode::PageDown,
        KeyCode::Home,
    ] {
        assert_eq!(app.handle_key(key(code), &backend, &cancel), None);
    }
    app.insert_text("ignored paste");
    app.scroll_wheel(true);
    app.observer.focus_at(4, 5);
    app.handle_key(key(KeyCode::Esc), &backend, &cancel);
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "intact draft"
    );
    assert_eq!(app.composer.cursor, 3);
    assert_eq!(app.scroll, Some(7));
    assert!(app.drain_actions().is_empty());
}

#[test]
fn observer_terminal_input_dispatch_ignores_paste_and_does_not_select_main_text() {
    use crate::tui::{TuiEvent, apply_input, selection::DragPhase};
    let remote = Remote::offline(Vec::new());
    let backend = Backend::Attach(&remote);
    let mut app = App::new(&backend);
    app.composer.set_input("preserved");
    app.composer.cursor = 4;
    app.scroll = Some(9);
    app.observer.active = true;
    let cancel = CancellationToken::new();
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    for event in [
        TuiEvent::Paste("must not enter composer".into()),
        TuiEvent::Wheel { up: true, steps: 1 },
        TuiEvent::Select {
            phase: DragPhase::Start,
            col: 4,
            row: 3,
        },
        TuiEvent::Select {
            phase: DragPhase::End,
            col: 14,
            row: 3,
        },
    ] {
        assert_eq!(
            apply_input(&mut app, &mut terminal, event, &cancel, &backend).unwrap(),
            None
        );
    }
    assert_eq!(app.composer.input.iter().collect::<String>(), "preserved");
    assert_eq!(app.composer.cursor, 4);
    assert_eq!(app.scroll, Some(9));
    assert!(app.selection.is_none());
}

#[tokio::test]
async fn observer_accepts_controller_snapshot_rows_without_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = otto::fixtures::controller(workspace.path(), sessions.path()).await;
    let registry = controller.subagent_tasks().expect("registry");
    let task = registry
        .add(otto::subagent::tasks::Task::default(), None, None)
        .expect("add task");
    registry.mark_running(&task.id, chrono::Utc::now());
    registry.observe(
        &task.id,
        &otto_core::agent::Event::TextDelta {
            text: "live partial".into(),
        },
    );
    let snapshot = controller.observer_snapshot().expect("snapshot");
    let info = controller.info();
    let mut observer = Observer::default();
    observer.reset_scope(&info.workspace, &info.session_id);
    observer.active = true;
    observer.snapshot(snapshot);
    assert_eq!(observer.panes.len(), 1);
    assert!(screen(&observer, 12).contains("live partial"));
}
