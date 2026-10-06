use super::*;

#[test]
fn cancel_ends_a_running_prompt_with_cancelled() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    let initialized = client.initialize();
    assert_eq!(
        initialized["result"]["agentCapabilities"]["_meta"]["otto"]["taskControl"],
        true
    );
    let session_id = client.new_session(workspace.path());

    let id = client.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);
    // Ordinary text on a busy session steers the active prompt rather than
    // creating a hidden later turn or failing with "prompt already running".
    let (busy_frames, busy) = client.call("session/prompt", prompt(&session_id, "again"));
    assert_eq!(busy["result"]["stopReason"], "end_turn", "{busy}");
    assert!(busy_frames.iter().any(|frame| {
        frame["params"]["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Guidance accepted"))
    }));

    let (steer_frames, steered) = client.call(
        "session/prompt",
        prompt(&session_id, "/steer finish the provider request first"),
    );
    assert_eq!(steered["result"]["stopReason"], "end_turn", "{steered}");
    assert!(steer_frames.iter().any(|frame| {
        frame["params"]["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Guidance accepted"))
    }));
    let (_, extension) = client.call(
        "_otto/tasks/steer",
        json!({"sessionId":session_id,"text":"then inspect the result"}),
    );
    assert_eq!(extension["result"]["accepted"], true, "{extension}");

    let (queued_frames, queued) = client.call(
        "session/prompt",
        prompt(&session_id, "/queue inspect the completed run"),
    );
    assert_eq!(queued["result"]["stopReason"], "end_turn", "{queued}");
    assert!(queued_frames.iter().any(|frame| {
        frame["params"]["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Follow-up accepted"))
    }));

    client.send(
        json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session_id}}),
    );
    let (_, response) = client.finish(id, &mut |frame| panic!("unexpected request: {frame}"));
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn cancelling_an_active_tool_closes_its_display_as_failed_once() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, _) = provider_with(vec![tool_call_reply(
        "cancel-tool",
        "bash",
        r#"{"command":"sleep 60"}"#,
    )]);
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let sid = client.new_session(workspace.path());
    let id = client.start("session/prompt", prompt(&sid, "wait for the tool"));
    let mut frames = Vec::new();
    loop {
        let frame = client.recv();
        let started = frame["params"]["update"]["sessionUpdate"] == "tool_call"
            && frame["params"]["update"]["toolCallId"] == "cancel-tool";
        frames.push(frame);
        if started {
            break;
        }
    }
    client.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":sid}}));
    let (rest, response) = client.finish(id, &mut |frame| panic!("{frame}"));
    frames.extend(rest);
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_terminal_tool_update(&frames, "cancel-tool", "failed");
    let terminal = updates(&frames)
        .into_iter()
        .find(|update| {
            update["sessionUpdate"] == "tool_call_update" && update["toolCallId"] == "cancel-tool"
        })
        .unwrap();
    assert!(
        terminal["content"]
            .to_string()
            .to_lowercase()
            .contains("cancel"),
        "{terminal}"
    );
    assert_eq!(client.close(), Some(0));
}

#[test]
fn explicit_queue_acknowledges_then_runs_followups_in_fifo_order() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, served, release) = serve_gated(vec![
        text_reply("first answer"),
        text_reply("queued answer one"),
        text_reply("queued answer two"),
    ]);
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());

    let first_id = client.start("session/prompt", prompt(&session_id, "first task"));
    wait_for_count(&served, 1);
    for followup in ["follow-up one", "follow-up two"] {
        let (frames, response) = client.call(
            "session/prompt",
            prompt(&session_id, &format!("/queue {followup}")),
        );
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        assert!(message_text(&frames).contains("Follow-up accepted"));
    }

    release.send(()).unwrap();
    let (frames, response) = client.finish(first_id, &mut |frame| panic!("{frame}"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(message_text(&frames), "first answer");
    wait_for_count(&served, 3);

    let deadline = Instant::now() + TIMEOUT;
    let mut answers = Vec::new();
    while answers.len() < 2 {
        assert!(
            Instant::now() < deadline,
            "queued answers did not arrive: {answers:?}"
        );
        let frame = client.recv();
        if frame["method"] == "session/update"
            && frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
        {
            let text = frame["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap_or_default();
            if text.contains("queued answer") {
                answers.push(text.to_string());
            }
        }
    }
    assert_eq!(answers, ["queued answer one", "queued answer two"]);
    assert_eq!(client.close(), Some(0));
}

#[test]
fn stdin_eof_during_a_prompt_answers_cancelled_and_exits_zero() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());

    let id = client.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);
    client.stdin = None;
    let (_, response) = client.finish(id, &mut |frame| panic!("unexpected request: {frame}"));
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(client.close(), Some(0));
}

/// The first `.jsonl` at `root/<workspace key>/`; child transcripts live one
/// level deeper and are not matched.
fn find_session_file(root: &Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|workspace| std::fs::read_dir(workspace.path()).ok())
        .flat_map(|entries| entries.filter_map(Result::ok))
        .map(|entry| entry.path())
        .find(|path| {
            path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        })
}

fn sigterm(client: &Client) {
    let pid = nix::unistd::Pid::from_raw(client.child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).expect("send SIGTERM");
}

#[test]
fn sigterm_without_a_lease_cancels_the_prompt_and_exits_zero() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());

    let id = client.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);
    sigterm(&client);
    let (_, response) = client.finish(id, &mut |frame| panic!("unexpected request: {frame}"));
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn sigterm_with_a_lease_migrates_the_session() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure_with(home.path(), &base_url, true);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());

    let id = client.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);
    let session_path = find_session_file(&home.path().join(".otto/sessions"))
        .expect("the first prompt wrote the session file");
    sigterm(&client);
    let (_, response) = client.finish(id, &mut |frame| panic!("unexpected request: {frame}"));
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(client.close(), Some(0));

    let inbox = std::fs::read_to_string(session_path.with_extension("inbox.json")).expect("inbox");
    assert!(inbox.contains("This session was moved"), "{inbox}");
    let heartbeat = std::fs::read_to_string(session_path.with_extension("lease").join("heartbeat"))
        .expect("heartbeat");
    assert!(heartbeat.contains("\"released\":true"), "{heartbeat}");
}

#[test]
fn a_timer_wake_turn_reply_reaches_the_client_outside_any_prompt() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, _requests) = provider_with(vec![
        tool_call_reply("c1", "remind", r#"{"seconds":1,"message":"ping"}"#),
        text_reply("timer set"),
        text_reply("the timer fired"),
    ]);
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    let (_, response) = client.call("session/prompt", prompt(&session_id, "remind me"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

    assert_eq!(next_wake_reply(&mut client), "the timer fired");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn ordinary_input_during_a_wake_turn_steers_the_wake_turn() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // Request 2 is the wake turn's provider call.
    let (base_url, served, release) = serve_gated_at(
        vec![
            tool_call_reply("c1", "remind", r#"{"seconds":1,"message":"ping"}"#),
            text_reply("timer set"),
            text_reply("the timer fired"),
            text_reply("second answer"),
        ],
        2,
    );
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    let (_, response) = client.call("session/prompt", prompt(&session_id, "remind me"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    wait_for_count(&served, 3);

    let (frames, response) = client.call("session/prompt", prompt(&session_id, "meanwhile"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(
        message_text(&frames).contains("Guidance accepted for the active task"),
        "{frames:?}"
    );

    release.send(()).unwrap();
    let wake_reply = next_wake_reply(&mut client);
    assert!(wake_reply.contains("the timer fired"), "{wake_reply}");
    // Guidance is consumed by the wake turn at its next provider boundary.
    // It may require a follow-up provider request, but never creates an
    // independently queued ACP prompt.
    assert!(wake_reply.contains("second answer"), "{wake_reply}");
    assert_eq!(served.load(Ordering::SeqCst), 4);
    assert_eq!(client.close(), Some(0));
}
