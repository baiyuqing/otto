//! Relays in different admitted workspaces of one `otto serve`, and memory
//! review through a relay.

use super::*;

/// Two relays open sessions in two admitted workspaces. Each session reads
/// its own workspace's files and model, histories stay apart, a session is
/// loaded only with its own workspace, and an unadmitted cwd is refused.
#[test]
fn relays_select_admitted_workspaces_without_crossing_history_or_tools() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = std::fs::canonicalize(workspace.path()).unwrap();
    let other = tempfile::tempdir().unwrap();
    let other_path = std::fs::canonicalize(other.path()).unwrap();
    std::fs::write(workspace_path.join("marker.txt"), "ONLY_A_MARKER").unwrap();
    std::fs::write(other_path.join("marker.txt"), "ONLY_B_MARKER").unwrap();
    std::fs::create_dir(other_path.join(".otto")).unwrap();
    std::fs::write(
        other_path.join(".otto/config.toml"),
        "default_model = \"test/workspace-b-model\"\n[providers.test.models.workspace-b-model]\n",
    )
    .unwrap();
    let (base_url, requests) = provider_with(vec![
        tool_call_reply("a", "read", r#"{"path":"marker.txt"}"#),
        text_reply("A done"),
        tool_call_reply("b", "read", r#"{"path":"marker.txt"}"#),
        text_reply("B done"),
        text_reply("concurrent"),
        text_reply("concurrent"),
    ]);
    configure(home.path(), &base_url, None);
    let serve = Serve::start(home.path(), &workspace_path, "off");
    let register = json!({"path": other_path, "trust": true}).to_string();
    let (status, body) = http(&serve.socket, "POST", "/v1/workspaces", Some(&register));
    assert_eq!(status, 201, "{body}");

    let mut a = Client::attach(home.path(), &workspace_path, Some(&serve.socket));
    let mut b = Client::attach(home.path(), &workspace_path, Some(&serve.socket));
    a.initialize();
    b.initialize();
    let id_a = a.new_session(&workspace_path);
    let id_b = b.new_session(&other_path);
    let (_, info) = http(&serve.socket, "GET", &format!("/v1/sessions/{id_b}"), None);
    let info: Value = serde_json::from_str(&info).unwrap();
    assert_eq!(info["workspace"], other_path.to_string_lossy().as_ref());
    assert_eq!(info["model"], "workspace-b-model");

    for (client, id, text) in [(&mut a, &id_a, "read A"), (&mut b, &id_b, "read B")] {
        let (_, reply) = client.call("session/prompt", prompt(id, text));
        assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
    }
    {
        let requests = requests.lock().unwrap();
        assert!(requests[1].contains("ONLY_A_MARKER") && !requests[1].contains("ONLY_B_MARKER"));
        assert!(requests[3].contains("ONLY_B_MARKER") && !requests[3].contains("ONLY_A_MARKER"));
    }

    // Both sessions run a turn at the same time.
    let left = a.start("session/prompt", prompt(&id_a, "only A followup"));
    let right = b.start("session/prompt", prompt(&id_b, "only B followup"));
    let (_, left) = a.finish(left, &mut |frame| panic!("unexpected request: {frame}"));
    let (_, right) = b.finish(right, &mut |frame| panic!("unexpected request: {frame}"));
    assert_eq!(left["result"]["stopReason"], "end_turn", "{left}");
    assert_eq!(right["result"]["stopReason"], "end_turn", "{right}");
    for (id, own, absent) in [
        (&id_a, "only A followup", "only B followup"),
        (&id_b, "only B followup", "only A followup"),
    ] {
        let (_, history) = http(
            &serve.socket,
            "GET",
            &format!("/v1/sessions/{id}/history"),
            None,
        );
        assert!(
            history.contains(own) && !history.contains(absent),
            "{history}"
        );
    }

    let (_, mismatch) = b.load_session(&other_path, &id_a);
    assert!(mismatch.get("error").is_some(), "{mismatch}");
    let (_, loaded) = a.load_session(&other_path, &id_b);
    assert_eq!(loaded["result"]["sessionId"], json!(id_b), "{loaded}");
    let (_, listed) = a.call("session/list", json!({"cwd": other_path}));
    let rows = listed["result"]["sessions"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["sessionId"] == json!(id_b)));
    assert!(!rows.iter().any(|row| row["sessionId"] == json!(id_a)));
    let (_, default_list) = a.call("session/list", json!({}));
    assert!(
        default_list["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["sessionId"] == json!(id_a))
    );
    let untrusted = tempfile::tempdir().unwrap();
    let untrusted_path = std::fs::canonicalize(untrusted.path()).unwrap();
    let (_, rejected) = a.call(
        "session/new",
        json!({"cwd": untrusted_path, "mcpServers": []}),
    );
    assert!(rejected.get("error").is_some(), "{rejected}");
    assert_eq!(a.close(), Some(0));
    assert_eq!(b.close(), Some(0));
}

/// A relay advertises memory review, lists the candidate a `remember` call
/// queued in its session, and records an accept through serve.
#[test]
fn a_relay_reviews_memory_candidates_of_its_session() {
    let (home, workspace, base_url) = fixture(vec![
        tool_call_reply(
            "call-memory",
            "remember",
            r#"{"kind":"preference","key":"tabs","text":"prefers tabs"}"#,
        ),
        text_reply("Queued."),
    ]);
    configure(home.path(), &base_url, None);
    let serve = Serve::start(home.path(), workspace.path(), "off");
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    let initialized = relay.initialize();
    assert_eq!(
        initialized["result"]["agentCapabilities"]["_meta"]["otto"]["memoryReview"], true,
        "{initialized}"
    );
    let session_id = relay.new_session(workspace.path());
    // MANUAL leaves the candidate for a human; AUTO would review it.
    let (_, manual) = relay.call(
        "_otto/approval_mode/set",
        json!({"sessionId": session_id, "mode": "manual"}),
    );
    assert!(manual.get("error").is_none(), "{manual}");
    let (_, reply) = relay.call("session/prompt", prompt(&session_id, "remember tabs"));
    assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");

    let (_, pending) = relay.call(
        "_otto/memory/pending",
        json!({"sessionId": session_id, "limit": 20}),
    );
    let candidates = pending["result"]["candidates"]
        .as_array()
        .unwrap_or_else(|| panic!("no candidates: {pending}"));
    assert_eq!(candidates.len(), 1, "{pending}");
    assert_eq!(candidates[0]["key"], "tabs");
    assert_eq!(candidates[0]["action"], "create");
    let candidate_id = candidates[0]["id"].as_str().unwrap().to_string();

    let (_, reviewed) = relay.call(
        "_otto/memory/review",
        json!({"sessionId": session_id, "candidateId": candidate_id, "decision": "accept"}),
    );
    assert_eq!(reviewed["result"]["decision"], "accept", "{reviewed}");
    assert_eq!(reviewed["result"]["candidateId"], json!(candidate_id));
    assert!(reviewed["result"]["record"]["id"].is_string(), "{reviewed}");

    let (_, after) = relay.call("_otto/memory/pending", json!({"sessionId": session_id}));
    assert_eq!(after["result"]["candidates"], json!([]), "{after}");
    // A session this relay did not open is not reviewed.
    let (_, foreign) = relay.call("_otto/memory/pending", json!({"sessionId": "0".repeat(32)}));
    assert_eq!(foreign["error"]["code"], -32002, "{foreign}");
    assert_eq!(relay.close(), Some(0));
}
