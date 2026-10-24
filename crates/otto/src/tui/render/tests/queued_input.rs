//! Visible queue lifecycle: these assertions inspect the actual terminal buffer.
use super::*;
use crate::tui::app::Backend as AppBackend;
use crate::tui::backend::{Completion, Reply, Ticket};
use otto_core::agent::steering::{Message, Status};

fn message(id: u64, text: &str, status: Status) -> Message {
    Message {
        id,
        text: text.into(),
        status,
        input_id: format!("input-{id}"),
    }
}

fn read_ticket(app: &mut App) -> Ticket {
    app.refresh_steering()
}

fn complete(app: &mut App, ticket: Ticket, messages: Vec<Message>) {
    app.apply_completion(Completion {
        ticket,
        mutation: true,
        result: Ok(Reply::Steering(messages)),
    });
}

fn queue(app: &mut App, messages: Vec<Message>) {
    let ticket = read_ticket(app);
    complete(app, ticket, messages);
}

fn main_conversation(app: &App) -> String {
    let rows = screen_rows(app, 100, 32);
    // Exclude the pinned queue banner and composer: queue text must be in the
    // conversation itself, not merely in a field or feedback banner.
    rows.into_iter()
        .take_while(|row| !row.contains("◆ QUEUED") && !row.contains('╭'))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn queued_guidance_is_visible_until_consumed_once_in_local_and_attached_frames() {
    for attached in [false, true] {
        let (_workspace, _sessions, mut app) = app_fixture().await;
        app.attached = attached;
        app.start_turn();
        queue(
            &mut app,
            vec![
                message(1, "first guidance", Status::Queued),
                message(2, "second guidance", Status::Queued),
            ],
        );
        let screen = main_conversation(&app);
        assert!(
            screen.contains("[Queued for current turn] first guidance"),
            "older accepted input must be visible: {screen}"
        );
        assert!(
            screen.contains("[Queued for current turn] second guidance"),
            "newer accepted input must be visible: {screen}"
        );
        assert!(
            app.entries.is_empty(),
            "pending guidance must not be durable history"
        );
        app.apply_event(&wire_frame(&Event::Notification {
            kind: Some(otto_core::agent::inbox::NotificationKind::UserMessage),
            task_id: String::new(),
            text: "first guidance".into(),
            usage: Default::default(),
            present: false,
            cached_input_tokens_present: false,
        }));
        let screen = main_conversation(&app);
        assert_eq!(
            screen.matches("first guidance").count(),
            1,
            "delivery must replace, not duplicate, pending text: {screen}"
        );
        assert!(
            !screen.contains("[Queued for current turn] first guidance"),
            "consumed input must lose its queued label: {screen}"
        );
        assert!(
            screen.contains("[Queued for current turn] second guidance"),
            "delivery must preserve other queued input: {screen}"
        );
        queue(
            &mut app,
            vec![
                message(1, "first guidance", Status::Delivered),
                message(2, "second guidance", Status::Withdrawn),
            ],
        );
        let screen = main_conversation(&app);
        assert!(
            !screen.contains("second guidance"),
            "withdrawn text must leave conversation: {screen}"
        );
        assert_eq!(
            screen.matches("first guidance").count(),
            1,
            "completed read cannot duplicate delivered history: {screen}"
        );
    }
}

#[tokio::test]
async fn next_input_is_transient_and_consumption_replaces_queued_projection() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.queued_input = Some("next prompt".into());
    assert!(main_conversation(&app).contains("[Queued next input] next prompt"));
    assert!(app.entries.is_empty());
    let remote = crate::tui::attach::Remote::offline(Vec::new());
    app.submit_queued_input(
        &AppBackend::Attach(&remote),
        &tokio_util::sync::CancellationToken::new(),
    );
    let screen = main_conversation(&app);
    assert_eq!(
        screen.matches("next prompt").count(),
        1,
        "consumption renders only actual prompt: {screen}"
    );
    assert!(
        !screen.contains("[Queued next input]"),
        "consumed next input cannot keep queued label: {screen}"
    );
}

#[tokio::test]
async fn restored_queue_is_visible_and_old_reads_cannot_reappear_after_switch_or_finish() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    let stale = read_ticket(&mut app);
    let fresh = read_ticket(&mut app);
    complete(
        &mut app,
        fresh,
        vec![message(2, "restored pending", Status::Queued)],
    );
    complete(
        &mut app,
        stale,
        vec![message(1, "stale pending", Status::Queued)],
    );
    let screen = main_conversation(&app);
    assert!(
        screen.contains("restored pending"),
        "authoritative restored queue must be visible: {screen}"
    );
    assert!(
        !screen.contains("stale pending"),
        "old read must not repaint queue: {screen}"
    );
    let stale_session = read_ticket(&mut app);
    let remote = crate::tui::attach::Remote::offline(Vec::new());
    app.queued_input = Some("previous session next input".into());
    let restore = app.refresh(&AppBackend::Attach(&remote));
    complete(
        &mut app,
        stale_session,
        vec![message(3, "previous session pending", Status::Queued)],
    );
    let screen = main_conversation(&app);
    assert!(
        !screen.contains("restored pending") && !screen.contains("previous session pending"),
        "switch must clear previous session queue: {screen}"
    );
    assert!(
        !screen.contains("previous session next input"),
        "next input must stay scoped to its session: {screen}"
    );
    complete(
        &mut app,
        restore,
        vec![message(4, "resumed queue", Status::Queued)],
    );
    assert!(
        main_conversation(&app).contains("resumed queue"),
        "refresh must request and display resumed queue"
    );
    let late = read_ticket(&mut app);
    app.end_turn();
    complete(
        &mut app,
        late,
        vec![message(5, "late queue", Status::Queued)],
    );
    assert!(
        !main_conversation(&app).contains("late queue"),
        "turn finish must reject a pre-finish queue read"
    );
}
