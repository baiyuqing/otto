use super::*;

#[test]
fn prompt_streams_updates_then_list_and_load_see_the_session() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("hello.txt"), "file-body\n").unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let (base_url, requests) = serve(Script {
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
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "confined reads need no smart review"
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
fn approval_policy_revision_checks_legacy_presets_and_reload() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, requests) = provider_with(vec![text_reply("session saved")]);
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    let initialized = client.initialize();
    assert_eq!(
        initialized["result"]["agentCapabilities"]["_meta"]["otto"]["approvalPolicy"]["version"], 1,
        "{initialized}"
    );
    let session_id = client.new_session(workspace.path());
    client.call("session/prompt", prompt(&session_id, "save this session"));
    let get = |client: &mut Client| {
        client
            .call(
                "_otto/approval_policy/get",
                json!({"sessionId": session_id}),
            )
            .1
    };
    let initial = get(&mut client);
    assert_eq!(initial["result"]["policy"], "on_request", "{initial}");
    assert_eq!(initial["result"]["reviewer"], "auto_review", "{initial}");
    let revision = initial["result"]["revision"].as_u64().unwrap();
    let change = json!({"sessionId": session_id, "expectedRevision": revision,
        "policy": "never", "reviewer": "auto_review"});
    let (_, never) = client.call("_otto/approval_policy/set", change.clone());
    assert_eq!(never["result"]["policy"], "never", "{never}");
    assert_eq!(never["result"]["mode"], "auto", "{never}");
    assert_eq!(never["result"]["automatic"], true, "{never}");
    assert_eq!(never["result"]["reviewer_active"], false, "{never}");
    let (_, stale) = client.call("_otto/approval_policy/set", change);
    assert!(stale["error"].is_object(), "{stale}");
    assert_eq!(get(&mut client)["result"], never["result"]);
    assert_eq!(client.close(), Some(0));

    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    client.call(
        "session/load",
        json!({"cwd": workspace.path(), "sessionId": session_id, "mcpServers": []}),
    );
    assert_eq!(get(&mut client)["result"], never["result"]);
    let (_, manual) = client.call(
        "_otto/approval_mode/set",
        json!({"sessionId": session_id, "mode": "manual"}),
    );
    assert_eq!(manual["result"]["automatic"], false, "{manual}");
    let reset = get(&mut client);
    assert_eq!(reset["result"]["policy"], "on_request", "{reset}");
    assert_eq!(reset["result"]["reviewer"], "user", "{reset}");
    assert!(reset["result"]["revision"].as_u64().unwrap() > revision);
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "settings never need a provider"
    );
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
            &json!("model"),
            &json!("context"),
            &json!("compact"),
            &json!("steer"),
            &json!("queue"),
            &json!("permissions")
        ]
    );
    assert_eq!(
        update["availableCommands"][2]["input"]["hint"],
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

    let (frames, response) = client.call("session/prompt", prompt(&session_id, "/context "));
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
fn model_command_reports_and_switches_the_session_without_a_provider_request() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, requests) = provider_with(vec![text_reply("plain answer")]);
    configure(home.path(), &base_url);
    add_profile(home.path(), "other", &base_url, "gpt-other");

    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());

    let (frames, response) = client.call("session/prompt", prompt(&session_id, "/model"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(
        message_text(&frames),
        "Model test/gpt-test (type openai-compatible)\n\
         Configured models: other/gpt-other, test/gpt-test\n\
         Switch with /model <provider>/<model_id>."
    );

    let (frames, response) = client.call(
        "session/prompt",
        prompt(&session_id, "/model other/gpt-other"),
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(
        message_text(&frames),
        "Switched to model other/gpt-other (type openai-compatible)."
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        0,
        "/model called the provider"
    );

    // The switch is real for this session and nothing else changed.
    let (_, response) = client.call("session/prompt", prompt(&session_id, "hello"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let sent: Value = serde_json::from_str(&requests.lock().unwrap()[0]).unwrap();
    assert_eq!(sent["model"], json!("gpt-other"), "{sent}");
    let (frames, response) = client.call("session/prompt", prompt(&session_id, "/model"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(
        message_text(&frames).starts_with("Model other/gpt-other "),
        "{frames:?}"
    );
    let config = std::fs::read_to_string(home.path().join(".config/otto/config.toml")).unwrap();
    assert!(
        config.starts_with("default_profile = \"test\""),
        "the switch wrote the configuration file: {config}"
    );

    // A bare model id without a provider is an error, not a model request.
    let (_, response) = client.call("session/prompt", prompt(&session_id, "/model nope"));
    assert!(response["error"].is_object(), "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nope"),
        "{response}"
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "/model nope reached the provider"
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
    // A small recent-token budget makes the first turn summarizable.
    let config = home.path().join(".config/otto/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        format!("{text}\n[agent.compaction]\nkeep_recent_tokens = 1\n"),
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

#[test]
fn update_plan_sends_a_plan_update_and_load_replays_it() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, _requests) = provider_with(vec![
        tool_call_reply(
            "call-1",
            "update_plan",
            r#"{"steps":[{"text":"read","status":"completed"},{"text":"write","status":"in_progress"}]}"#,
        ),
        text_reply("working on it"),
    ]);
    configure(home.path(), &base_url);
    let entries = json!([
        {"content": "read", "priority": "medium", "status": "completed"},
        {"content": "write", "priority": "medium", "status": "in_progress"}
    ]);

    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    let (frames, response) = client.call("session/prompt", prompt(&session_id, "plan it"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let plans: Vec<&Value> = updates(&frames)
        .into_iter()
        .filter(|update| update["sessionUpdate"] == "plan")
        .collect();
    assert_eq!(plans.len(), 1, "{frames:?}");
    assert_eq!(plans[0]["entries"], entries);
    assert_eq!(client.close(), Some(0));

    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let (frames, response) = client.call(
        "session/load",
        json!({"cwd": workspace.path(), "sessionId": session_id, "mcpServers": []}),
    );
    assert!(response["result"].is_object(), "{response}");
    let replayed = updates(&frames);
    let last = replayed.last().expect("replayed updates");
    assert_eq!(last["sessionUpdate"], "plan", "{frames:?}");
    assert_eq!(last["entries"], entries);
    assert_eq!(client.close(), Some(0));
}

/// Adds a second configured profile to the test home so `/model` has a target.
fn add_profile(home: &Path, name: &str, base_url: &str, model: &str) {
    let config = home.join(".config/otto/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        format!(
            "{text}\n[profiles.{name}]\nprovider = \"openai-compatible\"\n\
             base_url = \"{base_url}\"\nmodel = \"{model}\"\napi_key_env = \"OTTO_API_KEY\"\n"
        ),
    )
    .unwrap();
}
