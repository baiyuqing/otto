use super::*;

fn output(status: &mut Status) -> String {
    let mut bytes = Vec::new();
    status.flush(&mut bytes);
    String::from_utf8(bytes).unwrap()
}
fn record(state: &str) -> String {
    format!("\x1b]7501;state={state}:app=otto\x1b\\")
}

#[test]
fn exact_records_are_deduplicated_and_completion_is_sticky() {
    let mut status = Status::default();
    assert_eq!(output(&mut status), record("idle"));
    status.set(State::Working);
    status.set(State::Working);
    status.finish(Completion::Succeeded);
    status.approvals(BTreeSet::new(), false);
    assert_eq!(output(&mut status), record("working") + &record("done"));
    assert!(output(&mut status).is_empty());
    status.set(State::Working);
    status.finish(Completion::Failed);
    assert_eq!(output(&mut status), record("working") + &record("error"));
    status.finish(Completion::Canceled);
    assert_eq!(output(&mut status), record("idle"));
}

#[test]
fn concurrent_approvals_resolve_without_panel_or_sensitive_payloads() {
    let mut status = Status::default();
    output(&mut status);
    status.set(State::Working);
    for id in ["private-one", "private-two"] {
        status.observe(&WireEvent {
            event_type: APPROVAL_REQUESTED.into(),
            approval_id: id.into(),
            ..Default::default()
        });
    }
    assert_eq!(
        output(&mut status),
        record("working") + &record("blocked:kind=permission")
    );
    status.observe(&WireEvent {
        event_type: APPROVAL_DECIDED.into(),
        approval_id: "private-one".into(),
        ..Default::default()
    });
    assert!(output(&mut status).is_empty());
    status.observe(&WireEvent {
        event_type: APPROVAL_DECIDED.into(),
        approval_id: "private-two".into(),
        ..Default::default()
    });
    assert_eq!(output(&mut status), record("working"));
}

#[test]
fn snapshots_reconcile_existing_approvals_and_new_sessions_clear_them() {
    let mut status = Status::default();
    output(&mut status);
    status.approvals(BTreeSet::from(["hidden".into()]), true);
    assert_eq!(output(&mut status), record("blocked:kind=permission"));
    status.approvals(BTreeSet::new(), true);
    assert_eq!(output(&mut status), record("working"));
    status.set(State::Idle);
    status.approvals(BTreeSet::new(), false);
    assert_eq!(output(&mut status), record("idle"));
}

#[test]
fn broken_writer_is_best_effort_and_disables_future_reports() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut status = Status::default();
    status.flush(&mut Broken);
    status.set(State::Working);
    assert!(output(&mut status).is_empty());
}

#[tokio::test]
async fn local_and_attached_app_lifecycle_feed_the_reporter() {
    // Local and attached turns share App's event and completion application.
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = otto::fixtures::controller(workspace.path(), sessions.path()).await;
    let mut app = crate::tui::App::new(&crate::tui::app::Backend::Local(&controller));
    assert_eq!(output(&mut app.program_status), record("idle"));
    app.start_turn();
    assert_eq!(output(&mut app.program_status), record("working"));
    app.apply_event(&WireEvent {
        event_type: APPROVAL_REQUESTED.into(),
        approval_id: "synthetic".into(),
        ..Default::default()
    });
    assert_eq!(
        output(&mut app.program_status),
        record("blocked:kind=permission")
    );
    app.apply_event(&WireEvent {
        event_type: APPROVAL_DECIDED.into(),
        approval_id: "synthetic".into(),
        ..Default::default()
    });
    assert_eq!(output(&mut app.program_status), record("working"));
    app.finish_turn(Completion::Succeeded);
    assert_eq!(output(&mut app.program_status), record("done"));
    app.enable_program_status::<ratatui::backend::TestBackend>();
    assert!(!app.program_status.enabled);
}
