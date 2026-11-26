//! Tests for queued input echoes and document scrolling.

use super::*;

#[test]
fn delivered_queued_input_is_visible_after_steering_read_clears_banner() {
    let text = "我发的 queued input 又看不到了";
    for attached in [false, true] {
        let (mut app, _) = app();
        app.attached = attached;
        app.start_turn();
        app.apply_steering(vec![steer(1, text, Status::Queued)]);
        app.apply_steering(vec![steer(1, text, Status::Delivered)]);
        assert!(app.queued_steer.is_none());
        let event = otto_core::wire::events::to_wire(&otto_core::agent::Event::Notification {
            input_id: String::new(),
            input_provenance: None,
            kind: Some(otto_core::agent::inbox::NotificationKind::UserMessage),
            task_id: String::new(),
            text: text.into(),
            usage: Default::default(),
            present: false,
            cached_input_tokens_present: false,
        });
        app.apply_event(&event);
        assert_eq!(
            app.entries
                .iter()
                .filter(|entry| entry.kind == Some(EntryKind::User) && entry.raw == text)
                .count(),
            1,
            "delivered queued input must remain a user transcript entry (attached={attached})"
        );
        assert!(
            !app.entries
                .iter()
                .any(|entry| entry.kind == Some(EntryKind::System) && entry.raw.contains(text))
        );
    }
}

#[test]
fn user_message_echo_does_not_duplicate_prompt_history() {
    let (mut app, remote) = app();
    app.insert_text("hello");
    assert!(
        matches!(app.handle_key(enter(),&Backend::Attach(&remote),&CancellationToken::new()),Some(Action::Prompt(line)) if line=="hello")
    );
    app.apply_event(&WireEvent {
        event_type: USER_MESSAGE.into(),
        text: "hello".into(),
        ..Default::default()
    });
    app.composer.edit(Edit::PreviousHistory);
    assert_eq!(app.composer.input.iter().collect::<String>(), "hello");
    app.composer.edit(Edit::NextHistory);
    assert!(app.composer.input.is_empty());
    app.insert_text("queued");
    let line = app.composer.take().unwrap();
    app.queued_input = Some(line.clone());
    app.awaiting_user_echo = Some(line.clone());
    app.apply_event(&WireEvent {
        event_type: USER_MESSAGE.into(),
        text: line.clone(),
        ..Default::default()
    });
    app.composer.edit(Edit::PreviousHistory);
    assert_eq!(app.composer.input.iter().collect::<String>(), line);
    app.composer.edit(Edit::PreviousHistory);
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "hello",
        "queued echo must not add a second history entry"
    );
}

#[test]
fn unrelated_user_message_does_not_consume_queued_next_input() {
    let (mut app, _) = app();
    app.attached = false;
    app.start_turn();
    app.queued_input = Some("/memory search vim".into());

    app.apply_event(&WireEvent {
        event_type: USER_MESSAGE.into(),
        text: "guidance for current turn".into(),
        ..Default::default()
    });

    assert_eq!(app.queued_input.as_deref(), Some("/memory search vim"));
    assert_eq!(app.entries.last().unwrap().raw, "guidance for current turn");
}

#[test]
fn scroll_reverses_immediately_after_reaching_the_document_end() {
    let (mut app, _) = app();
    app.transition(
        0,
        Transition::Push(probe(
            "document",
            View {
                document: "one\ntwo\nthree".into(),
                ..Default::default()
            },
            Arc::new(AtomicUsize::new(0)),
        )),
    );
    app.geometry.borrow_mut().max_scroll = 2;
    app.surfaces.top_mut().unwrap().state.scroll = u16::MAX;
    app.scroll_wheel(true);
    assert_eq!(app.surfaces.top().unwrap().state.scroll, 1);
}
