use super::*;
use otto_core::model::{InputDelivery, InputSource};

async fn local() -> (tempfile::TempDir, tempfile::TempDir, Arc<Controller>, App) {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = Arc::new(otto::fixtures::controller(workspace.path(), sessions.path()).await);
    let app = App::new(&Backend::Local(&controller));
    (workspace, sessions, controller, app)
}

fn local_enter(app: &mut App, controller: &Controller) -> Option<Action> {
    app.handle_key(
        enter(),
        &Backend::Local(controller),
        &CancellationToken::new(),
    )
}

#[tokio::test]
async fn enter_persists_before_clear_and_identical_inputs_have_distinct_ids() {
    let (_workspace, _sessions, controller, mut app) = local().await;
    let mut ids = Vec::new();
    for _ in 0..2 {
        app.composer.set_input("source of truth");
        let Some(Action::LocalPrompt {
            text,
            input_id,
            delivery,
        }) = local_enter(&mut app, &controller)
        else {
            panic!("Enter must retain its stable identity")
        };
        assert_eq!(text, "source of truth");
        assert_eq!(delivery, InputDelivery::Direct);
        let saved = controller.input_submissions().unwrap();
        let record = saved
            .iter()
            .find(|record| record.id == input_id)
            .expect("durable record before composer clear");
        assert_eq!(record.text, text);
        assert_eq!(record.state, "pending");
        assert_eq!(record.provenance.source, InputSource::Tui);
        assert!(app.composer.input.is_empty());
        assert!(
            app.entries
                .iter()
                .any(|entry| entry.raw.contains(&input_id))
        );
        ids.push(input_id);
    }
    assert_ne!(ids[0], ids[1]);
    assert_eq!(controller.input_submissions().unwrap().len(), 2);
}

#[tokio::test]
async fn failed_save_retains_original_editor_and_does_not_dispatch() {
    let (_workspace, _sessions, controller, mut app) = local().await;
    controller
        .current_session_opt()
        .unwrap()
        .handle()
        .close()
        .unwrap();
    app.composer.set_input("do not lose my draft");
    assert_eq!(local_enter(&mut app, &controller), None);
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "do not lose my draft"
    );
    assert!(app.requests.is_empty());
    assert!(
        app.entries
            .iter()
            .any(|entry| entry.raw.contains("original draft retained"))
    );
}

#[tokio::test]
async fn delayed_write_queue_carries_id_through_busy_to_idle_and_blocks_overwrite() {
    let (_workspace, _sessions, controller, mut app) = local().await;
    app.enqueue(
        0,
        Request::Command(backend::Command::Rename("renamed".into())),
    );
    app.composer.set_input("queued input");
    assert_eq!(local_enter(&mut app, &controller), None);
    let record = controller.input_submissions().unwrap().pop().unwrap();
    assert_eq!(record.provenance.delivery, InputDelivery::Queued);
    app.composer.set_input("second input remains draft");
    assert_eq!(local_enter(&mut app, &controller), None);
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "second input remains draft"
    );
    assert_eq!(controller.input_submissions().unwrap().len(), 1);
    let (ticket, _) = app.requests[0].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket,
        result: Ok(Reply::Operation {
            message: String::new(),
            info: controller.info(),
            history: None,
            prompt: None,
        }),
    });
    let action = app.submit_queued_input(&Backend::Local(&controller), &CancellationToken::new());
    assert!(
        matches!(action, Some(Action::LocalPrompt { input_id, delivery: InputDelivery::Queued, .. }) if input_id == record.id)
    );
}

#[tokio::test]
async fn busy_enter_admission_race_to_idle_preserves_id_in_completion() {
    let (_workspace, _sessions, controller, mut app) = local().await;
    app.start_turn(); // controller is already idle: exercise the actual admission race.
    app.composer.set_input("race input");
    assert_eq!(local_enter(&mut app, &controller), None);
    let record = controller.input_submissions().unwrap().pop().unwrap();
    assert_eq!(record.provenance.delivery, InputDelivery::Steering);
    let (ticket, request) = app.requests[0].clone();
    let mut driver = Driver::new(backend::Connection::Local(controller.clone()));
    driver.submit(ticket, request);
    app.apply_completion(driver.next().await);
    assert!(
        matches!(app.drain_actions().as_slice(), [Action::LocalPrompt { input_id, delivery: InputDelivery::Direct, .. }] if input_id == &record.id)
    );
    assert_eq!(
        controller.input_submissions().unwrap()[0]
            .provenance
            .delivery,
        InputDelivery::Direct
    );
}

#[tokio::test]
async fn recovery_is_explicit_session_scoped_draft_and_does_not_replay() {
    let (_workspace, _sessions, controller, _app) = local().await;
    let record = controller
        .prepare_input("uncertain effect", InputDelivery::Direct)
        .unwrap();
    controller
        .update_input_submission(&record.id, "accepted", None)
        .unwrap();
    // A fresh UI has no queued action, even when admission happened before loss.
    let mut app = App::new(&Backend::Local(&controller));
    app.refresh_tasks(&Backend::Local(&controller));
    assert!(app.drain_actions().is_empty());
    assert!(app.queued_input.is_none());
    assert!(
        app.entries
            .iter()
            .any(|entry| entry.raw.contains(&record.id))
    );
    app.composer.set_input(&format!("/recover {}", record.id));
    assert_eq!(local_enter(&mut app, &controller), None);
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "uncertain effect"
    );
    assert!(!controller.has_input(&record.id));
    controller.new_session().await.unwrap();
    assert!(controller.input_submissions().unwrap().is_empty());
}

#[tokio::test]
async fn withdrawn_queue_is_recoverable_by_id_without_execution() {
    let (_workspace, _sessions, controller, mut app) = local().await;
    app.enqueue(
        0,
        Request::Command(backend::Command::Rename("pending".into())),
    );
    app.composer.set_input("withdraw me");
    local_enter(&mut app, &controller);
    let record = controller.input_submissions().unwrap().pop().unwrap();
    app.handle_key(
        control_u(),
        &Backend::Local(&controller),
        &CancellationToken::new(),
    );
    assert!(app.queued_input.is_none());
    assert!(app.queued_input_id.is_none());
    assert_eq!(
        controller.input_submissions().unwrap()[0].state,
        "withdrawn"
    );
    assert!(!controller.has_input(&record.id));
}

#[tokio::test]
async fn process_restart_restores_uncertain_intent_only_not_an_action() {
    let (workspace, sessions, controller, mut app) = local().await;
    app.composer.set_input("uncertain after process loss");
    local_enter(&mut app, &controller);
    let record = controller.input_submissions().unwrap().pop().unwrap();
    controller
        .update_input_submission(&record.id, "accepted", None)
        .unwrap();
    controller
        .notification_mailbox()
        .unwrap()
        .enqueue(otto_core::mailbox::Notification {
            id: record.id.clone(),
            text: record.text.clone(),
            kind: Some(otto_core::mailbox::NotificationKind::UserMessage),
            input_provenance: Some(record.provenance),
            ..Default::default()
        })
        .unwrap();
    let path = controller.info().session_path;
    controller.close().unwrap();
    let restarted = otto::fixtures::controller(workspace.path(), sessions.path()).await;
    restarted.resume_session(&path).await.unwrap();
    let mut restored = App::new(&Backend::Local(&restarted));
    restored.refresh_tasks(&Backend::Local(&restarted));
    assert_eq!(restarted.input_submissions().unwrap()[0].id, record.id);
    assert!(restored.drain_actions().is_empty());
    assert!(restored.queued_input.is_none());
    assert!(!restarted.has_input(&record.id));
    assert!(
        !restarted
            .notification_mailbox()
            .unwrap()
            .queued()
            .iter()
            .any(|entry| entry.notification.id == record.id)
    );

    assert!(
        restored
            .entries
            .iter()
            .any(|entry| entry.raw.contains(&record.id))
    );
}

#[test]
fn no_session_input_events_live_only_in_the_handle() {
    use otto_session::SessionHandle;
    let header = otto_core::session::Header {
        id: "memory-session".into(),
        ..Default::default()
    };
    let memory = otto::runtime::builder::MemoryHandle::new(header.clone());
    memory
        .append_input_submissions("recoverable during process lifetime")
        .unwrap();
    assert!(memory.path().is_empty());
    assert_eq!(memory.input_submission_events().unwrap().len(), 1);
    let restarted = otto::runtime::builder::MemoryHandle::new(header);
    assert!(restarted.input_submission_events().unwrap().is_empty());
}

#[tokio::test]
async fn active_enter_reaches_admission_and_delivers_original_id_and_source() {
    use otto::fixtures::subagent_doubles::{FakeProvider, assistant_text, match_any};
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Default::default())]);
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    provider.set_hook(Arc::new({
        let release = release.clone();
        let calls = calls.clone();
        move |_, _| {
            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
            let release = release.clone();
            Box::pin(async move {
                if first {
                    release.notified().await;
                }
            })
        }
    }));
    otto::runtime::builder::TEST_PROVIDERS
        .lock()
        .unwrap()
        .push((workspace.path().to_string_lossy().into_owned(), provider));
    let controller = Arc::new(otto::fixtures::controller(workspace.path(), sessions.path()).await);
    let running = tokio::spawn({
        let controller = controller.clone();
        async move {
            controller
                .prompt("initial task", &mut |_| {}, &CancellationToken::new())
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut app = App::new(&Backend::Local(&controller));
    app.start_turn();
    app.composer.set_input("active guidance");
    local_enter(&mut app, &controller);
    let record = controller.input_submissions().unwrap().pop().unwrap();
    let (ticket, request) = app.requests[0].clone();
    let mut driver = Driver::new(backend::Connection::Local(controller.clone()));
    driver.submit(ticket, request);
    app.apply_completion(driver.next().await);
    let guidance = controller.steering_messages().unwrap();
    assert_eq!(guidance[0].input_id, record.id);
    assert_eq!(
        guidance[0].input_provenance.unwrap().delivery,
        InputDelivery::Steering
    );
    assert_eq!(controller.input_submissions().unwrap()[0].state, "accepted");
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(controller.has_input(&record.id));
    let delivered = controller
        .history()
        .into_iter()
        .find(|message| message.input_id == record.id)
        .unwrap();
    assert_eq!(delivered.input_provenance.unwrap().source, InputSource::Tui);
    assert_eq!(
        delivered.input_provenance.unwrap().delivery,
        InputDelivery::Steering
    );
}

#[tokio::test]
async fn queued_id_is_held_during_wait_and_drains_after_wake_finishes() {
    let (_workspace, _sessions, controller, mut app) = local().await;
    app.enqueue(
        0,
        Request::Command(backend::Command::Rename("pending".into())),
    );
    app.composer.set_input("after children finish");
    local_enter(&mut app, &controller);
    let id = app.queued_input_id.clone().unwrap();
    app.writes.clear();
    app.start_operation("waiting for children");
    assert!(
        app.submit_queued_input(&Backend::Local(&controller), &CancellationToken::new())
            .is_none()
    );
    assert_eq!(app.queued_input_id.as_deref(), Some(id.as_str()));
    app.end_turn();
    let action = app.submit_queued_input(&Backend::Local(&controller), &CancellationToken::new());
    assert!(matches!(action, Some(Action::LocalPrompt { input_id, .. }) if input_id == id));
}

#[test]
fn identical_text_events_are_deduplicated_by_id_not_content() {
    let (mut app, _) = app();
    for id in ["first", "second", "first"] {
        app.apply_event(&WireEvent {
            event_type: USER_MESSAGE.into(),
            text: "same words".into(),
            input_id: id.into(),
            ..Default::default()
        });
    }
    let entries: Vec<_> = app
        .entries
        .iter()
        .filter(|entry| entry.kind == Some(EntryKind::User))
        .collect();
    assert_eq!(entries.len(), 2);
    assert_ne!(entries[0].id, entries[1].id);
}

#[tokio::test]
async fn admission_rejection_keeps_id_and_recoverable_text() {
    let (_workspace, _sessions, controller, mut app) = local().await;
    app.start_turn();
    let text = "x".repeat(32_001);
    app.composer.set_input(&text);
    local_enter(&mut app, &controller);
    let record = controller.input_submissions().unwrap().pop().unwrap();
    let (ticket, request) = app.requests[0].clone();
    let mut driver = Driver::new(backend::Connection::Local(controller.clone()));
    driver.submit(ticket, request);
    app.apply_completion(driver.next().await);
    assert_eq!(
        controller.input_submissions().unwrap()[0].state,
        "failed admission"
    );
    assert!(
        app.entries
            .iter()
            .any(|entry| entry.raw.contains(&record.id) && entry.raw.contains("admission failed"))
    );
    assert!(!controller.has_input(&record.id));
    assert_eq!(controller.input_submissions().unwrap()[0].text, text);
}
