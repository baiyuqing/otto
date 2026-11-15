use super::*;

#[test]
fn local_status_completion_requests_redraw_only_for_visible_changes() {
    let (mut app, _remote) = app();
    app.enqueue(lifecycle::LOCAL_STATUS_SURFACE, Request::LocalStatus);
    let (ticket, _) = app.requests.pop().unwrap();
    let unchanged = backend::LocalStatus {
        info: app.info.clone(),
        tasks: app.tasks.clone(),
        skill_names: app.skill_names.clone(),
        input_statuses: Vec::new(),
    };
    assert!(!app.apply_completion(Completion {
        ticket,
        mutation: false,
        result: Ok(Reply::LocalStatus(unchanged)),
    }));

    app.enqueue(lifecycle::LOCAL_STATUS_SURFACE, Request::LocalStatus);
    let (ticket, _) = app.requests.pop().unwrap();
    let changed = backend::LocalStatus {
        info: app.info.clone(),
        tasks: app.tasks.clone(),
        skill_names: vec!["new-skill".into()],
        input_statuses: Vec::new(),
    };
    assert!(app.apply_completion(Completion {
        ticket,
        mutation: false,
        result: Ok(Reply::LocalStatus(changed)),
    }));
}

#[tokio::test]
async fn background_status_is_bounded_refreshes_recovery_and_retries_after_failure() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = Arc::new(otto::fixtures::controller(workspace.path(), sessions.path()).await);
    let mut app = App::new(&Backend::Local(&controller));
    let record = controller
        .prepare_input(
            "recover this input",
            otto_core::model::InputDelivery::Direct,
        )
        .unwrap();
    controller
        .update_input_submission(&record.id, "accepted", None)
        .unwrap();
    for _ in 0..20 {
        app.poll_local_status();
    }
    assert_eq!(
        app.requests.len(),
        1,
        "slow status reads must not accumulate"
    );
    let (ticket, request) = app.requests.pop().unwrap();
    assert!(matches!(request, Request::LocalStatus));
    let mut driver = Driver::new(backend::Connection::Local(controller.clone()));
    driver.submit(ticket, request);
    assert!(app.apply_completion(driver.next().await));
    assert!(
        app.entries
            .iter()
            .any(|entry| entry.raw.contains(&record.id) && entry.raw.contains("accepted"))
    );
    let entry_count = app.entries.len();
    app.poll_local_status();
    let (ticket, request) = app.requests.pop().unwrap();
    driver.submit(ticket, request);
    assert!(
        !app.apply_completion(driver.next().await),
        "an unchanged status must not request a redraw"
    );
    assert_eq!(
        app.entries.len(),
        entry_count,
        "unchanged statuses are not repeated"
    );

    app.poll_local_status();
    let (ticket, _) = app.requests.pop().unwrap();
    assert!(!app.apply_completion(Completion {
        ticket,
        mutation: false,
        result: Err("read unavailable".into()),
    }));
    assert_eq!(
        app.entries.len(),
        entry_count,
        "keep the last good state on read failure"
    );
    app.poll_local_status();
    assert_eq!(app.requests.len(), 1, "the next timer tick retries");
}

#[tokio::test]
async fn background_status_rejects_old_scopes_and_generations_and_preserves_live_usage() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = otto::fixtures::controller(workspace.path(), sessions.path()).await;
    let mut app = App::new(&Backend::Local(&controller));
    app.poll_local_status();
    let (ticket, _) = app.requests.pop().unwrap();
    let mut status = backend::local_status(&controller);
    status.info.session_name = "stale result".into();
    app.invalidate_reads();
    app.apply_completion(Completion {
        ticket,
        mutation: false,
        result: Ok(Reply::LocalStatus(status)),
    });
    assert_ne!(app.info.session_name, "stale result");

    app.poll_local_status();
    let (ticket, _) = app.requests.pop().unwrap();
    let status = backend::local_status(&controller);
    app.info.session_id = "another-session".into();
    app.apply_completion(Completion {
        ticket,
        mutation: false,
        result: Ok(Reply::LocalStatus(status)),
    });
    assert_eq!(app.info.session_id, "another-session");

    app.info = controller.info();
    app.invalidate_reads();
    app.start_turn();
    app.usage.input_tokens = 1000;
    app.poll_local_status();
    let (ticket, _) = app.requests.pop().unwrap();
    let status = backend::local_status(&controller);
    app.apply_completion(Completion {
        ticket,
        mutation: false,
        result: Ok(Reply::LocalStatus(status)),
    });
    assert_eq!(
        app.usage.input_tokens, 1000,
        "an older snapshot cannot overwrite streamed usage"
    );

    app.poll_local_status();
    let (ticket, _) = app.requests.pop().unwrap();
    let status = backend::local_status(&controller);
    app.end_turn();
    app.apply_completion(Completion {
        ticket,
        mutation: false,
        result: Ok(Reply::LocalStatus(status)),
    });
    assert_eq!(
        app.usage.input_tokens, 1000,
        "turn end invalidates in-flight snapshots"
    );
}
