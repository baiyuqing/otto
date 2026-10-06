use super::*;

#[test]
fn initialize_advertises_the_implemented_capabilities() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    configure(home.path(), "http://127.0.0.1:1");
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    let (_, response) = client.call("initialize", json!({"protocolVersion": 99}));
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        result["agentCapabilities"]["sessionCapabilities"]["list"],
        json!({})
    );
    assert_eq!(result["authMethods"], json!([]));
    assert_eq!(result["agentInfo"]["name"], "otto");
    assert_eq!(
        result["agentCapabilities"]["_meta"]["otto"]["approvalDialogue"],
        true
    );
    assert_eq!(
        result["agentCapabilities"]["_meta"]["otto"]["taskControl"],
        true
    );
    assert_eq!(client.close(), Some(0));
}

#[test]
fn approval_message_returns_null_without_a_pending_request() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    configure(home.path(), "http://127.0.0.1:1");
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    let initialized = client.initialize();
    assert_eq!(
        initialized["result"]["agentCapabilities"]["_meta"]["otto"]["approvalDialogue"],
        true
    );
    let session_id = client.new_session(workspace.path());
    let (_, response) = client.call(
        "_otto/approvals/message",
        json!({"sessionId": session_id, "text": "hello"}),
    );
    assert_eq!(response["result"], Value::Null, "{response}");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn task_control_extensions_return_json_without_session_updates() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    configure(home.path(), "http://127.0.0.1:1");
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());

    let (frames, control) = client.call(
        "_otto/tasks/control",
        json!({"sessionId":session_id,"text":"what can you do?","channel":"telegram"}),
    );
    assert!(
        frames.is_empty(),
        "extension replies use JSON only: {frames:?}"
    );
    assert_eq!(control["result"], Value::Null, "{control}");

    let (_, invalid) = client.call(
        "_otto/tasks/steer",
        json!({"sessionId":session_id,"text":"  "}),
    );
    assert_eq!(invalid["error"]["code"], -32602, "{invalid}");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn invalid_requests_get_the_specified_error_codes() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    configure(home.path(), "http://127.0.0.1:1");
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();

    let code = |client: &mut Client, method: &str, params: Value| -> i64 {
        let (_, response) = client.call(method, params);
        response["error"]["code"]
            .as_i64()
            .unwrap_or_else(|| panic!("expected an error: {response}"))
    };
    let cwd = json!(workspace.path());
    assert_eq!(
        code(
            &mut client,
            "session/new",
            json!({"cwd": elsewhere.path(), "mcpServers": []})
        ),
        -32602,
        "cwd outside the workspace"
    );
    assert_eq!(
        code(
            &mut client,
            "session/new",
            json!({"cwd": cwd, "mcpServers": [{"name": "x", "command": "y", "args": [], "env": []}]})
        ),
        -32602,
        "non-empty mcpServers"
    );
    assert_eq!(
        code(
            &mut client,
            "session/load",
            json!({"cwd": cwd, "sessionId": "NOT-HEX", "mcpServers": []})
        ),
        -32602,
        "non-hex session id"
    );
    assert_eq!(
        code(
            &mut client,
            "session/load",
            json!({"cwd": cwd, "sessionId": "0".repeat(32), "mcpServers": []})
        ),
        -32002,
        "unknown session id"
    );
    assert_eq!(
        code(&mut client, "session/prompt", prompt(&"1".repeat(32), "hi")),
        -32002,
        "prompt for a session this connection never loaded"
    );
    assert_eq!(code(&mut client, "session/bogus", json!({})), -32601);

    client.send(json!([1, 2]));
    let frame = client.recv();
    assert_eq!(frame["error"]["code"], -32600);
    let stdin = client.stdin.as_mut().unwrap();
    writeln!(stdin, "{{not json").unwrap();
    let frame = client.recv();
    assert_eq!(frame["error"]["code"], -32700);
    assert_eq!(frame["id"], Value::Null);
    assert_eq!(client.close(), Some(0));
}
