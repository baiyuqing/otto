use super::*;

#[test]
fn cancelling_an_attached_active_tool_closes_its_display_as_failed_once() {
    let (home, workspace, base_url) = fixture(vec![tool_call_reply(
        "cancel-tool",
        "bash",
        r#"{"command":"sleep 60"}"#,
    )]);
    configure(home.path(), &base_url, None);
    let server = Serve::start(home.path(), workspace.path(), "off");
    let mut client = Client::attach(home.path(), workspace.path(), Some(&server.socket));
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
fn a_relayed_prompt_sends_the_same_updates_as_local_acp() {
    // Local `otto acp`.
    let (home, workspace, base_url) = fixture(read_script());
    configure(home.path(), &base_url, None);
    let mut local = Client::local(home.path(), workspace.path());
    local.initialize();
    let session_id = local.new_session(workspace.path());
    let (local_frames, response) = local.call("session/prompt", prompt(&session_id, "read hello"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(local.close(), Some(0));

    // The relay, with the socket taken from `[server].socket`.
    let (home, workspace, base_url) = fixture(read_script());
    let sockets = private_dir();
    let socket = sockets.path().join("o.sock");
    configure(home.path(), &base_url, Some(&socket));
    let _serve = Serve::start_with(home.path(), workspace.path(), "off", Some(&socket));
    let mut relay = Client::attach(home.path(), workspace.path(), None);
    let init = relay.initialize();
    assert_eq!(init["result"]["agentInfo"]["name"], "otto");
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        init["result"]["agentCapabilities"]["_meta"]["otto"]["approvalDialogue"], true,
        "the relay advertises approval dialogue: {init}"
    );
    assert_eq!(
        init["result"]["agentCapabilities"]["_meta"]["otto"]["taskControl"], true,
        "the relay advertises task control: {init}"
    );
    let session_id = relay.new_session(workspace.path());
    let (_, memory) = relay.call("_otto/memory/pending", json!({"sessionId": session_id}));
    assert_eq!(memory["error"]["code"], -32601, "{memory}");
    let (_, approval) = relay.call(
        "_otto/approvals/message",
        json!({"sessionId": session_id, "text": "hello"}),
    );
    assert_eq!(approval["result"], Value::Null, "{approval}");
    let (_, control) = relay.call(
        "_otto/tasks/control",
        json!({"sessionId":session_id,"text":"what can you do?","channel":"feishu"}),
    );
    assert_eq!(control["result"], Value::Null, "{control}");
    let (relay_frames, response) = relay.call("session/prompt", prompt(&session_id, "read hello"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

    assert_eq!(
        kinds(&relay_frames),
        ["tool_call", "tool_call_update", "agent_message_chunk"],
        "{relay_frames:?}"
    );
    assert_eq!(updates(&relay_frames), updates(&local_frames));
    assert_eq!(relay.close(), Some(0));
}

#[test]
fn session_load_replays_history_before_the_response_and_list_titles_follow_the_session() {
    let (home, workspace, base_url) = fixture(read_script());
    configure(home.path(), &base_url, None);
    let serve = Serve::start(home.path(), workspace.path(), "off");
    let mut first = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    first.initialize();
    let session_id = first.new_session(workspace.path());
    let (_, response) = first.call("session/prompt", prompt(&session_id, "read hello"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

    let title = |client: &mut Client| -> Value {
        let (_, listed) = client.call("session/list", json!({"cwd": workspace.path()}));
        let rows = listed["result"]["sessions"].as_array().expect("sessions");
        let row = rows
            .iter()
            .find(|row| row["sessionId"] == json!(session_id))
            .unwrap_or_else(|| panic!("session missing from {listed}"));
        assert!(row["updatedAt"].is_string(), "{row}");
        row["title"].clone()
    };
    assert_eq!(title(&mut first), "read hello");
    let (status, _) = http(
        &serve.socket,
        "PATCH",
        &format!("/v1/sessions/{session_id}"),
        Some(r#"{"name":"named session"}"#),
    );
    assert_eq!(status, 200);
    assert_eq!(title(&mut first), "named session");

    // A second relay replays the history, then answers.
    let mut second = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    second.initialize();
    let (frames, response) = second.load_session(workspace.path(), &session_id);
    assert_eq!(
        response["result"]["sessionId"],
        json!(session_id),
        "{response}"
    );
    assert_eq!(
        kinds(&frames),
        [
            "user_message_chunk",
            "tool_call",
            "tool_call_update",
            "agent_message_chunk"
        ],
        "{frames:?}"
    );
    assert_eq!(updates(&frames)[0]["content"]["text"], "read hello");
    assert_eq!(updates(&frames)[3]["content"]["text"], "all done");

    // An id serve has no file for is the error local `otto acp` returns.
    let (_, unknown) = second.load_session(workspace.path(), &"0".repeat(32));
    assert_eq!(unknown["error"]["code"], -32002, "{unknown}");
    assert_eq!(unknown["error"]["message"], "unknown sessionId");
    let (_, invalid) = second.load_session(workspace.path(), "NOT-HEX");
    assert_eq!(invalid["error"]["code"], -32602, "{invalid}");
    // Serve admission rejects an untrusted cwd with the ACP caller-error code.
    for (method, params) in [
        ("session/new", json!({"cwd":home.path(), "mcpServers":[]})),
        (
            "session/load",
            json!({"cwd":home.path(), "sessionId":session_id, "mcpServers":[]}),
        ),
        ("session/list", json!({"cwd":home.path()})),
    ] {
        let (_, elsewhere) = second.call(method, params);
        assert_eq!(elsewhere["error"]["code"], -32602, "{elsewhere}");
        assert!(
            elsewhere["error"]["message"]
                .as_str()
                .unwrap()
                .contains("outside the startup workspace")
        );
    }
    assert_eq!(first.close(), Some(0));
    assert_eq!(second.close(), Some(0));
}

#[test]
fn prompts_of_two_relays_on_one_session_queue_and_each_gets_only_its_own_text() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, received, release) = serve_gated(vec![
        text_reply("reply to first"),
        text_reply("reply to second"),
    ]);
    configure(home.path(), &base_url, None);
    let serve = Serve::start(home.path(), workspace.path(), "off");
    let mut first = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    first.initialize();
    let session_id = first.new_session(workspace.path());
    let mut second = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    second.initialize();
    // Nothing is stored yet, so the load finds the session open on serve.
    let (_, loaded) = second.load_session(workspace.path(), &session_id);
    assert_eq!(loaded["result"]["sessionId"], json!(session_id), "{loaded}");

    let first_id = first.start("session/prompt", prompt(&session_id, "first"));
    wait_for_count(&received, 1);
    let second_id = second.start("session/prompt", prompt(&session_id, "second"));
    // The second turn is queued behind the first, which is held at the
    // provider; give the request time to reach serve.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(received.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();

    let (first_frames, first_response) = first.finish(first_id, &mut |frame| panic!("{frame}"));
    let (second_frames, second_response) = second.finish(second_id, &mut |frame| panic!("{frame}"));
    assert_eq!(first_response["result"]["stopReason"], "end_turn");
    assert_eq!(second_response["result"]["stopReason"], "end_turn");
    assert_eq!(agent_text(&first_frames), "reply to first");
    assert_eq!(agent_text(&second_frames), "reply to second");
    assert_eq!(first.close(), Some(0));
    assert_eq!(second.close(), Some(0));
}

#[test]
fn session_cancel_ends_a_running_prompt_and_a_queued_one() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure(home.path(), &base_url, None);
    let serve = Serve::start(home.path(), workspace.path(), "off");
    let mut first = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    let initialized = first.initialize();
    assert_eq!(
        initialized["result"]["agentCapabilities"]["_meta"]["otto"]["taskControl"],
        true
    );
    let session_id = first.new_session(workspace.path());
    let mut second = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    second.initialize();
    second.load_session(workspace.path(), &session_id);

    let first_id = first.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);
    // A second prompt on the same relay session is refused as locally.
    let (_, busy) = first.call("session/prompt", prompt(&session_id, "again"));
    assert_eq!(busy["error"]["message"], "a prompt is already running");

    let (steer_frames, steered) = first.call(
        "session/prompt",
        prompt(&session_id, "/steer finish the provider request first"),
    );
    assert_eq!(steered["result"]["stopReason"], "end_turn", "{steered}");
    assert!(agent_text(&steer_frames).contains("Guidance accepted"));
    let (_, extension) = first.call(
        "_otto/tasks/steer",
        json!({"sessionId":session_id,"text":"then inspect the result"}),
    );
    assert_eq!(extension["result"]["accepted"], true, "{extension}");

    // `/queue` acknowledges separately and the server FIFO owns its turn.
    let second_id = second.start(
        "session/prompt",
        prompt(&session_id, "/queue inspect the current task"),
    );
    let (_, accepted_queue) = second.finish(second_id, &mut |frame| panic!("{frame}"));
    assert_eq!(
        accepted_queue["result"]["stopReason"], "end_turn",
        "{accepted_queue}"
    );
    second.cancel(&session_id);
    let canceled_notice = loop {
        let frame = second.recv();
        if frame["method"] == "session/update"
            && frame["params"]["update"]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Queued follow-up canceled"))
        {
            break frame;
        }
    };
    assert!(canceled_notice["params"]["sessionId"] == session_id);

    first.cancel(&session_id);
    let (_, response) = first.finish(first_id, &mut |frame| panic!("{frame}"));
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(first.close(), Some(0));
    assert_eq!(second.close(), Some(0));
}

#[test]
fn losing_serve_during_a_prompt_answers_an_error_and_exits_with_status_1() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure(home.path(), &base_url, None);
    let mut serve = Serve::start(home.path(), workspace.path(), "off");
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();
    let session_id = relay.new_session(workspace.path());
    let id = relay.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);

    serve.child.kill().expect("stop serve");
    let (_, response) = relay.finish(id, &mut |frame| panic!("{frame}"));
    assert_eq!(response["error"]["code"], -32603, "{response}");
    assert_eq!(
        response["error"]["message"], "connection to otto serve lost",
        "{response}"
    );
    assert_eq!(relay.wait(), Some(1));
}

#[test]
fn start_without_serve_prints_the_reason_and_exits_with_status_1() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    configure(home.path(), "http://127.0.0.1:1", None);
    let sockets = tempfile::tempdir().unwrap();
    let socket = sockets.path().join("absent.sock");
    let output = otto(home.path())
        .args(["acp", "--attach", "--cwd"])
        .arg(workspace.path())
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::null())
        .output()
        .expect("run otto acp --attach");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with(&format!(
            "otto serve is not reachable at {}: ",
            socket.display()
        )),
        "{stderr}"
    );
}
