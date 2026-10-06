use super::*;

#[test]
fn a_relay_advertises_commands_and_runs_context_and_compact_on_serve() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, requests) = provider_with(vec![
        text_reply("MARKER-ASSISTANT-1"),
        text_reply("MARKER-ASSISTANT-2"),
        summary_reply("SUMMARY-TEXT"),
        text_reply("plain answer"),
    ]);
    configure(home.path(), &base_url, None);
    // A small recent-token budget makes the first turn summarizable; automatic
    // reflection would add a background provider request after /compact.
    let config = home.path().join(".config/otto/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        format!(
            "{text}\n[agent.compaction]\nkeep_recent_tokens = 1\n\n[reflection]\nauto = \"off\"\n"
        ),
    )
    .unwrap();
    let serve = Serve::start(home.path(), workspace.path(), "off");
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();

    let (session_id, frame) = relay.new_session_with_commands(workspace.path());
    let update = &frame["params"]["update"];
    assert_eq!(frame["params"]["sessionId"], json!(session_id), "{frame}");
    assert_eq!(
        update["sessionUpdate"], "available_commands_update",
        "{frame}"
    );
    assert_eq!(update["availableCommands"][0]["name"], "compact");
    assert_eq!(update["availableCommands"][1]["name"], "context");
    assert_eq!(update["availableCommands"][2]["name"], "reflect");
    assert_eq!(update["availableCommands"][3]["name"], "skill");
    assert_eq!(update["availableCommands"][4]["name"], "steer");
    assert_eq!(update["availableCommands"][5]["name"], "queue");
    assert_eq!(update["availableCommands"][6]["name"], "auto");

    let (_, first_response) = relay.call("session/prompt", prompt(&session_id, "MARKER-USER-1"));
    assert_eq!(
        first_response["result"]["stopReason"], "end_turn",
        "{first_response}"
    );
    let (_, second_response) = relay.call("session/prompt", prompt(&session_id, "MARKER-USER-2"));
    assert_eq!(
        second_response["result"]["stopReason"], "end_turn",
        "{second_response}"
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    let first: Value = serde_json::from_str(&requests.lock().unwrap()[0]).unwrap();
    let system = first["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(system.len() > 40, "system prompt: {system:?}");

    let (frames, response) = relay.call("session/prompt", prompt(&session_id, "/context"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let text = agent_text(&frames);
    assert!(text.contains("Estimated next request: "), "{text}");
    assert!(text.contains("Messages (4): "), "{text}");
    for hidden in [&system[..40], "MARKER-USER-1", "MARKER-ASSISTANT-2"] {
        assert!(!text.contains(hidden), "{hidden:?} leaked into {text}");
    }
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "/context called the provider"
    );

    let (frames, response) = relay.call("session/prompt", prompt(&session_id, "/compact keep-api"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(requests.lock().unwrap().len(), 3, "no summary request");
    assert!(requests.lock().unwrap()[2].contains("keep-api"));
    let text = agent_text(&frames);
    assert!(
        text.starts_with("Compacted the session context: ") && text.contains(" tokens before"),
        "{text}"
    );

    let (frames, response) = relay.call("session/prompt", prompt(&session_id, "/contextual x"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert!(requests.lock().unwrap()[3].contains("/contextual x"));
    assert_eq!(agent_text(&frames), "plain answer");

    // Past reflection runs are read from serve, and never reach the model.
    let (frames, response) = relay.call("session/prompt", prompt(&session_id, "/reflect list"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(agent_text(&frames), "No reflection runs yet.");
    let (frames, response) = relay.call(
        "session/prompt",
        prompt(&session_id, "/reflect show abcd1234"),
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(
        agent_text(&frames),
        "No reflection run abcd1234; /reflect list shows the recent ones."
    );
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert_eq!(relay.close(), Some(0));
}

#[test]
fn cancelling_attached_reflection_releases_server_admission() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, _) = provider_with(vec![text_reply("finished work")]);
    configure(home.path(), &base_url, None);
    let session_id = {
        let serve = Serve::start(home.path(), workspace.path(), "off");
        let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
        relay.initialize();
        let session_id = relay.new_session(workspace.path());
        relay.call("session/prompt", prompt(&session_id, "do useful work"));
        assert_eq!(relay.close(), Some(0));
        session_id
    };
    let (stalled_url, received) = serve_stalled();
    configure(home.path(), &stalled_url, None);
    let serve = Serve::start(home.path(), workspace.path(), "off");
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();
    let (_, loaded) = relay.call(
        "session/load",
        json!({
            "sessionId": session_id, "cwd": workspace.path(), "mcpServers": []
        }),
    );
    assert!(loaded["result"].is_object(), "{loaded}");
    let request = relay.start("session/prompt", prompt(&session_id, "/reflect"));
    wait_for_count(&received, 1);
    relay.send(
        json!({"jsonrpc":"2.0", "method":"session/cancel", "params":{"sessionId":session_id}}),
    );
    let (_, done) = relay.finish(request, &mut |frame| panic!("unexpected request: {frame}"));
    assert_eq!(done["result"]["stopReason"], "cancelled", "{done}");
    // A cancelled ACP response alone is insufficient: the server must have
    // stopped the reflection and released its admission before responding.
    let mut socket = UnixStream::connect(&serve.socket).unwrap();
    socket.set_read_timeout(Some(TIMEOUT)).unwrap();
    let body = r#"{"text":"next task","queue":false}"#;
    write!(socket, "POST /v1/sessions/{session_id}/turns HTTP/1.1\r\nHost: otto\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut status = String::new();
    BufReader::new(socket).read_line(&mut status).unwrap();
    assert!(
        status.starts_with("HTTP/1.1 200 "),
        "server still busy after reflection cancel: {status}"
    );
    assert_eq!(relay.close(), Some(0));
}

#[test]
fn a_relay_receives_the_reply_of_a_wake_turn_started_by_serve() {
    let (home, workspace, base_url) = fixture(vec![
        tool_call_reply("c1", "remind", r#"{"seconds":1,"message":"ping"}"#),
        text_reply("timer set"),
        text_reply("the timer fired"),
    ]);
    configure(home.path(), &base_url, None);
    let serve = Serve::start(home.path(), workspace.path(), "off");
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();
    let session_id = relay.new_session(workspace.path());
    let (_, response) = relay.call("session/prompt", prompt(&session_id, "remind me"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

    // The relay polls serve once a second; the update arrives outside any prompt.
    let text = loop {
        let frame = relay.recv();
        let update = &frame["params"]["update"];
        if update["_meta"]["otto"]["wakeReply"] == true {
            break update["content"]["text"].as_str().unwrap().to_string();
        }
    };
    assert_eq!(text, "the timer fired");
    assert_eq!(relay.close(), Some(0));
}
