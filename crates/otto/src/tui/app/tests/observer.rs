use super::*;
use crate::app::tasks::ObserverSnapshot;
use crossterm::event::KeyModifiers;
use ratatui::{Terminal, backend::TestBackend};

fn toggle(app: &mut App, remote: &Remote) {
    press(app, remote, KeyCode::F(6));
}

#[test]
fn observer_reads_are_bounded_visible_only_and_preserve_last_good_on_failure() {
    let (mut app, remote) = app();
    app.poll_observer();
    assert!(app.requests.is_empty());
    toggle(&mut app, &remote);
    let (ticket, request) = app.requests.pop().unwrap();
    assert!(matches!(request, Request::ObserverSnapshot));
    for _ in 0..10 {
        app.poll_observer();
    }
    assert!(app.requests.is_empty());
    app.apply_completion(Completion {
        ticket: ticket.clone(),
        mutation: false,
        result: Err("unavailable".into()),
    });
    assert_eq!(app.observer.error.as_deref(), Some("unavailable"));
    toggle(&mut app, &remote);
    app.poll_observer();
    assert!(app.requests.is_empty());
    toggle(&mut app, &remote);
    assert_eq!(app.requests.len(), 1);
    app.connected = false;
    app.requests.clear();
    app.poll_observer();
    assert!(app.requests.is_empty());
}

#[test]
fn observer_rejects_previous_session_completions() {
    let (mut app, remote) = app();
    toggle(&mut app, &remote);
    let (ticket, _) = app.requests.pop().unwrap();
    app.info.session_id = "new-session".into();
    app.poll_observer();
    app.apply_completion(Completion {
        ticket,
        mutation: false,
        result: Err("stale failure".into()),
    });
    assert!(app.observer.error.is_none());
    assert!(app.observer.panes.is_empty());
}

#[test]
fn observer_never_covers_approval_or_steals_its_keys_and_escape_returns_main() {
    let (mut app, remote) = app();
    toggle(&mut app, &remote);
    app.sync_shared_approval(&[approval("observe-approval")]);
    assert!(app.has_surface());
    let surface = app.top_id();
    toggle(&mut app, &remote);
    assert!(app.observer.active);
    assert_eq!(app.top_id(), surface);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::tui::render::draw(frame, &app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..30)
        .map(|y| {
            (0..100)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Confirm"));
    press(&mut app, &remote, KeyCode::Esc);
    assert!(!app.observer.active);
    assert!(!app.has_surface());
    toggle(&mut app, &remote);
    assert!(
        app.has_surface(),
        "reopening must reveal still-pending hidden approval"
    );
}

#[test]
fn observer_ctrl_c_keeps_cancel_semantics() {
    let (mut app, remote) = app();
    app.start_turn();
    toggle(&mut app, &remote);
    assert_eq!(
        app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &Backend::Attach(&remote),
            &CancellationToken::new()
        ),
        Some(Action::CancelTurn)
    );
}

#[test]
fn observer_help_shows_actual_configured_keys() {
    let (mut app, remote) = app();
    app.observer.bindings =
        crate::tui::observer::bindings::Bindings::parse(&otto_core::config::ui::ObserverBindings {
            toggle: "F8".into(),
            open: "Ctrl+Left".into(),
            back: "Ctrl+Right".into(),
        })
        .unwrap();
    app.dispatch_line(
        "/help",
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    let view = app.active_view().unwrap();
    assert!(view.document.contains("F8: toggle"));
    assert!(view.document.contains("Ctrl+Left: open"));
    assert!(!view.document.contains("F6: toggle"));
}

#[test]
fn observer_snapshot_lane_does_not_invalidate_approval_snapshot() {
    let (mut app, remote) = app();
    app.request_approvals();
    let (approval_ticket, _) = app.requests.pop().unwrap();
    toggle(&mut app, &remote);
    let (observer_ticket, _) = app.requests.pop().unwrap();
    app.apply_completion(Completion {
        ticket: observer_ticket,
        mutation: false,
        result: Ok(Reply::ObserverSnapshot(ObserverSnapshot::default())),
    });
    app.apply_completion(Completion {
        ticket: approval_ticket,
        mutation: false,
        result: Ok(Reply::ApprovalSnapshot(otto_core::approval::Snapshot {
            requests: vec![approval("not-lost")],
            cursor: 0,
        })),
    });
    assert!(app.has_surface());
}
