use super::*;

/// Spawns a client whose model proposes one memory, and returns the id of the
/// pending candidate.
fn client_with_pending_candidate(
    home: &Path,
    workspace: &Path,
    kind: &str,
) -> (Client, String, String) {
    let served = Arc::new(AtomicUsize::new(0));
    let arguments = format!(r#"{{"kind":"{kind}","key":"tabs","text":"prefers tabs"}}"#);
    let (base_url, _requests) = serve(Script {
        replies: vec![
            tool_call_reply("call-1", "remember", &arguments),
            // The advisor's request takes this reply; it is not valid advice.
            text_reply("no advice"),
            text_reply("queued"),
        ],
        served,
    });
    configure(home, &base_url);
    let mut client = Client::spawn(home, workspace, "off");
    let (_, initialized) = client.call("initialize", json!({"protocolVersion": 1}));
    assert_eq!(
        initialized["result"]["agentCapabilities"]["_meta"]["otto"]["memoryReview"],
        true
    );
    let session_id = client.new_session(workspace);
    let (_, response) = client.call("session/prompt", prompt(&session_id, "remember tabs"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let (_, pending) = client.call("_otto/memory/pending", json!({"sessionId": session_id}));
    let candidates = pending["result"]["candidates"].as_array().expect("list");
    assert_eq!(candidates.len(), 1, "{pending}");
    assert_eq!(candidates[0]["text"], "prefers tabs");
    assert_eq!(candidates[0]["origin"], "model");
    let id = candidates[0]["id"].as_str().expect("id").to_string();
    (client, session_id, id)
}

#[test]
fn opening_a_session_with_a_pending_candidate_sends_the_pending_notice() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (mut client, _, _) =
        client_with_pending_candidate(home.path(), workspace.path(), "preference");
    let (_, response) = client.call(
        "session/new",
        json!({"cwd": workspace.path(), "mcpServers": []}),
    );
    let second = response["result"]["sessionId"]
        .as_str()
        .expect("session id");
    let notice = loop {
        let frame = client.recv();
        if frame["params"]["update"]["_meta"]["otto"]["reflectionNotice"] == true {
            break frame;
        }
    };
    assert_eq!(notice["params"]["sessionId"], second, "{notice}");
    let text = notice["params"]["update"]["content"]["text"]
        .as_str()
        .expect("text");
    assert!(
        text.starts_with("1 memory proposal is pending review (since ")
            && text.contains("Run /memory review"),
        "{text}"
    );
    assert_eq!(client.close(), Some(0));
}

#[test]
fn memory_review_accepts_a_pending_candidate_only_on_the_clients_request() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (mut client, session_id, id) =
        client_with_pending_candidate(home.path(), workspace.path(), "preference");

    let review = |client: &mut Client, id: &str, decision: &str| {
        client
            .call(
                "_otto/memory/review",
                json!({"sessionId": session_id, "candidateId": id, "decision": decision}),
            )
            .1
    };
    let bad = review(&mut client, &id, "maybe");
    assert_eq!(bad["error"]["code"], -32602, "{bad}");
    let missing = review(&mut client, "nope", "accept");
    assert_eq!(missing["error"]["code"], -32002, "{missing}");

    let accepted = review(&mut client, &id, "accept");
    assert_eq!(accepted["result"]["decision"], "accept", "{accepted}");
    assert!(accepted["result"]["record"]["id"].is_string(), "{accepted}");
    let (_, after) = client.call("_otto/memory/pending", json!({"sessionId": session_id}));
    assert_eq!(after["result"]["candidates"], json!([]), "{after}");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn memory_review_reject_writes_no_record() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (mut client, session_id, id) =
        client_with_pending_candidate(home.path(), workspace.path(), "preference");
    let (_, rejected) = client.call(
        "_otto/memory/review",
        json!({"sessionId": session_id, "candidateId": id, "decision": "reject"}),
    );
    assert_eq!(rejected["result"]["decision"], "reject", "{rejected}");
    assert!(rejected["result"]["record"].is_null(), "{rejected}");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn memory_pending_pages_with_a_cursor_and_rejects_bad_paging() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, _requests) = serve(Script {
        replies: vec![
            tool_call_reply(
                "c1",
                "remember",
                r#"{"kind":"fact","key":"a","text":"first"}"#,
            ),
            text_reply("no advice"),
            tool_call_reply(
                "c2",
                "remember",
                r#"{"kind":"fact","key":"b","text":"second"}"#,
            ),
            text_reply("no advice"),
            text_reply("queued"),
        ],
        served: Arc::new(AtomicUsize::new(0)),
    });
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    let (_, done) = client.call("session/prompt", prompt(&session_id, "remember two"));
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");

    let page = |client: &mut Client, extra: Value| -> Value {
        let mut params = json!({"sessionId": session_id});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        client.call("_otto/memory/pending", params).1
    };
    let first = page(&mut client, json!({"limit": 1}));
    assert_eq!(
        first["result"]["candidates"].as_array().unwrap().len(),
        1,
        "{first}"
    );
    let cursor = first["result"]["nextCursor"].as_str().unwrap().to_string();
    assert!(!cursor.is_empty(), "{first}");
    let second = page(&mut client, json!({"limit": 1, "cursor": cursor}));
    assert_eq!(
        second["result"]["candidates"].as_array().unwrap().len(),
        1,
        "{second}"
    );
    assert_eq!(second["result"]["nextCursor"], "", "{second}");
    assert_ne!(
        first["result"]["candidates"][0]["id"],
        second["result"]["candidates"][0]["id"]
    );
    let everything = page(&mut client, json!({}));
    assert_eq!(
        everything["result"]["candidates"].as_array().unwrap().len(),
        2
    );

    for bad in [
        json!({"limit": 0}),
        json!({"limit": 51}),
        json!({"cursor": "not-a-cursor"}),
    ] {
        let response = page(&mut client, bad.clone());
        assert_eq!(response["error"]["code"], -32602, "{bad}: {response}");
    }
    assert_eq!(client.close(), Some(0));
}

#[test]
fn memory_review_of_a_decided_candidate_is_a_conflict() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (mut client, session_id, id) =
        client_with_pending_candidate(home.path(), workspace.path(), "preference");
    let params = json!({"sessionId": session_id, "candidateId": id, "decision": "accept"});
    let (_, first) = client.call("_otto/memory/review", params.clone());
    assert!(first["result"].is_object(), "{first}");
    let (_, again) = client.call("_otto/memory/review", params);
    assert_eq!(again["error"]["code"], -32011, "{again}");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn memory_methods_report_unavailable_memory_with_its_own_code() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    configure(home.path(), "http://127.0.0.1:1");
    let config = home.path().join(".config/otto/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}\n[memory]\nenabled = false\n")).unwrap();
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    let (_, pending) = client.call("_otto/memory/pending", json!({"sessionId": session_id}));
    assert_eq!(pending["error"]["code"], -32010, "{pending}");
    let (_, review) = client.call(
        "_otto/memory/review",
        json!({"sessionId": session_id, "candidateId": "x", "decision": "accept"}),
    );
    assert_eq!(review["error"]["code"], -32010, "{review}");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn memory_methods_reject_an_unknown_session() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    configure(home.path(), "http://127.0.0.1:1");
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let (_, response) = client.call(
        "_otto/memory/pending",
        json!({"sessionId": "0123456789abcdef0123456789abcdef"}),
    );
    assert_eq!(response["error"]["code"], -32002, "{response}");
    assert_eq!(client.close(), Some(0));
}
