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
    // A small recent-token budget makes the first turn summarizable.
    let config = home.path().join(".config/otto/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        format!("{text}\n[agent.compaction]\nkeep_recent_tokens = 1\n"),
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
    assert_eq!(update["availableCommands"][2]["name"], "model");
    assert_eq!(update["availableCommands"][3]["name"], "steer");
    assert_eq!(update["availableCommands"][4]["name"], "queue");
    assert_eq!(update["availableCommands"][5]["name"], "auto");
    assert_eq!(update["availableCommands"][6]["name"], "permissions");

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

#[test]
fn a_relay_lists_and_switches_profiles_on_serve() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, requests) = provider_with(vec![text_reply("plain answer")]);
    configure(home.path(), &base_url, None);
    add_profile(home.path(), "other", &base_url, "gpt-other");
    let serve = Serve::start(home.path(), workspace.path(), "off");
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();
    let session_id = relay.new_session(workspace.path());

    let (frames, response) = relay.call("session/prompt", prompt(&session_id, "/model"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(
        agent_text(&frames),
        "Profile: test\nProvider: openai-compatible\nModel: gpt-test\n\
         Configured profiles: other, test\nSwitch with /model <profile>."
    );

    let (frames, response) = relay.call("session/prompt", prompt(&session_id, "/model other"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(
        agent_text(&frames),
        "Switched to profile other (provider openai-compatible, model gpt-other)."
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        0,
        "/model called the provider"
    );

    // Serve's own session switched, not just the relay's view of it.
    let (frames, response) = relay.call("session/prompt", prompt(&session_id, "/model"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(
        agent_text(&frames).starts_with("Profile: other\n"),
        "{frames:?}"
    );

    // An unknown profile is an error on the relay, not a model request.
    let (_, response) = relay.call("session/prompt", prompt(&session_id, "/model nope"));
    assert!(response["error"].is_object(), "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nope"),
        "{response}"
    );
    assert_eq!(requests.lock().unwrap().len(), 0);
    assert_eq!(relay.close(), Some(0));
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
