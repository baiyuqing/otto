//! Offline message visibility and lifecycle ordering regressions.
use super::*;
use ratatui::{Terminal, backend::TestBackend};

fn screen(app: &App) -> String {
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
fn periodic_refresh_is_bounded_and_projects_external_full_text_in_both_modes() {
    for attached in [false, true] {
        let (mut app, _) = app();
        app.attached = attached;
        app.poll_steering();
        for _ in 0..50 {
            app.poll_steering();
        }
        assert_eq!(app.requests.len(), 1);
        let ticket = app.requests[0].0.clone();
        let text = "external first line\nexternal complete second line";
        app.apply_completion(Completion {
            ticket,
            mutation: true,
            result: Ok(Reply::Steering(vec![steer(1, text, Status::Queued)])),
        });
        assert_eq!(app.pending_steers[0].text, text);
        let rendered = screen(&app);
        assert!(rendered.contains("external first line"));
        assert!(rendered.contains("external complete second line"));
        app.poll_steering();
        assert_eq!(app.requests.len(), 2);
    }
}

#[test]
fn delivered_snapshot_before_notification_keeps_body_then_replaces_it_once() {
    for attached in [false, true] {
        let (mut app, _) = app();
        app.attached = attached;
        app.apply_steering(vec![steer(1, "unique delivery body", Status::Queued)]);
        app.apply_steering(vec![steer(1, "unique delivery body", Status::Delivered)]);
        assert!(app.queued_steer.is_none());
        assert_eq!(screen(&app).matches("unique delivery body").count(), 1);
        app.apply_steering(vec![]);
        assert_eq!(screen(&app).matches("unique delivery body").count(), 1);
        deliver(&mut app, "unique delivery body");
        app.apply_steering(vec![steer(1, "unique delivery body", Status::Delivered)]);
        assert!(app.pending_steers.is_empty());
        assert_eq!(screen(&app).matches("unique delivery body").count(), 1);
    }
}

#[test]
fn repeated_text_inputs_each_deliver_once_and_notification_can_precede_snapshot() {
    for attached in [false, true] {
        let (mut app, _) = app();
        app.attached = attached;
        app.apply_steering(vec![
            steer(1, "same words", Status::Queued),
            steer(2, "same words", Status::Queued),
        ]);
        deliver(&mut app, "same words");
        app.apply_steering(vec![
            steer(1, "same words", Status::Delivered),
            steer(2, "same words", Status::Queued),
        ]);
        assert_eq!(app.pending_steers.len(), 1);
        assert_eq!(app.pending_steers[0].id, 2);
        deliver(&mut app, "same words");
        app.apply_steering(vec![
            steer(1, "same words", Status::Delivered),
            steer(2, "same words", Status::Delivered),
        ]);
        assert!(app.pending_steers.is_empty());
        assert_eq!(
            app.entries
                .iter()
                .filter(|entry| entry.raw == "same words")
                .count(),
            2
        );
        let before_notification = app.refresh_steering();
        deliver(&mut app, "arrived first");
        app.apply_completion(Completion {
            ticket: before_notification,
            mutation: true,
            result: Ok(Reply::Steering(vec![steer(
                3,
                "arrived first",
                Status::Queued,
            )])),
        });
        assert!(app.pending_steers.is_empty());
        app.apply_steering(vec![steer(3, "arrived first", Status::Delivered)]);
        assert!(app.pending_steers.is_empty());
        // An unrelated old notification must not consume a NEW queued input.
        deliver(&mut app, "future words");
        app.apply_steering(vec![steer(4, "future words", Status::Queued)]);
        assert_eq!(app.pending_steers[0].id, 4);
    }
}

#[test]
fn attached_own_enter_follows_bottom_but_external_arrivals_preserve_history() {
    let (mut app, remote) = app();
    for index in 0..60 {
        app.entries.push(Entry {
            kind: Some(EntryKind::User),
            raw: format!("historical row {index}"),
            ..Default::default()
        });
    }
    screen(&app);
    press(&mut app, &remote, KeyCode::PageUp);
    let historical_top = app.scroll;
    assert!(historical_top.is_some());
    deliver(&mut app, "external message below");
    assert_eq!(app.scroll, historical_top);
    let rendered = screen(&app);
    assert!(rendered.contains("NEW MESSAGES BELOW"));
    assert!(rendered.contains("Ctrl+End"));
    app.handle_key(
        KeyEvent::new(KeyCode::End, crossterm::event::KeyModifiers::CONTROL),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    assert!(app.scroll.is_none());
    assert!(screen(&app).contains("external message below"));
    press(&mut app, &remote, KeyCode::PageUp);
    app.composer.set_input("my own visible prompt");
    assert!(matches!(
        app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
        Some(Action::Prompt(_))
    ));
    assert!(app.scroll.is_none());
    deliver(&mut app, "my own visible prompt");
    assert!(screen(&app).contains("my own visible prompt"));
    assert!(!screen(&app).contains("NEW MESSAGES BELOW"));
}

#[test]
fn local_busy_same_text_input_is_not_an_optimistic_idle_echo() {
    let (mut app, remote) = app();
    app.attached = false;
    app.dispatch_line(
        "same local words",
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    deliver(&mut app, "same local words");
    assert_eq!(app.entries.len(), 1);
    app.start_turn();
    app.composer.set_input("same local words");
    app.submit_input(&Backend::Attach(&remote), &CancellationToken::new());
    app.apply_steering(vec![steer(1, "same local words", Status::Queued)]);
    app.apply_steering(vec![steer(1, "same local words", Status::Delivered)]);
    deliver(&mut app, "same local words");
    assert_eq!(
        app.entries
            .iter()
            .filter(|entry| entry.raw == "same local words")
            .count(),
        2
    );
    assert!(app.pending_steers.is_empty());
}

#[test]
fn session_switch_clears_delivery_ids_and_unread_state() {
    let (mut app, _) = app();
    app.apply_steering(vec![steer(1, "old session", Status::Queued)]);
    deliver(&mut app, "old session");
    app.new_messages = true;
    let mut info = app.info.clone();
    info.session_id.push_str("-switched");
    app.apply_receipt(
        OperationReceipt {
            message: String::new(),
            info,
            history: Some(vec![]),
            prompt: None,
            draft: None,
        },
        false,
    );
    assert!(app.pending_steers.is_empty());
    assert!(app.delivered_steers.is_empty());
    assert!(!app.new_messages);
    app.apply_steering(vec![steer(1, "new session", Status::Queued)]);
    assert_eq!(app.pending_steers[0].text, "new session");
}

#[test]
fn same_session_history_reload_preserves_scroll_and_replaces_pending_body() {
    let history = vec![otto::fixtures::user("loaded delivery")];
    let remote = Remote::offline(history);
    let mut app = App::new(&Backend::Attach(&remote));
    app.entries.clear();
    app.scroll = Some(3);
    app.apply_steering(vec![steer(1, "loaded delivery", Status::Queued)]);
    app.refresh(&Backend::Attach(&remote));
    assert_eq!(app.scroll, Some(3));
    assert!(app.new_messages);
    app.apply_steering(vec![steer(1, "loaded delivery", Status::Delivered)]);
    assert!(app.pending_steers.is_empty());
    assert_eq!(screen(&app).matches("loaded delivery").count(), 1);
}

#[test]
fn overlapping_approval_poll_is_applied_but_invalidated_poll_is_ignored() {
    let (mut app, _) = app();
    app.request_approvals();
    let ticket = app.requests.last().unwrap().0.clone();
    app.poll_steering();
    app.apply_completion(Completion {
        ticket,
        mutation: false,
        result: Ok(Reply::ApprovalSnapshot(otto_core::approval::Snapshot {
            requests: vec![approval("visible")],
            cursor: 1,
        })),
    });
    assert!(app.active_view().is_some());
    let (mut app, _) = super::app();
    app.request_approvals();
    let ticket = app.requests.last().unwrap().0.clone();
    app.invalidate_reads();
    app.apply_completion(Completion {
        ticket,
        mutation: false,
        result: Ok(Reply::ApprovalSnapshot(otto_core::approval::Snapshot {
            requests: vec![approval("stale")],
            cursor: 1,
        })),
    });
    assert!(app.active_view().is_none());
}

#[test]
fn queued_external_arrival_preserves_scroll_and_sets_jump_hint() {
    let (mut app, _) = app();
    app.scroll = Some(7);
    app.apply_steering(vec![steer(1, "externally admitted", Status::Queued)]);
    assert_eq!(app.scroll, Some(7));
    assert!(screen(&app).contains("NEW MESSAGES BELOW"));
}

#[test]
fn superseded_and_previous_session_reads_cannot_repaint_and_failed_poll_recovers() {
    let (mut app, _) = app();
    let old = app.refresh_steering();
    app.invalidate_reads();
    let current = app.refresh_steering();
    app.apply_completion(Completion {
        ticket: old,
        mutation: true,
        result: Ok(Reply::Steering(vec![steer(1, "stale", Status::Queued)])),
    });
    assert!(app.pending_steers.is_empty());
    assert_eq!(app.steering_generation, Some(current.generation));
    let mut previous_session = current.clone();
    previous_session.session.push_str("-previous");
    app.apply_completion(Completion {
        ticket: previous_session,
        mutation: true,
        result: Ok(Reply::Steering(vec![steer(
            2,
            "wrong session",
            Status::Queued,
        )])),
    });
    assert!(app.pending_steers.is_empty());
    app.apply_completion(Completion {
        ticket: current,
        mutation: true,
        result: Err("offline failure".into()),
    });
    assert!(app.steering_generation.is_none());
    app.poll_steering();
    assert!(app.steering_generation.is_some());
}
