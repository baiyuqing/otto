//! Tests for banded rows above the composer.

use super::*;

#[tokio::test]
async fn model_feedback_is_pinned_in_a_banded_row_above_the_composer() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = (0..12)
        .map(|index| Entry {
            kind: Some(EntryKind::Assistant),
            raw: format!("old transcript row {index}"),
            ..Default::default()
        })
        .collect();
    app.scroll = Some(0);
    app.composer_notices = vec!["Thinking: high. Saved to profile.".into()];

    let width = 72;
    let height = 12;
    let rows = screen_rows(&app, width, height);
    let banner_row = rows
        .iter()
        .position(|row| row.contains("◆ UPDATED  Thinking: high. Saved to profile."))
        .expect("model feedback row");
    assert!(rows[banner_row + 1].contains("Otto"), "{rows:?}");
    assert!(
        !app.entries
            .iter()
            .any(|entry| entry.raw.contains("Thinking: high")),
        "model feedback must not be a transcript entry"
    );

    let buffer = drawn(&app, width, height);
    for x in SIDE_MARGIN..width - SIDE_MARGIN {
        assert_eq!(
            buffer.cell((x, banner_row as u16)).map(|cell| cell.bg),
            Some(QUEUED_BACKGROUND),
            "model feedback must shade the full content row at x={x}"
        );
    }
}

#[tokio::test]
async fn multiline_model_feedback_uses_one_banded_row_per_message() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.composer_notices = vec![
        "Switched to profile terra (provider chatgpt, model gpt-terra). Set as default profile."
            .into(),
        "Thinking: medium".into(),
    ];

    let (width, height) = (100, 12);
    let rows = screen_rows(&app, width, height);
    let profile_row = rows
        .iter()
        .position(|row| row.contains("Set as default profile."))
        .expect("profile feedback row");
    assert!(rows[profile_row + 1].contains("◆ UPDATED  Thinking: medium"));
    assert!(rows.iter().all(|row| !row.contains(r"\x0a")), "{rows:?}");

    let buffer = drawn(&app, width, height);
    for y in profile_row as u16..=profile_row as u16 + 1 {
        for x in SIDE_MARGIN..width - SIDE_MARGIN {
            assert_eq!(
                buffer.cell((x, y)).map(|cell| cell.bg),
                Some(QUEUED_BACKGROUND),
                "each feedback row must be fully shaded at ({x}, {y})"
            );
        }
    }
}

#[tokio::test]
async fn queued_input_is_pinned_in_a_banded_row_above_the_composer() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = (0..12)
        .map(|index| Entry {
            kind: Some(EntryKind::Assistant),
            raw: format!("old transcript row {index}"),
            ..Default::default()
        })
        .collect();
    app.scroll = Some(0);
    app.start_turn();
    app.queued_input = Some("/memory search vim".to_string());
    app.apply_event(&otto_core::wire::events::WireEvent {
        event_type: otto_core::wire::events::USER_MESSAGE.into(),
        text: "guidance for current turn".into(),
        ..Default::default()
    });

    let width = 72;
    let height = 12;
    let rows = screen_rows(&app, width, height);
    let banner_row = rows
        .iter()
        .position(|row| row.contains("◆ QUEUED NEXT INPUT  /memory search vim"))
        .expect("queued banner row");
    assert!(rows[banner_row + 1].contains("Queued next input · Ctrl+U withdraw"));
    assert!(
        !rows.iter().any(|row| row.contains("❯ Queued next input")),
        "queued input must not be a transcript entry: {rows:?}"
    );

    let buffer = drawn(&app, width, height);
    for x in SIDE_MARGIN..width - SIDE_MARGIN {
        assert_eq!(
            buffer.cell((x, banner_row as u16)).map(|cell| cell.bg),
            Some(QUEUED_BACKGROUND),
            "queued banner must shade the full content row at x={x}"
        );
    }
}

/// Guidance the active turn has accepted but not delivered is pinned above
/// the composer, so the frame says both that it waits and how to take it back.
#[tokio::test]
async fn queued_guidance_draws_in_the_banner_and_the_composer_title() {
    use crate::tui::backend::{Completion, Reply};
    use otto_core::agent::steering::{Message, Status};

    let (_workspace, _sessions, mut app) = app_fixture().await;
    let message = |status| Message {
        input_provenance: None,
        id: 3,
        text: "use the other branch".to_string(),
        status,
        input_id: "steer:turn:1".to_string(),
    };
    // The run's own read is what paints the queued guidance; the frame test
    // goes through it rather than setting the field directly.
    let read = |app: &mut App, messages| {
        let ticket = app.refresh_steering();
        app.apply_completion(Completion {
            mutation: true,
            ticket,
            result: Ok(Reply::Steering(messages)),
        });
    };
    app.start_turn();
    read(&mut app, vec![message(Status::Queued)]);

    let screen = rendered(&app, 72, 12);
    assert!(
        screen.contains("◆ QUEUED FOR CURRENT TURN  use the other branch"),
        "{screen}"
    );
    assert!(
        screen.contains("Queued for current turn · Ctrl+U recall to edit · Esc delivers now"),
        "{screen}"
    );
    assert!(
        !screen.contains("│use the other branch"),
        "queued guidance should not remain in the composer:\n{screen}"
    );

    // A withdrawal, then a delivery, each leave nothing to draw.
    read(&mut app, vec![message(Status::Withdrawn)]);
    let screen = rendered(&app, 72, 12);
    assert!(!screen.contains("Queued for current turn"), "{screen}");
    assert!(!screen.contains("Ctrl+U withdraw"), "{screen}");

    read(&mut app, vec![message(Status::Queued)]);
    assert!(rendered(&app, 72, 12).contains("◆ QUEUED FOR CURRENT TURN  use the other branch"));
    app.apply_event(&otto_core::wire::events::WireEvent {
        event_type: otto_core::wire::events::USER_MESSAGE.into(),
        text: "use the other branch".into(),
        ..Default::default()
    });
    let screen = rendered(&app, 72, 12);
    assert!(!screen.contains("Queued for current turn"), "{screen}");
    assert!(!screen.contains("Ctrl+U withdraw"), "{screen}");
}
