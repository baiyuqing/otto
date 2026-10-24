use super::*;

#[test]
fn attached_wire_frames_upsert_structured_approval_and_keep_literal_shell_escapes() {
    let remote = Remote::offline(vec![]);
    let cancel = CancellationToken::new();
    let mut ui = Ui {
        remote: &remote,
        app: App::new(&Backend::Attach(&remote)),
        cancel: &cancel,
        followed: None,
        status: None,
        rows: vec![],
        last_row: None,
        last_done: None,
        fresh: false,
        compacting: None,
        connected: true,
        pending: None,
        epoch: 0,
        transcript: 0,
        needs_sync: false,
        cancel_after_send: false,
        actions: VecDeque::new(),
    };
    let mut request: otto_core::approval::Request = serde_json::from_value(serde_json::json!({
        "schema_version":1, "id":"attached-approval", "revision":1,
        "kind":"confirmation", "authority":"consent",
        "scope":{"workspace":"/work", "session_id":"fixture", "task_id":"", "run_id":""},
        "subject":{"source":"fixture", "id":"fixture", "version":"1",
            "details":serde_json::json!({"command":"printf '\\n'\n\techo attached"}).to_string()},
        "title":"Attached request", "reason":"Review command", "persistent":false,
        "blocking":false, "created_at":1, "expires_at":null, "state":"pending",
        "decision":null, "application":"not_started", "application_error":""
    }))
    .unwrap();
    for state in [
        otto_core::approval::State::Pending,
        otto_core::approval::State::Approved,
    ] {
        request.state = state;
        request.revision += 1;
        let wire = serde_json::to_string(&WireEvent {
            event_type: "approval_changed".into(),
            approval: Some(request.clone()),
            ..Default::default()
        })
        .unwrap();
        ui.on_frame(Ok(Some(serde_json::from_str(&wire).unwrap())));
    }
    assert_eq!(ui.app.entries.len(), 1);
    assert_eq!(ui.transcript, 2);
    let lines = crate::tui::transcript::lines(&ui.app.entries, false, 100);
    let text = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Attached request"));
    assert!(text.contains("approved"));
    assert!(text.contains("printf '\\n'\n"));
    assert!(text.contains("    echo attached"));
    assert!(!text.contains('\t'));
}
