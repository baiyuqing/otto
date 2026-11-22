use super::*;

#[tokio::test]
async fn attached_wire_frames_upsert_structured_approval_and_keep_literal_shell_escapes() {
    let remote = Remote::offline(vec![]);
    let cancel = CancellationToken::new();
    let mut ui = attached_ui(&remote, &cancel).await;
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
    // Inspect only the terminal protocol, never user-controlled request details.
    let mut reports = Vec::new();
    ui.app.program_status.flush(&mut reports);
    reports.clear();
    ui.app.start_turn();
    ui.app.program_status.flush(&mut reports);
    reports.clear();
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
        ui.app.program_status.flush(&mut reports);
        let expected = if state == otto_core::approval::State::Pending {
            b"\x1b]7501;state=blocked:kind=permission:app=otto\x1b\\".as_slice()
        } else {
            b"\x1b]7501;state=working:app=otto\x1b\\".as_slice()
        };
        assert_eq!(reports, expected);
        reports.clear();
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
