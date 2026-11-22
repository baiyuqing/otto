use super::*;

fn status(ui: &mut Ui<'_>) -> String {
    let mut bytes = Vec::new();
    ui.app.program_status.flush(&mut bytes);
    String::from_utf8(bytes).unwrap()
}
fn record(state: &str) -> String {
    format!("\x1b]7501;state={state}:app=otto\x1b\\")
}

#[tokio::test]
async fn attached_frames_approval_and_disconnect_report_real_lifecycle() {
    let remote = Remote::offline(vec![]);
    let cancel = CancellationToken::new();
    let mut ui = attached_ui(&remote, &cancel).await;
    assert_eq!(status(&mut ui), record("idle"));
    ui.app.start_turn();
    ui.on_frame(Ok(Some(WireEvent {
        event_type: "approval_requested".into(),
        approval_id: "synthetic".into(),
        ..Default::default()
    })));
    assert_eq!(
        status(&mut ui),
        record("working") + &record("blocked:kind=permission")
    );
    ui.on_frame(Ok(Some(WireEvent {
        event_type: "approval_decided".into(),
        approval_id: "synthetic".into(),
        ..Default::default()
    })));
    assert_eq!(status(&mut ui), record("working"));
    ui.disconnect();
    assert_eq!(status(&mut ui), record("idle"));
}

#[tokio::test]
async fn attached_completion_survives_later_disconnect() {
    let remote = Remote::offline(vec![]);
    let cancel = CancellationToken::new();
    let mut ui = attached_ui(&remote, &cancel).await;
    status(&mut ui);
    ui.app.start_turn();
    status(&mut ui);
    ui.app
        .finish_turn(otto_platform::program_status::Completion::Succeeded);
    assert_eq!(status(&mut ui), record("done"));
    ui.disconnect();
    assert!(status(&mut ui).is_empty());
}

#[tokio::test]
async fn attached_compaction_error_and_cancellation_are_not_success() {
    let remote = Remote::offline(vec![]);
    let cancel = CancellationToken::new();
    let mut ui = attached_ui(&remote, &cancel).await;
    status(&mut ui);
    ui.app.start_turn();
    status(&mut ui);
    ui.on_compact(Err(Error::Unreachable("synthetic".into())));
    assert_eq!(status(&mut ui), record("error"));
    ui.app.start_turn();
    status(&mut ui);
    ui.on_compact(Ok(None));
    assert_eq!(status(&mut ui), record("idle"));
}
