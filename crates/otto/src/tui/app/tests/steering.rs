//! Tests for guidance tracking, recall and withdrawal.

use super::*;

/// The run's own read is the only source of the queued guidance: a message
/// that left the queued state disappears without the TUI deciding anything.
#[test]
fn a_steering_read_paints_and_clears_the_tracked_guidance() {
    let (mut app, _) = app();
    app.apply_steering(vec![steer(1, "use the other branch", Status::Queued)]);
    assert_eq!(app.queued_steer.as_ref().map(|steer| steer.id), Some(1));

    // Newest queued wins, and a delivered earlier message is history.
    app.apply_steering(vec![
        steer(1, "first", Status::Delivered),
        steer(2, "second", Status::Queued),
    ]);
    assert_eq!(
        app.queued_steer.as_ref().map(|steer| steer.text.as_str()),
        Some("second")
    );

    app.apply_steering(vec![steer(2, "second", Status::Delivered)]);
    assert!(app.queued_steer.is_none());
}

/// A submission during a busy turn must be admitted, and its guidance must
/// show up promptly: the read follows the admission on the command lock.
#[test]
fn a_busy_submission_admits_guidance_then_reads_the_state_after_it() {
    let (mut app, remote) = app();
    app.start_turn();
    app.composer.set_input("use the other branch");
    assert_eq!(
        app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
        None
    );
    assert!(matches!(
        commands(&app).as_slice(),
        [Request::Command(backend::Command::AdmitInput(text)),
         Request::Command(backend::Command::SteeringState)] if text == "use the other branch"
    ));
    let (admission, _) = app.requests[0].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket: admission,
        result: Ok(Reply::Operation {
            message: "Guidance queued for the active task: use the other branch".into(),
            info: app.info.clone(),
            history: None,
            prompt: None,
        }),
    });
    let (read, _) = app.requests[1].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket: read,
        result: Ok(Reply::Steering(vec![steer(
            1,
            "use the other branch",
            Status::Queued,
        )])),
    });
    assert_eq!(app.queued_steer.as_ref().map(|steer| steer.id), Some(1));

    // The read leaves no write pending, so typing again during the same
    // turn is still guidance rather than a queued later turn.
    app.composer.set_input("and stop after that");
    assert_eq!(
        app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
        None
    );
    assert!(app.queued_input.is_none());
    let requests = commands(&app);
    assert!(matches!(
        &requests[requests.len() - 2..],
        [Request::Command(backend::Command::AdmitInput(text)),
         Request::Command(backend::Command::SteeringState)] if text == "and stop after that"
    ));
}

/// The read that follows an admission changes nothing, so it must not hold
/// a later submission back. If it took the pending-write slot, typing again
/// while it was in flight would be parked as a queued next turn instead of
/// steering the same turn — the silent later turn the contract forbids.
#[test]
fn a_guidance_read_in_flight_never_defers_the_next_submission() {
    let (mut app, remote) = app();
    app.start_turn();
    app.composer.set_input("use the other branch");
    assert_eq!(
        app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
        None
    );
    // The admission's own write resolves; only its read is outstanding.
    let (admission, _) = app.requests[0].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket: admission,
        result: Ok(Reply::Operation {
            message: "Guidance queued for the active task: use the other branch".into(),
            info: app.info.clone(),
            history: None,
            prompt: None,
        }),
    });
    assert!(
        !app.has_writes(),
        "a read in flight must not look like a pending write"
    );

    app.composer.set_input("and stop after that");
    assert_eq!(
        app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
        None
    );
    assert!(
        app.queued_input.is_none(),
        "a second guidance message must not become a queued next turn"
    );
    let requests = commands(&app);
    assert!(
        matches!(
            &requests[2..],
            [Request::Command(backend::Command::AdmitInput(text)),
             Request::Command(backend::Command::SteeringState)] if text == "and stop after that"
        ),
        "{requests:?}"
    );
}

#[test]
fn control_u_recalls_backend_confirmed_guidance_for_editing() {
    let (mut app, remote) = app();
    app.start_turn();
    // Both a queued next input and queued guidance are waiting: Ctrl+U must
    // take back the guidance the run owns and leave the queued next input
    // alone, because only the run can resolve the guidance's state.
    app.queued_input = Some("/memory search vim".into());
    app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));
    app.awaiting_user_echo = Some("use the other branch".into());
    app.composer.set_input("draft");

    app.handle_key(
        control_u(),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );

    assert!(app.composer.input.is_empty(), "the draft is cleared");
    assert_eq!(app.queued_input.as_deref(), Some("/memory search vim"));
    assert!(matches!(
        commands(&app).as_slice(),
        [
            Request::Command(backend::Command::WithdrawSteer(3)),
            Request::Command(backend::Command::SteeringState)
        ]
    ));

    let (withdraw, _) = app.requests[0].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket: withdraw,
        result: Ok(Reply::Operation {
            message: "Queued input withdrawn.".into(),
            info: app.info.clone(),
            history: None,
            prompt: None,
        }),
    });
    let (read, _) = app.requests[1].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket: read,
        result: Ok(Reply::Steering(vec![steer(
            3,
            "use the other branch",
            Status::Withdrawn,
        )])),
    });

    assert!(app.queued_steer.is_none());
    assert!(
        app.awaiting_user_echo.is_none(),
        "withdrawn guidance cannot still be waiting for its echo"
    );
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "use the other branch",
        "backend-confirmed withdrawal recalls the exact guidance"
    );
}

#[test]
fn recalled_guidance_does_not_overwrite_a_new_draft() {
    let (mut app, remote) = app();
    app.start_turn();
    app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));

    app.handle_key(
        control_u(),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    app.composer.set_input("new draft");
    app.apply_steering(vec![steer(3, "use the other branch", Status::Withdrawn)]);

    assert_eq!(app.composer.input.iter().collect::<String>(), "new draft");
    assert!(app.recalling_steer.is_none());
}

#[test]
fn delivered_guidance_is_not_restored_when_recall_loses_the_race() {
    let (mut app, remote) = app();
    app.start_turn();
    app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));

    app.handle_key(
        control_u(),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    app.apply_steering(vec![steer(3, "use the other branch", Status::Delivered)]);

    assert!(app.composer.input.is_empty());
    assert!(app.recalling_steer.is_none());
}

/// The still-live queued-next-input path: Ctrl+U clears it locally, with no
/// backend round trip.
#[test]
fn control_u_still_withdraws_a_queued_next_input() {
    let (mut app, remote) = app();
    app.start_turn();
    app.queued_input = Some("/memory search vim".into());
    app.awaiting_user_echo = Some("/memory search vim".into());
    app.composer.set_input("draft");

    app.handle_key(
        control_u(),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );

    assert!(app.queued_input.is_none());
    assert!(app.awaiting_user_echo.is_none());
    assert!(app.composer.input.is_empty());
    assert!(app.requests.is_empty());
}

#[test]
fn a_turn_end_returns_undelivered_guidance_to_the_composer() {
    let (mut app, _) = app();
    app.start_turn();
    app.awaiting_user_echo = Some("use the other branch".into());
    app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));

    app.end_turn();

    assert!(app.queued_steer.is_none());
    assert!(app.awaiting_user_echo.is_none());
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "use the other branch"
    );
    assert!(matches!(
        app.requests.last().unwrap().1,
        Request::Command(backend::Command::SteeringState)
    ));

    // A draft the user typed meanwhile is never overwritten.
    app.composer.set_input("my own draft");
    app.queued_steer = Some(steer(4, "use the other branch", Status::Queued));
    app.end_turn();
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "my own draft"
    );
}

/// Delivery, not the turn end, is what resolves guidance the run took: the
/// echo is the run's own word that the text reached the model.
#[test]
fn a_delivered_guidance_echo_leaves_the_composer_empty_at_the_turn_end() {
    let (mut app, _) = app();
    app.start_turn();
    app.awaiting_user_echo = Some("use the other branch".into());
    app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));

    app.apply_event(&WireEvent {
        event_type: USER_MESSAGE.into(),
        text: "use the other branch".into(),
        ..Default::default()
    });
    assert!(app.queued_steer.is_none());
    assert!(app.awaiting_user_echo.is_none());

    app.end_turn();
    assert!(app.composer.input.is_empty());
}

/// `/steer` is explicit guidance: with nothing running it reports the error
/// the runtime gave instead of silently becoming a prompt.
#[test]
fn an_explicit_steer_command_never_starts_a_turn() {
    let (mut app, remote) = app();
    assert_eq!(
        app.dispatch_line(
            "/steer use the other branch",
            &Backend::Attach(&remote),
            &CancellationToken::new()
        ),
        None
    );
    assert!(matches!(
        commands(&app).as_slice(),
        [Request::Command(backend::Command::Steer(text)),
         Request::Command(backend::Command::SteeringState)] if text == "use the other branch"
    ));
    assert!(
        app.drain_actions().is_empty(),
        "an explicit steer must not dispatch a prompt"
    );
    // The runtime's reason reaches the transcript rather than a turn.
    let (ticket, _) = app.requests[0].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket,
        result: Err("there is no active task accepting guidance".into()),
    });
    assert!(
        app.entries
            .iter()
            .any(|entry| entry.raw == "there is no active task accepting guidance"),
        "{:?}",
        app.entries
            .iter()
            .map(|e| e.raw.as_str())
            .collect::<Vec<_>>()
    );
}
