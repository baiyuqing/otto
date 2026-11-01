use super::*;
fn call_holding_permissions(
    client: &mut Client,
    method: &str,
    params: Value,
    held: &mut Vec<Value>,
    frames: &mut Vec<Value>,
) -> Value {
    let id = client.start(method, params);
    loop {
        let frame = client.recv();
        if frame["id"] == json!(id) && frame.get("method").is_none() {
            return frame;
        }
        if frame["method"] == "session/request_permission" {
            held.push(frame.clone());
        }
        frames.push(frame);
    }
}

#[test]
fn relay_tightening_mode_while_a_card_is_open_rejects_both_stale_answers_and_reprompts() {
    use std::os::unix::fs::PermissionsExt;
    for old_choice in ["allow_once", "reject_once"] {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let target = home.path().join("revised-effect.txt");
        let script = workspace.path().join("reviewed.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf 'effect\\n' >> '{}'\n", target.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let served = Arc::new(AtomicUsize::new(0));
        let (base_url,_) = serve(Script {replies:vec![
            tool_call_reply("revised-call","bash",&json!({"command":"./reviewed.sh",
                "sandbox_permissions":"require_escalated","justification":"Append the requested fixture line outside the workspace."}).to_string()),
            text_reply(&json!({"risk":"medium","user_authorization":"high","outcome":"ask","reason":"Confirm this exact fixture operation."}).to_string()),text_reply("done")
        ],served:served.clone()});
        configure(home.path(), &base_url, None);
        let serve = Serve::start(home.path(), workspace.path(), "seatbelt");
        let mut client = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
        client.initialize();
        let session_id = client.new_session(workspace.path());
        let prompt_id = client.start(
            "session/prompt",
            prompt(
                &session_id,
                &format!("Append exactly one effect line to {}.", target.display()),
            ),
        );
        let mut initial_frames = Vec::new();
        let old = loop {
            let frame = client.recv();
            if frame["method"] == "session/request_permission" {
                break frame;
            }
            assert_ne!(
                frame["id"],
                json!(prompt_id),
                "prompt finished before the first permission: {frame}"
            );
            initial_frames.push(frame);
        };
        let old_metadata = &old["params"]["_meta"]["otto"];
        let old_revision = old_metadata["revision"].as_u64().unwrap();
        let mut held = Vec::new();
        let changed = call_holding_permissions(
            &mut client,
            "_otto/approval_mode/set",
            json!({"sessionId":session_id,"mode":"manual"}),
            &mut held,
            &mut initial_frames,
        );
        assert_eq!(changed["result"]["mode"], "manual", "{changed}");
        let activity = call_holding_permissions(
            &mut client,
            "_otto/approvals/list",
            json!({"sessionId":session_id}),
            &mut held,
            &mut initial_frames,
        );
        let pending = activity["result"]["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == old_metadata["approvalId"])
            .unwrap();
        assert!(
            pending["revision"].as_u64().unwrap() > old_revision,
            "request must actually be revised: {activity}"
        );
        assert!(!target.exists());
        client.send(json!({"jsonrpc":"2.0","id":old["id"],"result":{
            "outcome":{"outcome":"selected","optionId":old_choice}
        }}));
        let mut fresh_permissions = 0;
        let mut answer_fresh = |frame: &Value| {
            assert_eq!(frame["method"], "session/request_permission");
            let fresh = &frame["params"]["_meta"]["otto"];
            assert_eq!(fresh["approvalId"], old_metadata["approvalId"]);
            assert!(fresh["revision"].as_u64().unwrap() > old_revision);
            assert_eq!(fresh["subjectVersion"], old_metadata["subjectVersion"]);
            assert!(
                !target.exists(),
                "old {old_choice} must not authorize or reject the revised operation"
            );
            fresh_permissions += 1;
            json!({"outcome":{"outcome":"selected","optionId":"allow_once"}})
        };
        for frame in held {
            let result = answer_fresh(&frame);
            client.send(json!({"jsonrpc":"2.0","id":frame["id"],"result":result}));
        }
        let (frames, response) = client.finish(prompt_id, &mut answer_fresh);
        assert_eq!(fresh_permissions, 1);
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        initial_frames.extend(frames);
        assert_terminal_tool_update(&initial_frames, "revised-call", "completed");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "effect\n");
        assert_eq!(served.load(Ordering::SeqCst), 3);
        assert_eq!(client.close(), Some(0));
    }
}

#[test]
fn relay_auto_review_ask_becomes_a_permission_request_and_applies_once() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let target = home.path().join("relay-effect.txt");
    let script = workspace.path().join("reviewed.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf 'effect\\n' >> '{}'\n", target.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let (base_url,_) = serve(Script {replies:vec![
        tool_call_reply("relay-reviewed-call","bash",&json!({"command":"./reviewed.sh",
            "sandbox_permissions":"require_escalated","justification":"Append the requested fixture line outside the workspace."}).to_string()),
        text_reply(&json!({"risk":"medium","user_authorization":"medium","outcome":"ask",
            "reason":"A person must confirm this exact fixture operation."}).to_string()),text_reply("done")
    ],served:served.clone()});
    configure(home.path(), &base_url, None);
    let serve = Serve::start(home.path(), workspace.path(), "seatbelt");
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();
    let session_id = relay.new_session(workspace.path());
    let id = relay.start(
        "session/prompt",
        prompt(
            &session_id,
            &format!("Append exactly one effect line to {}.", target.display()),
        ),
    );
    let mut permissions = 0;
    let (frames, response) = relay.finish(id, &mut |frame| {
        assert_eq!(frame["method"], "session/request_permission");
        assert!(!target.exists());
        permissions += 1;
        json!({"outcome":{"outcome":"selected","optionId":"allow_once"}})
    });
    assert_eq!(
        permissions, 1,
        "a review ask must not wait invisibly in attach mode"
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_terminal_tool_update(&frames, "relay-reviewed-call", "completed");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "effect\n");
    assert_eq!(served.load(Ordering::SeqCst), 3);
    assert_eq!(relay.close(), Some(0));
}

fn manual_session(client: &mut Client, workspace: &Path) -> String {
    let session_id = client.new_session(workspace);
    let (_, response) = client.call(
        "_otto/approval_mode/set",
        json!({"sessionId": session_id, "automatic": false}),
    );
    assert_eq!(response["result"]["automatic"], false, "{response}");
    session_id
}

fn escalated(call_id: &str) -> String {
    let arguments = json!({
        "command": "cat \"$HOME/elevated.txt\"",
        "sandbox_permissions": "require_escalated",
        "justification": "read the reviewed home fixture",
    })
    .to_string();
    tool_call_reply(call_id, "bash", &arguments)
}

fn script() -> (Vec<String>, Arc<AtomicUsize>) {
    (
        vec![escalated("call-1"), text_reply("done")],
        Arc::new(AtomicUsize::new(0)),
    )
}

/// Creates `<home>/denied/fixture.txt`, the file a read grant test asks to read.
fn denied_read_dir(home: &Path) -> std::path::PathBuf {
    let denied_dir = home.join("denied");
    std::fs::create_dir(&denied_dir).unwrap();
    std::fs::write(denied_dir.join("fixture.txt"), "outside-seatbelt\n").unwrap();
    denied_dir
}

/// Appends a `[sandbox] deny_read_paths` entry for `denied_dir` to the written config.
fn deny_read_dir(home: &Path, denied_dir: &Path) {
    let config_path = home.join(".config/otto/config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!(
        "\n[sandbox]\ndeny_read_paths = [\"{}\"]\n",
        denied_dir.canonicalize().unwrap().display()
    ));
    std::fs::write(config_path, config).unwrap();
}

/// Whether the config holds a `read_paths` key; `deny_read_paths` does not count.
fn grants_read_paths(config: &str) -> bool {
    config
        .lines()
        .any(|line| line.trim_start().starts_with("read_paths"))
}

fn start_with_read(
    read_grant: bool,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Serve,
    Arc<AtomicUsize>,
) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("elevated.txt"), "outside-seatbelt\n").unwrap();
    let denied_dir = denied_read_dir(home.path());
    let (mut replies, served) = script();
    if read_grant {
        let path = denied_dir.join("fixture.txt").canonicalize().unwrap();
        let call = |id| {
            tool_call_reply(
                id,
                "bash",
                &json!({
                    "command": format!("cat '{}'", path.display()),
                    "sandbox_read_path": path,
                    "justification": "read the fixture",
                })
                .to_string(),
            )
        };
        replies = vec![call("call-1"), text_reply("done")];
    }
    let (base_url, _requests) = serve(Script {
        replies,
        served: Arc::clone(&served),
    });
    configure(home.path(), &base_url, None);
    deny_read_dir(home.path(), &denied_dir);
    let serve = Serve::start(home.path(), workspace.path(), "seatbelt");
    (home, workspace, serve, served)
}

fn start_with_replies(
    replies: Vec<String>,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Serve,
    Arc<AtomicUsize>,
) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let (base_url, _) = serve(Script {
        replies,
        served: Arc::clone(&served),
    });
    configure(home.path(), &base_url, None);
    let serve = Serve::start(home.path(), workspace.path(), "seatbelt");
    (home, workspace, serve, served)
}

fn pending_request(relay: &mut Client, session_id: &str) -> (u64, Value) {
    let prompt_id = relay.start("session/prompt", prompt(session_id, "run the command"));
    let mut frames = Vec::new();
    loop {
        let frame = relay.recv();
        if frame["method"] == "session/request_permission" {
            assert_eq!(frame["params"]["sessionId"], session_id);
            return (prompt_id, frame);
        }
        if frame["id"] == json!(prompt_id) && frame.get("method").is_none() {
            panic!("prompt ended before approval: {frame}; preceding: {frames:?}");
        }
        frames.push(frame);
    }
}

fn answer_dialogue(relay: &mut Client, session_id: &str, text: &str) -> (Vec<Value>, Value) {
    let dialogue_id = relay.start(
        "_otto/approvals/message",
        json!({"sessionId": session_id, "text": text}),
    );
    relay.finish(dialogue_id, &mut |frame| {
        panic!("unexpected request: {frame}")
    })
}

fn start() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Serve,
    Arc<AtomicUsize>,
) {
    start_with_read(false)
}

fn answered_by_relay(option: &str) -> (Value, Vec<Value>, usize) {
    let (home, workspace, serve, served) = start();
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();
    let session_id = manual_session(&mut relay, workspace.path());
    let id = relay.start("session/prompt", prompt(&session_id, "read the fixture"));
    let mut requests = Vec::new();
    let (frames, response) = relay.finish(id, &mut |frame| {
        assert_eq!(frame["method"], "session/request_permission");
        requests.push(frame.clone());
        json!({"outcome": {"outcome": "selected", "optionId": option}})
    });
    assert_eq!(requests.len(), 1);
    let params = &requests[0]["params"];
    assert_eq!(params["toolCall"]["toolCallId"], "call-1");
    assert_eq!(params["toolCall"]["title"], "cat \"$HOME/elevated.txt\"");
    assert_eq!(
        params["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["optionId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["allow_once", "reject_once"]
    );
    let served = served.load(Ordering::SeqCst);
    assert_eq!(relay.close(), Some(0));
    (response, frames, served)
}

#[test]
fn persistent_read_permission_survives_the_serve_relay() {
    for (option, granted) in [("allow_once", true), ("reject_once", false)] {
        let (home, workspace, serve, served) = start_with_read(true);
        let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
        relay.initialize();
        let session_id = manual_session(&mut relay, workspace.path());
        let id = relay.start("session/prompt", prompt(&session_id, "read fixture"));
        let mut requests = Vec::new();
        let (frames, response) = relay.finish(id, &mut |frame| {
            requests.push(frame.clone());
            json!({"outcome": {"outcome": "selected", "optionId": option}})
        });
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        assert_eq!(requests.len(), 1);
        let params = &requests[0]["params"];
        assert_eq!(params["toolCall"]["kind"], "read");
        assert_eq!(params["options"][0]["kind"], "allow_always");
        assert!(
            params["toolCall"]["title"]
                .as_str()
                .unwrap()
                .contains("Permanently")
        );
        assert_eq!(
            served.load(Ordering::SeqCst),
            2,
            "the prepared call must not require a model-generated retry"
        );
        let config = std::fs::read_to_string(home.path().join(".config/otto/config.toml")).unwrap();
        assert_eq!(grants_read_paths(&config), granted, "{config}");
        assert_eq!(
            serde_json::to_string(&frames)
                .unwrap()
                .contains("outside-seatbelt"),
            granted
        );
        assert_eq!(relay.close(), Some(0));
    }
}

#[test]
fn allow_once_executes_the_prepared_call_once_in_the_same_prompt() {
    let (response, frames, served) = answered_by_relay("allow_once");
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(served, 2);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["params"]["update"]["sessionUpdate"] == "tool_call")
            .count(),
        1,
        "approval must resume the exact original call, not dispatch a second call"
    );
    assert!(
        serde_json::to_string(&frames)
            .unwrap()
            .contains("outside-seatbelt")
    );
    assert!(agent_text(&frames).ends_with("done"));
}

#[test]
fn reject_once_ends_the_turn_without_a_retry() {
    let (response, frames, served) = answered_by_relay("reject_once");
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(served, 2, "the retry prompt must not reach the provider");
    assert_terminal_tool_update(&frames, "call-1", "failed");
    assert!(
        !serde_json::to_string(&frames)
            .unwrap()
            .contains("outside-seatbelt")
    );
}

#[test]
fn the_relay_reads_and_sets_the_session_mode() {
    let (home, workspace, serve, _) = start_with_read(false);
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();
    let session_id = relay.new_session(workspace.path());
    let (_, set) = relay.call(
        "_otto/approval_mode/set",
        json!({"sessionId": session_id, "mode": "yolo"}),
    );
    assert_eq!(set["result"]["mode"], "yolo", "{set}");
    let (_, fetched) = relay.call("_otto/approval_mode/get", json!({"sessionId": session_id}));
    assert_eq!(fetched["result"]["mode"], "yolo", "{fetched}");
    // The relay reads the mode the serve process persisted.
    let (status, body) = http(
        &serve.socket,
        "GET",
        &format!("/v1/sessions/{session_id}/approval-mode"),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["mode"],
        "yolo",
        "{body}"
    );
    assert_eq!(relay.close(), Some(0));
}

#[test]
fn a_decision_over_http_withdraws_the_open_permission_request() {
    let (home, workspace, serve, served) = start();
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();
    let session_id = manual_session(&mut relay, workspace.path());
    let id = relay.start("session/prompt", prompt(&session_id, "read the fixture"));
    let mut frames = Vec::new();
    let request = loop {
        let frame = relay.recv();
        if frame["method"] == "session/request_permission" {
            break frame;
        }
        frames.push(frame);
    };

    // Another client reads the approval id from the turn's events and
    // allows it.
    let (_, session) = http(
        &serve.socket,
        "GET",
        &format!("/v1/sessions/{session_id}"),
        None,
    );
    let session: Value = serde_json::from_str(&session).expect("session json");
    let turn_id = session["turn"]["id"]
        .as_str()
        .expect("running turn")
        .to_string();
    let events = read_until(
        &serve.socket,
        &format!("/v1/sessions/{session_id}/turns/{turn_id}/events"),
        "approval_requested",
    );
    let approval_id = events
        .split("\"approval_id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("approval id")
        .to_string();
    let (status, _) = http(
        &serve.socket,
        "POST",
        &format!("/v1/sessions/{session_id}/approvals/{approval_id}"),
        Some(r#"{"decision":"allow"}"#),
    );
    assert_eq!(status, 200);

    // The relay withdraws the request; its answer, sent late as a
    // denial, changes nothing: the prompt ends with the retry's text.
    let cancel = loop {
        let frame = relay.recv();
        if frame["method"] == "$/cancel_request" {
            break frame;
        }
        frames.push(frame);
    };
    assert_eq!(cancel["params"]["requestId"], request["id"], "{cancel}");
    relay.send(json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": {"outcome": {"outcome": "selected", "optionId": "reject_once"}},
    }));
    let (rest, response) = relay.finish(id, &mut |frame| panic!("{frame}"));
    frames.extend(rest);
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(agent_text(&frames).ends_with("done"), "{frames:?}");
    assert_eq!(served.load(Ordering::SeqCst), 2);
    assert_eq!(relay.close(), Some(0));
}

#[test]
fn question_and_ordinary_task_leave_the_permission_pending_until_answered() {
    let arguments = json!({
        "command": "printf ran > \"$HOME/otto-elevated-ran\"",
        "sandbox_permissions": "require_escalated",
        "justification": "write a marker only if approval is granted",
    })
    .to_string();
    let (home, workspace, serve, _) = start_with_replies(vec![
        tool_call_reply("call-1", "bash", &arguments),
        text_reply("It is waiting for approval."),
        tool_call_reply("control-call", "approval_queue", "{}"),
        text_reply("The task is queued."),
        text_reply("The approval was denied."),
    ]);
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();
    let session_id = manual_session(&mut relay, workspace.path());
    let (prompt_id, permission) = pending_request(&mut relay, &session_id);
    let other_session = relay.new_session(workspace.path());
    let (_, other) = relay.call(
        "_otto/approvals/message",
        json!({"sessionId": other_session, "text": "cancel it"}),
    );
    assert_eq!(other["result"], Value::Null, "{other}");

    let (question_frames, question) = answer_dialogue(&mut relay, &session_id, "What is waiting?");
    assert_eq!(
        question["result"]["text"], "It is waiting for approval.",
        "{question}"
    );
    assert_eq!(question["result"]["queued"], false, "{question}");
    assert!(
        !question_frames
            .iter()
            .any(|frame| frame["method"] == "$/cancel_request")
    );

    let (queue_frames, queued) = answer_dialogue(&mut relay, &session_id, "Do another task");
    assert_eq!(queued["result"]["text"], "The task is queued.", "{queued}");
    assert_eq!(queued["result"]["queued"], true, "{queued}");
    assert!(
        !queue_frames
            .iter()
            .any(|frame| frame["method"] == "$/cancel_request")
    );

    relay.send(json!({
        "jsonrpc": "2.0",
        "id": permission["id"],
        "result": {"outcome": {"outcome": "selected", "optionId": "reject_once"}},
    }));
    let (_, response) = relay.finish(prompt_id, &mut |frame| {
        panic!("unexpected request: {frame}")
    });
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(!home.path().join("otto-elevated-ran").exists());
    assert_eq!(relay.close(), Some(0));
}

#[test]
fn revoke_withdraws_the_request_and_a_late_card_response_cannot_run_it() {
    let arguments = json!({
        "command": "printf ran > \"$HOME/otto-elevated-ran\"",
        "sandbox_permissions": "require_escalated",
        "justification": "write a marker only if approval is granted",
    })
    .to_string();
    let (home, workspace, serve, _) = start_with_replies(vec![
        tool_call_reply("call-1", "bash", &arguments),
        tool_call_reply(
            "control-call",
            "approval_revoke",
            r#"{"id":"$EXACT_APPROVAL_ID"}"#,
        ),
        text_reply("Withdrawn."),
        text_reply("The request was withdrawn."),
    ]);
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();
    let session_id = manual_session(&mut relay, workspace.path());
    let (prompt_id, permission) = pending_request(&mut relay, &session_id);
    let other_session = relay.new_session(workspace.path());
    let (_, other) = relay.call(
        "_otto/approvals/message",
        json!({"sessionId": other_session, "text": "cancel it"}),
    );
    assert_eq!(other["result"], Value::Null, "{other}");
    let (mut frames, dialogue) = answer_dialogue(&mut relay, &session_id, "Cancel that command");
    assert_eq!(dialogue["result"]["text"], "Withdrawn.", "{dialogue}");
    let cancel = loop {
        if let Some(cancel) = frames
            .iter()
            .find(|frame| frame["method"] == "$/cancel_request")
        {
            break cancel.clone();
        }
        let frame = relay.recv();
        if frame["method"] == "$/cancel_request" {
            break frame;
        }
        frames.push(frame);
    };
    assert_eq!(cancel["params"]["requestId"], permission["id"], "{cancel}");

    relay.send(json!({
        "jsonrpc": "2.0",
        "id": permission["id"],
        "result": {"outcome": {"outcome": "selected", "optionId": "allow_once"}},
    }));
    let response = frames
        .iter()
        .find(|frame| frame.get("method").is_none() && frame["id"] == json!(prompt_id))
        .cloned()
        .unwrap_or_else(|| {
            relay
                .finish(prompt_id, &mut |frame| {
                    panic!("unexpected request: {frame}")
                })
                .1
        });
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(!home.path().join("otto-elevated-ran").exists());
    assert_eq!(relay.close(), Some(0));
}
