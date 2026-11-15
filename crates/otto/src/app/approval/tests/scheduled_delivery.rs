//! Scheduled delivery and scheduled run tests.

use super::*;

#[tokio::test]
async fn user_turns_set_the_origin_and_wake_turns_keep_it() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = wake_controller(
        workspace.path(),
        sessions.path(),
        vec![("a", false), ("b", false), ("c", false), ("d", false)],
    )
    .await;
    assert_eq!(origin_of(&controller), None);
    controller
        .prompt("hi", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        origin_of(&controller),
        Some(otto_session::origin::Origin::terminal())
    );
    let chat = otto_session::origin::Origin::chat("telegram", "42").unwrap();
    turn(&controller, "hi", Some(chat.clone())).await;
    assert_eq!(origin_of(&controller), Some(chat.clone()));
    queue_wake(&controller);
    run_wake(&controller, &CancellationToken::new()).await;
    assert_eq!(origin_of(&controller), Some(chat));
    turn(&controller, "hi", None).await;
    assert_eq!(origin_of(&controller), None);
}

#[tokio::test]
async fn the_next_user_turn_appends_a_delivery_once_before_the_user_message() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::schedule::Store::open_in_memory().unwrap());
    let controller = schedule_controller(
        workspace.path(),
        sessions.path(),
        vec![("a", false), ("b", false), ("c", false)],
        Some(store.clone()),
    )
    .await;
    let session_id = controller.info().session_id;
    let terminal = otto_session::origin::Origin::terminal();
    let row = delivered_row(&store, terminal.clone(), &session_id, "drink water");

    // A wake turn appends nothing.
    run_wake(&controller, &CancellationToken::new()).await;
    assert!(scheduled_messages(&controller).is_empty());

    turn(&controller, "hello", Some(terminal.clone())).await;
    let history = controller.current_session().messages();
    let appended = history
        .iter()
        .position(|message| message.context_type == "scheduled_delivery")
        .expect("delivery appended");
    assert_eq!(history[appended].role, Role::Context);
    assert_eq!(history[appended].text(), row.context_text());
    let user = history
        .iter()
        .position(|message| message.role == Role::User && message.text() == "hello")
        .expect("user message");
    assert!(appended < user);
    assert!(
        store
            .pending_writebacks(&terminal, &session_id, 10)
            .unwrap()
            .is_empty()
    );

    turn(&controller, "again", Some(terminal)).await;
    assert_eq!(scheduled_messages(&controller).len(), 1);
}

#[tokio::test]
async fn a_chat_turn_appends_deliveries_created_by_another_session() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::schedule::Store::open_in_memory().unwrap());
    let controller = schedule_controller(
        workspace.path(),
        sessions.path(),
        vec![("a", false), ("b", false)],
        Some(store.clone()),
    )
    .await;
    let chat = otto_session::origin::Origin::chat("telegram", "42").unwrap();
    let other = otto_session::origin::Origin::chat("telegram", "7").unwrap();
    delivered_row(&store, chat.clone(), "old-session", "for chat 42");
    delivered_row(&store, other, "old-session", "for chat 7");

    turn(&controller, "hello", Some(chat)).await;
    let messages = scheduled_messages(&controller);
    assert_eq!(messages.len(), 1);
    assert!(messages[0].text().ends_with("for chat 42"));

    // A turn with no origin appends nothing.
    turn(&controller, "again", None).await;
    assert_eq!(scheduled_messages(&controller).len(), 1);
}

#[tokio::test]
async fn a_scheduled_run_maps_the_turn_end_to_an_outcome() {
    use crate::schedule::RunOutcome;
    assert_eq!(
        scheduled_run(vec![("checking ", true), ("all fine ", false)]).await,
        RunOutcome::Reply("checking\n\nall fine".into())
    );
    assert!(matches!(
        scheduled_run(Vec::new()).await,
        RunOutcome::FailedBeforeTools(message) if message.contains("script exhausted")
    ));
    assert!(matches!(
        scheduled_run(vec![("checking", true)]).await,
        RunOutcome::Failed(message) if message.contains("script exhausted")
    ));
}
