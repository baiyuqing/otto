use super::*;

#[test]
fn prompt_streams_updates_then_list_and_load_see_the_session() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("hello.txt"), "file-body\n").unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let (base_url, _requests) = serve(Script {
        replies: vec![
            tool_call_reply("call-1", "read", r#"{"path":"hello.txt"}"#),
            text_reply("all done"),
        ],
        served: Arc::clone(&served),
    });
    configure(home.path(), &base_url);

    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    let (frames, response) = client.call("session/prompt", prompt(&session_id, "read hello"));
    assert_terminal_tool_update(&frames, "call-1", "completed");
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let live = kinds(&frames);
    assert_eq!(live.first(), Some(&"tool_call"), "{frames:?}");
    assert!(live.contains(&"tool_call_update"));
    assert_eq!(live.last(), Some(&"agent_message_chunk"));
    let call = updates(&frames)[0];
    assert_eq!(call["toolCallId"], "call-1");
    assert_eq!(call["kind"], "read");
    assert_eq!(call["title"], "hello.txt");
    assert_eq!(call["status"], "in_progress");
    let done = updates(&frames)
        .into_iter()
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .unwrap();
    assert_eq!(done["status"], "completed");
    assert!(
        done["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("file-body")
    );
    assert_eq!(
        updates(&frames).last().unwrap()["content"]["text"],
        "all done"
    );

    let (_, listed) = client.call("session/list", json!({"cwd": workspace.path()}));
    let sessions = listed["result"]["sessions"].as_array().expect("sessions");
    let row = sessions
        .iter()
        .find(|row| row["sessionId"] == json!(session_id))
        .unwrap_or_else(|| panic!("session missing from {listed}"));
    assert_eq!(row["title"], "read hello");
    assert!(row["updatedAt"].is_string());
    assert_eq!(client.close(), Some(0));

    // A new process replays the stored history before the load response.
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let (frames, response) = client.call(
        "session/load",
        json!({"cwd": workspace.path(), "sessionId": session_id, "mcpServers": []}),
    );
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
    let replayed = updates(&frames);
    assert_eq!(replayed[0]["content"]["text"], "read hello");
    assert_eq!(replayed[1]["toolCallId"], "call-1");
    assert_eq!(replayed[2]["toolCallId"], "call-1");
    assert_eq!(replayed[3]["content"]["text"], "all done");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn session_new_and_load_are_followed_by_the_command_list() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, _requests) = provider_with(vec![text_reply("ok")]);
    configure(home.path(), &base_url);

    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let (session_id, frame) = client.new_session_with_commands(workspace.path());
    assert_eq!(frame["method"], "session/update", "{frame}");
    assert_eq!(frame["params"]["sessionId"], json!(session_id));
    let update = &frame["params"]["update"];
    assert_eq!(update["sessionUpdate"], "available_commands_update");
    let names: Vec<&Value> = update["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| &command["name"])
        .collect();
    assert_eq!(
        names,
        [
            &json!("compact"),
            &json!("context"),
            &json!("reflect"),
            &json!("skill"),
            &json!("steer"),
            &json!("queue"),
            &json!("auto")
        ]
    );
    assert_eq!(
        update["availableCommands"][0]["input"]["hint"],
        "optional focus"
    );
    let available_commands = update["availableCommands"].clone();
    // The session file exists only after a prompt; load it in a new process.
    client.call("session/prompt", prompt(&session_id, "hi"));
    assert_eq!(client.close(), Some(0));

    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let (_, response) = client.call(
        "session/load",
        json!({"cwd": workspace.path(), "sessionId": session_id, "mcpServers": []}),
    );
    assert!(response["result"].is_object(), "{response}");
    let frame = client.recv();
    assert_eq!(
        frame["params"]["update"]["sessionUpdate"], "available_commands_update",
        "{frame}"
    );
    assert_eq!(
        frame["params"]["update"]["availableCommands"], available_commands,
        "{frame}"
    );
    assert_eq!(client.close(), Some(0));
}

#[test]
fn context_command_answers_with_counts_and_sends_no_provider_request() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, requests) = provider_with(vec![text_reply("MARKER-ASSISTANT-BODY")]);
    configure(home.path(), &base_url);

    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    client.call("session/prompt", prompt(&session_id, "MARKER-USER-BODY"));
    assert_eq!(requests.lock().unwrap().len(), 1);
    let first: Value = serde_json::from_str(&requests.lock().unwrap()[0]).unwrap();
    let system = first["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(system.len() > 40, "system prompt: {system:?}");

    let (frames, response) = client.call("session/prompt", prompt(&session_id, " /context "));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let text = message_text(&frames);
    assert!(text.contains("Estimated next request: "), "{text}");
    assert!(text.contains("System prompt: "), "{text}");
    assert!(text.contains("Messages (2): "), "{text}");
    for hidden in [&system[..40], "MARKER-USER-BODY", "MARKER-ASSISTANT-BODY"] {
        assert!(!text.contains(hidden), "{hidden:?} leaked into {text}");
    }
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "/context called the provider"
    );
    assert_eq!(client.close(), Some(0));
}

#[test]
fn task_queue_capability_survives_compaction_and_session_reload() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, requests) = provider_with(vec![
        text_reply("first answer"),
        text_reply("second answer"),
        summary_reply("SUMMARY-TEXT"),
        text_reply("plain answer"),
        text_reply("answer after reload"),
    ]);
    configure(home.path(), &base_url);
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

    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    client.call("session/prompt", prompt(&session_id, "first question"));
    client.call("session/prompt", prompt(&session_id, "second question"));
    assert_eq!(requests.lock().unwrap().len(), 2);
    for body in &requests.lock().unwrap()[..2] {
        assert_task_queue_capability(body);
    }

    let (frames, response) = client.call(
        "session/prompt",
        prompt(&session_id, "/compact  keep-the-api-names"),
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(requests.lock().unwrap().len(), 3, "no summary request");
    assert!(requests.lock().unwrap()[2].contains("keep-the-api-names"));
    let text = message_text(&frames);
    assert!(
        text.starts_with("Compacted the session context: ") && text.contains(" tokens before"),
        "{text}"
    );
    assert!(!text.contains("SUMMARY-TEXT"), "{text}");

    let (frames, response) = client.call(
        "session/prompt",
        prompt(&session_id, "/contextual question"),
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert!(requests.lock().unwrap()[3].contains("/contextual question"));
    assert_eq!(message_text(&frames), "plain answer");
    assert_task_queue_capability(&requests.lock().unwrap()[3]);
    assert_eq!(client.close(), Some(0));

    let mut loaded = Client::spawn(home.path(), workspace.path(), "off");
    loaded.initialize();
    let (_, response) = loaded.call(
        "session/load",
        json!({"cwd": workspace.path(), "sessionId": session_id, "mcpServers": []}),
    );
    assert!(response["result"].is_object(), "{response}");
    let _commands = loaded.recv();
    loaded.call("session/prompt", prompt(&session_id, "after reload"));
    assert_eq!(requests.lock().unwrap().len(), 5);
    assert_task_queue_capability(&requests.lock().unwrap()[4]);
    assert_eq!(loaded.close(), Some(0));
}
