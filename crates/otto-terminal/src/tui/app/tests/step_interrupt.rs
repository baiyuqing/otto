//! Tests for `Esc` with queued guidance and the `step_interrupted` event.

use super::*;

fn key(code: KeyCode, modifiers: crossterm::event::KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn busy_app_with_queued_steer(queued: bool) -> (App, Remote) {
    let (mut app, remote) = app();
    app.start_turn();
    if queued {
        app.apply_steering(vec![steer(1, "use the other branch", Status::Queued)]);
        assert!(app.queued_steer.is_some());
    }
    (app, remote)
}

fn press_key(app: &mut App, remote: &Remote, event: KeyEvent) -> Option<Action> {
    app.handle_key(event, &Backend::Attach(remote), &CancellationToken::new())
}

/// Fails without the `Esc` branch: the key would return `CancelTurn` and
/// enqueue no `InterruptStep`.
#[test]
fn esc_with_queued_guidance_requests_a_step_interrupt() {
    let (mut app, remote) = busy_app_with_queued_steer(true);
    let none = crossterm::event::KeyModifiers::NONE;
    assert_eq!(press_key(&mut app, &remote, key(KeyCode::Esc, none)), None);
    // The steering read follows the command so the display repaints.
    assert!(matches!(
        commands(&app).as_slice(),
        [
            Request::Command(backend::Command::InterruptStep),
            Request::Command(backend::Command::SteeringState)
        ]
    ));
}

#[test]
fn esc_without_queued_guidance_cancels_the_turn() {
    let (mut app, remote) = busy_app_with_queued_steer(false);
    let none = crossterm::event::KeyModifiers::NONE;
    assert_eq!(
        press_key(&mut app, &remote, key(KeyCode::Esc, none)),
        Some(Action::CancelTurn)
    );
    assert!(commands(&app).is_empty());
}

#[test]
fn ctrl_c_cancels_the_turn_with_or_without_queued_guidance() {
    for queued in [false, true] {
        let (mut app, remote) = busy_app_with_queued_steer(queued);
        let ctrl = crossterm::event::KeyModifiers::CONTROL;
        assert_eq!(
            press_key(&mut app, &remote, key(KeyCode::Char('c'), ctrl)),
            Some(Action::CancelTurn),
            "queued guidance: {queued}"
        );
        assert!(commands(&app).is_empty());
    }
}

fn delta(event_type: &str, text: &str) -> WireEvent {
    WireEvent {
        event_type: event_type.into(),
        text: text.into(),
        ..Default::default()
    }
}

fn step_interrupted(queued_guidance: Option<u32>) -> WireEvent {
    WireEvent {
        event_type: "step_interrupted".into(),
        queued_guidance,
        ..Default::default()
    }
}

fn assistant_texts(app: &App) -> Vec<String> {
    app.entries
        .iter()
        .filter(|entry| entry.kind == Some(EntryKind::Assistant))
        .map(|entry| entry.raw.clone())
        .collect()
}

fn system_texts(app: &App) -> Vec<String> {
    app.entries
        .iter()
        .filter(|entry| entry.kind == Some(EntryKind::System))
        .map(|entry| entry.raw.clone())
        .collect()
}

/// Fails without the `step_interrupted` arm: no system line is pushed, and the
/// second delta extends the first assistant row instead of starting a new one.
#[test]
fn a_step_interrupt_closes_the_streaming_row_and_reports_one_message() {
    let (mut app, _) = app();
    app.apply_event(&delta("text_delta", "partial "));
    app.apply_event(&delta("text_delta", "reply"));
    app.apply_event(&step_interrupted(Some(1)));
    app.apply_event(&delta("text_delta", "after guidance"));

    assert_eq!(assistant_texts(&app), ["partial reply", "after guidance"]);
    assert_eq!(
        system_texts(&app),
        ["Step interrupted; delivering 1 queued message."]
    );
}

#[test]
fn a_step_interrupt_closes_the_reasoning_row() {
    let (mut app, _) = app();
    app.apply_event(&delta("reasoning_delta", "thinking"));
    app.apply_event(&step_interrupted(Some(1)));
    app.apply_event(&delta("reasoning_delta", "again"));

    let reasoning: Vec<_> = app
        .entries
        .iter()
        .filter(|entry| entry.kind == Some(EntryKind::Reasoning))
        .map(|entry| entry.raw.clone())
        .collect();
    assert_eq!(reasoning, ["thinking", "again"]);
}

#[test]
fn a_step_interrupt_names_the_number_of_delivered_messages() {
    for (count, line) in [
        (Some(3), "Step interrupted; delivering 3 queued messages."),
        (Some(0), "Step interrupted."),
        (None, "Step interrupted."),
    ] {
        let (mut app, _) = app();
        app.apply_event(&step_interrupted(count));
        assert_eq!(system_texts(&app), [line]);
    }
}
