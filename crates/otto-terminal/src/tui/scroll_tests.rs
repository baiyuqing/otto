//! Scrolling must draw cached state even while a session read is blocked.

use super::*;
use ratatui::backend::TestBackend;
use std::time::Duration;

#[tokio::test]
async fn idle_draw_and_active_wheel_do_not_wait_for_session_storage() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let builder = otto::fixtures::builder(workspace.path(), sessions.path());
    let runtime = otto::fixtures::initial_runtime(&builder);
    let header = builder.create_session(&runtime).unwrap().header();
    let store = Arc::new(otto::session::Store::create_lazy(sessions.path(), header).unwrap());
    let session = otto::runtime::builder::SharedSession::new(store.clone());
    let runner = builder.build_runner(&session, &runtime).await.unwrap();
    let info = builder.runtime_info(&runtime);
    let controller = Arc::new(Controller::new(builder, true, session, runner, info));
    let backend = app::Backend::Local(&controller);
    let mut app = App::new(&backend);
    app.driver = Some(backend::Driver::new(backend::Connection::Local(
        controller.clone(),
    )));
    app.entries = vec![entries::Entry {
        kind: Some(entries::EntryKind::Assistant),
        raw: (0..100)
            .map(|row| format!("history row {row:03}\n"))
            .collect(),
        ..Default::default()
    }];
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    redraw(&mut terminal, &mut app).unwrap();
    let bottom = app.max_scroll.get();
    let original = terminal.backend().buffer().clone();

    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let storage = std::thread::spawn(move || {
        let _held = store.state.lock().unwrap();
        locked_tx.send(()).unwrap();
        // A regression can block the test thread, so release on a bounded
        // fallback and report that scrolling failed to release us itself.
        release_rx.recv_timeout(Duration::from_secs(5)).is_ok()
    });
    locked_rx.recv().unwrap();

    app.poll_local_status();
    assert_eq!(
        app.submit_queued_input(&backend, &CancellationToken::new()),
        None
    );
    apply_input(
        &mut app,
        &mut terminal,
        TuiEvent::Wheel { up: true, steps: 1 },
        &CancellationToken::new(),
        &backend,
    )
    .unwrap();
    redraw(&mut terminal, &mut app).unwrap();
    assert_eq!(app.scroll, Some(bottom - 1));
    assert_ne!(terminal.backend().buffer(), &original);

    app.start_turn();
    let (sender, mut keys) = mpsc::channel(1);
    let (_events, mut events) = mpsc::unbounded_channel();
    let progress = async {
        sender
            .send(TuiEvent::Wheel { up: true, steps: 1 })
            .await
            .unwrap();
        sender.send(TuiEvent::Redraw).await.unwrap();
        // Capacity returns only after both inputs have been received. The
        // UI draws each received event before it polls this future again.
        let _permit = sender.reserve().await.unwrap();
        Ok::<_, ()>(())
    };
    drive_turn(
        &mut app,
        &mut terminal,
        &mut keys,
        &mut events,
        &CancellationToken::new(),
        &backend,
        progress,
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(app.scroll, Some(bottom - 2));
    app.scroll = Some(0);
    for (up, steps) in [(true, 64), (false, 3)] {
        apply_input(
            &mut app,
            &mut terminal,
            TuiEvent::Wheel { up, steps },
            &CancellationToken::new(),
            &backend,
        )
        .unwrap();
    }
    assert_eq!(
        app.scroll,
        Some(3),
        "batching preserves steps and reversal at the top"
    );
    release_tx.send(()).ok();
    assert!(
        storage.join().unwrap(),
        "scrolling waited for the session-store lock"
    );
}
