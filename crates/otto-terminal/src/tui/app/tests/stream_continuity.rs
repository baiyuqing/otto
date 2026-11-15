use super::*;

fn delta(text: &str) -> WireEvent {
    WireEvent {
        event_type: "text_delta".into(),
        text: text.into(),
        ..Default::default()
    }
}

fn assistant_texts(app: &App) -> Vec<&str> {
    app.entries
        .iter()
        .filter(|entry| entry.kind == Some(EntryKind::Assistant))
        .map(|entry| entry.raw.as_str())
        .collect()
}

#[test]
fn status_notices_between_deltas_do_not_split_the_assistant_reply() {
    for attached in [false, true] {
        let (mut app, _) = app();
        app.attached = attached;
        app.start_turn();
        app.apply_event(&delta("stream held"));
        app.push_system("[input-1] Tui / Direct / delivered; /inputs lists session-scoped status");
        app.push_system("another informational notice");
        app.apply_event(&delta(" then released"));
        assert_eq!(assistant_texts(&app), ["stream held then released"]);
        assert!(
            app.entries
                .iter()
                .any(|entry| entry.raw == "another informational notice")
        );
    }
}

#[test]
fn content_rows_remain_stream_boundaries_even_with_status_notices() {
    for kind in [EntryKind::User, EntryKind::Tool, EntryKind::Reasoning] {
        let (mut app, _) = app();
        app.apply_event(&delta("before"));
        app.entries.push(Entry {
            id: "content-boundary".into(),
            kind: Some(kind),
            raw: "intervening content".into(),
            ..Default::default()
        });
        app.push_system("status notice");
        app.apply_event(&delta("after"));
        assert_eq!(assistant_texts(&app), ["before", "after"]);
    }
}

#[test]
fn consecutive_turns_without_intervening_user_rows_keep_separate_replies() {
    let (mut app, _) = app();
    app.start_turn();
    app.apply_event(&delta("first reply"));
    app.end_turn();
    app.start_turn();
    app.apply_event(&delta("second reply"));
    assert_eq!(assistant_texts(&app), ["first reply", "second reply"]);
}
