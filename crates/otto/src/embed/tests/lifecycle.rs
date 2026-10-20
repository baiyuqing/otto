//! Tests for the one-runtime guard, approvals and startup diagnostics.

use super::*;

#[tokio::test]
async fn only_one_otto_runs_per_process() {
    let _serial = LOCK.lock().await;
    let fixture = fixture("");
    let first = builder(&fixture).build().await.unwrap();
    assert!(matches!(
        builder(&fixture).build().await,
        Err(Error::AlreadyRunning)
    ));
    let clone = first.clone();
    first.close().await.unwrap();
    assert!(
        matches!(first.new_session(None).await, Err(Error::Closed)),
        "a closed runtime refuses new sessions"
    );
    first.close().await.expect("a second close is a no-op");
    drop(first);
    drop(clone);
    builder(&fixture)
        .build()
        .await
        .expect("a build after close succeeds")
        .close()
        .await
        .unwrap();

    let dropped = builder(&fixture).build().await.unwrap();
    drop(dropped);
    builder(&fixture)
        .build()
        .await
        .expect("a build after the last clone dropped succeeds")
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn a_prompt_that_needs_approval_calls_decide_and_a_denial_ends_the_turn() {
    let _serial = LOCK.lock().await;
    let (provider, script) = Installed::new(vec![
        tool_call(
            "request_approval",
            r#"{"title":"Publish","reason":"push branch","proposal":"git push"}"#,
        ),
        reply("understood"),
    ]);
    let fixture = fixture("");
    provider.serve(&fixture);
    let otto = builder(&fixture).build().await.unwrap();
    let session = otto.new_session(None).await.unwrap();
    assert_eq!(session.info().approval_mode, ApprovalMode::Auto);
    session.set_auto_mode(false).unwrap();
    assert_eq!(session.info().approval_mode, ApprovalMode::Manual);
    // The embedding API selects every mode. Assert it, then put the
    // session back in MANUAL: this round trip is about `decide` receiving
    // the request, and YOLO would approve it without calling `decide`.
    session.set_approval_mode(ApprovalMode::Yolo).unwrap();
    assert_eq!(session.info().approval_mode, ApprovalMode::Yolo);
    session.set_approval_mode(ApprovalMode::Manual).unwrap();
    assert_eq!(session.info().approval_mode, ApprovalMode::Manual);
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let asked = Arc::clone(&seen);
    let stop = session
        .prompt(
            Input::Text("push it"),
            &mut move |event| sink.lock().unwrap().push(event),
            &CancellationToken::new(),
            move |request| {
                asked.lock().unwrap().push(request);
                async { ApprovalDecision::Deny }
            },
        )
        .await
        .expect("prompt");
    assert!(matches!(stop, Stop::EndTurn));
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "decide runs once");
        assert!(
            seen[0].command.contains("git push"),
            "{:?}",
            seen[0].command
        );

        let requests = script.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "the denial result reaches the model");
        let result = requests[1]
            .messages
            .iter()
            .flat_map(|message| &message.blocks)
            .find(|block| block.block_type == BlockType::ToolResult)
            .expect("a tool result block");
        assert!(
            result.text.contains("\"approved\":false")
                || result.text.contains("\"approved\": false"),
            "{}",
            result.text
        );

        let events = events.lock().unwrap();
        let position = |wanted: fn(&Event) -> bool| events.iter().position(wanted);
        let started = position(|e| matches!(e, Event::AgentStarted)).expect("AgentStarted");
        let call = position(|e| matches!(e, Event::ToolCallStarted { .. })).expect("call started");
        let finished =
            position(|e| matches!(e, Event::ToolCallFinished { .. })).expect("call finished");
        let done = position(|e| matches!(e, Event::AgentFinished)).expect("AgentFinished");
        assert!(started < call && call < finished && finished < done);
    }
    drop(session);
    otto.close().await.unwrap();
}

/// The startup warnings the CLI test pins in
/// `startup_warnings_keep_their_order_and_text`, written to the
/// diagnostics writer.
#[tokio::test]
async fn diagnostics_receive_the_startup_warnings_the_cli_writes_to_stderr() {
    let _serial = LOCK.lock().await;
    let fixture = fixture("[memory]\nenabled = false\n[mcp]\nenabled = false\n");
    for store in ["usage.db", "tasks.db", "schedule.db"] {
        std::fs::create_dir_all(fixture.home.join(".otto").join(store)).unwrap();
    }
    std::fs::create_dir_all(fixture.workspace.join(".otto")).unwrap();
    std::fs::write(
        fixture.workspace.join(".otto/config.toml"),
        "[sandbox]\ndriver = \"seatbelt\"\n",
    )
    .unwrap();
    let capture = Capture::default();
    let otto = builder(&fixture)
        .diagnostics(Box::new(capture.clone()))
        .build()
        .await
        .unwrap();
    otto.close().await.unwrap();
    let text = capture
        .text()
        .replace(&*fixture.workspace.to_string_lossy(), "<workspace>");
    assert_eq!(
        text,
        "warning: sandbox in <workspace>/.otto/config.toml: field is restricted to user or managed configuration\n\
             warning: usage store unavailable, continuing without usage history\n\
             warning: task history store unavailable, continuing without task recording\n\
             warning: schedule store unavailable, continuing without scheduled deliveries\n\
             warning: sandbox is off; bash runs unsandboxed as your user\n"
    );
}
