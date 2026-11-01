//! Regressions for review findings in message visibility reconciliation.
use super::*;
use ratatui::{Terminal, backend::TestBackend};

fn rendered(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(110, 26)).unwrap();
    terminal
        .draw(|frame| crate::tui::render::draw(frame, app))
        .unwrap();
    format!("{}", terminal.backend())
}

fn deliver(app: &mut App, text: &str) {
    app.apply_event(&WireEvent {
        event_type: USER_MESSAGE.into(),
        text: text.into(),
        ..Default::default()
    });
}

#[test]
fn delivered_projection_survives_before_turn_history_until_actual_row() {
    for attached in [false, true] {
        let (mut app, remote) = app();
        app.attached = attached;
        app.apply_steering(vec![steer(1, "foreign turn input", Status::Queued)]);
        app.apply_steering(vec![steer(1, "foreign turn input", Status::Delivered)]);
        app.refresh(&Backend::Attach(&remote));
        assert_eq!(app.pending_steers.len(), 1);
        assert_eq!(rendered(&app).matches("foreign turn input").count(), 1);
        app.apply_steering(vec![]);
        assert_eq!(app.pending_steers.len(), 1);
        let history = Remote::offline(vec![crate::fixtures::user("foreign turn input")]);
        app.refresh(&Backend::Attach(&history));
        assert!(app.pending_steers.is_empty());
        assert!(app.delivered_steers.contains(&1));
        assert_eq!(rendered(&app).matches("foreign turn input").count(), 1);
    }
}

#[test]
fn unrelated_surface_error_preserves_steering_ticket_and_poll_bound() {
    let (mut app, _) = app();
    let ticket = app.refresh_steering();
    let mut unrelated = ticket.clone();
    unrelated.surface = 42;
    app.pending.insert((42, unrelated.generation));
    app.generations.insert(42, unrelated.generation);
    app.apply_completion(Completion {
        ticket: unrelated,
        mutation: false,
        result: Err("surface refresh failed".into()),
    });
    assert_eq!(app.steering_generation, Some(ticket.generation));
    for _ in 0..50 {
        app.poll_steering();
    }
    assert_eq!(app.requests.len(), 1);
    app.apply_completion(Completion {
        ticket,
        mutation: true,
        result: Ok(Reply::Steering(vec![steer(
            1,
            "valid completion",
            Status::Queued,
        )])),
    });
    assert_eq!(app.pending_steers[0].text, "valid completion");
    app.poll_steering();
    assert_eq!(app.requests.len(), 2);
}

#[test]
fn first_observed_delivery_is_visible_once_before_and_after_notification() {
    for attached in [false, true] {
        let (mut app, _) = app();
        app.attached = attached;
        app.apply_steering(vec![steer(1, "between polls body", Status::Delivered)]);
        assert_eq!(rendered(&app).matches("between polls body").count(), 1);
        app.apply_steering(vec![steer(1, "between polls body", Status::Delivered)]);
        assert_eq!(app.pending_steers.len(), 1);
        deliver(&mut app, "between polls body");
        app.apply_steering(vec![steer(1, "between polls body", Status::Delivered)]);
        assert!(app.pending_steers.is_empty());
        assert_eq!(rendered(&app).matches("between polls body").count(), 1);
    }
}

#[test]
fn first_observed_deliveries_match_historical_occurrences_without_losing_repeated_text() {
    for attached in [false, true] {
        let remote = Remote::offline(vec![
            crate::fixtures::user("repeated delivery"),
            crate::fixtures::user("old delivery"),
        ]);
        let mut app = App::new(&Backend::Attach(&remote));
        app.attached = attached;
        let historical = vec![
            steer(1, "repeated delivery", Status::Delivered),
            steer(2, "old delivery", Status::Delivered),
        ];
        app.apply_steering(historical.clone());
        assert!(app.pending_steers.is_empty());
        let mut snapshot = historical;
        snapshot.push(steer(3, "repeated delivery", Status::Delivered));
        app.apply_steering(snapshot.clone());
        assert_eq!(app.pending_steers.len(), 1);
        assert_eq!(app.pending_steers[0].id, 3);
        assert_eq!(rendered(&app).matches("repeated delivery").count(), 2);
        deliver(&mut app, "repeated delivery");
        app.apply_steering(snapshot);
        assert!(app.pending_steers.is_empty());
        assert_eq!(rendered(&app).matches("repeated delivery").count(), 2);
    }
}

#[test]
fn returning_to_bottom_by_page_down_down_or_wheel_clears_arrival_hint() {
    for method in 0..3 {
        let (mut app, _) = app();
        app.max_scroll.set(20);
        app.scroll = Some(19);
        app.new_messages = true;
        match method {
            0 => {
                app.handle_scroll_key(&KeyEvent::new(
                    KeyCode::PageDown,
                    crossterm::event::KeyModifiers::NONE,
                ));
            }
            1 => {
                app.handle_scroll_key(&KeyEvent::new(
                    KeyCode::Down,
                    crossterm::event::KeyModifiers::NONE,
                ));
            }
            _ => app.scroll_wheel(false),
        }
        assert!(app.scroll.is_none());
        assert!(!app.new_messages);
        app.scroll_wheel(true);
        assert!(app.scroll.is_some());
        assert!(!app.new_messages);
    }
}
