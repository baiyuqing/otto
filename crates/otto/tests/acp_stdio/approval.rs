use super::*;

fn manual_session(client: &mut Client, workspace: &Path) -> String {
    let session_id = client.new_session(workspace);
    let (_, response) = client.call(
        "_otto/approval_mode/set",
        json!({"sessionId": session_id, "automatic": false}),
    );
    assert_eq!(response["result"]["automatic"], false, "{response}");
    session_id
}

struct Run {
    frames: Vec<Value>,
    response: Value,
    permission_requests: Vec<Value>,
    served: usize,
    config: String,
}

fn run(option: &str, replies: Vec<String>) -> Run {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("elevated.txt"), "outside-seatbelt\n").unwrap();
    let read_path = home.path().join("elevated.txt").canonicalize().unwrap();
    let replies = replies
        .into_iter()
        .map(|reply| reply.replace("__READ_PATH__", read_path.to_str().unwrap()))
        .collect();
    let served = Arc::new(AtomicUsize::new(0));
    let (base_url, requests) = serve(Script {
        replies,
        served: Arc::clone(&served),
    });
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "seatbelt");
    client.initialize();
    let session_id = manual_session(&mut client, workspace.path());
    let id = client.start("session/prompt", prompt(&session_id, "read the fixture"));
    let mut permission_requests = Vec::new();
    let (frames, response) = client.finish(id, &mut |frame| {
        assert_eq!(frame["method"], "session/request_permission");
        permission_requests.push(frame.clone());
        json!({"outcome": {"outcome": "selected", "optionId": option}})
    });
    let served = served.load(Ordering::SeqCst);
    assert_eq!(client.close(), Some(0));
    for body in requests.lock().unwrap().iter() {
        assert_task_queue_capability(body);
    }
    Run {
        frames,
        response,
        permission_requests,
        served,
        config: std::fs::read_to_string(home.path().join(".config/otto/config.toml")).unwrap(),
    }
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

fn pending_request(client: &mut Client, session_id: &str) -> (u64, Value) {
    let prompt_id = client.start("session/prompt", prompt(session_id, "run the command"));
    let mut frames = Vec::new();
    loop {
        let frame = client.recv();
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

fn answer_dialogue(client: &mut Client, session_id: &str, text: &str) -> (Vec<Value>, Value) {
    let dialogue_id = client.start(
        "_otto/approvals/message",
        json!({"sessionId": session_id, "text": text}),
    );
    client.finish(dialogue_id, &mut |frame| {
        panic!("unexpected request: {frame}")
    })
}

fn start_with_replies(replies: Vec<String>) -> (tempfile::TempDir, tempfile::TempDir, Client) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, _) = serve(Script {
        replies,
        served: Arc::new(AtomicUsize::new(0)),
    });
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "seatbelt");
    client.initialize();
    (home, workspace, client)
}

#[test]
fn persistent_read_grant_executes_prepared_call_and_denial_does_not_write_config() {
    let read_call = |id| {
        tool_call_reply(
            id,
            "bash",
            &json!({
                "command": "cat '__READ_PATH__'",
                "sandbox_read_path": "__READ_PATH__",
                "justification": "read the fixture",
            })
            .to_string(),
        )
    };
    for option in ["allow_once", "reject_once"] {
        let result = run(option, vec![read_call("c1"), text_reply("done")]);
        assert_eq!(
            result.response["result"]["stopReason"], "end_turn",
            "{}",
            result.response
        );
        assert_eq!(result.permission_requests.len(), 1);
        let params = &result.permission_requests[0]["params"];
        assert_eq!(params["options"][0]["kind"], "allow_always");
        assert_eq!(params["toolCall"]["kind"], "read");
        assert!(
            params["toolCall"]["title"]
                .as_str()
                .unwrap()
                .contains("Permanently")
        );
        assert_eq!(
            result.served, 2,
            "the prepared call must not require a model-generated retry"
        );
        let granted = option == "allow_once";
        assert_eq!(
            result.config.contains("read_paths"),
            granted,
            "{}",
            result.config
        );
        assert!(!result.config.contains("excluded_commands"));
        assert_eq!(
            serde_json::to_string(&result.frames)
                .unwrap()
                .contains("outside-seatbelt"),
            granted
        );
    }
}

#[test]
fn allow_once_executes_the_prepared_call_once_in_the_same_prompt() {
    let result = run("allow_once", vec![escalated("call-1"), text_reply("done")]);
    assert_eq!(
        result.response["result"]["stopReason"], "end_turn",
        "{}",
        result.response
    );
    assert_eq!(result.permission_requests.len(), 1);
    let params = &result.permission_requests[0]["params"];
    assert_eq!(params["toolCall"]["toolCallId"], "call-1");
    assert_eq!(params["toolCall"]["title"], "cat \"$HOME/elevated.txt\"");
    assert_eq!(params["toolCall"]["kind"], "execute");
    let options: Vec<(&str, &str)> = params["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| {
            (
                option["optionId"].as_str().unwrap(),
                option["kind"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        options,
        [("allow_once", "allow_once"), ("reject_once", "reject_once")]
    );
    assert_eq!(result.served, 2);
    assert_eq!(
        result
            .frames
            .iter()
            .filter(|frame| frame["params"]["update"]["sessionUpdate"] == "tool_call")
            .count(),
        1,
        "approval must resume the exact original call, not dispatch a second call"
    );
    let text = serde_json::to_string(&result.frames).unwrap();
    assert!(text.contains("outside-seatbelt"), "{text}");
    assert_eq!(
        updates(&result.frames).last().unwrap()["content"]["text"],
        "done"
    );
}

#[test]
fn reject_once_ends_the_turn_without_a_retry() {
    let result = run("reject_once", vec![escalated("call-1"), text_reply("done")]);
    assert_eq!(
        result.response["result"]["stopReason"], "end_turn",
        "{}",
        result.response
    );
    assert_eq!(result.permission_requests.len(), 1);
    assert_terminal_tool_update(&result.frames, "call-1", "failed");
    assert_eq!(
        result.served, 2,
        "the retry prompt must not reach the provider"
    );
    let text = serde_json::to_string(&result.frames).unwrap();
    assert!(!text.contains("outside-seatbelt"), "{text}");
}

#[test]
fn question_and_ordinary_task_leave_the_permission_pending_until_answered() {
    let (home, workspace, mut client) = start_with_replies(vec![
        escalated("call-1"),
        text_reply("It is waiting for approval."),
        tool_call_reply("control-call", "approval_queue", "{}"),
        text_reply("The task is queued."),
        text_reply("The approval was denied."),
    ]);
    let session_id = manual_session(&mut client, workspace.path());
    let (prompt_id, permission) = pending_request(&mut client, &session_id);
    let other_session = client.new_session(workspace.path());
    let (_, other) = client.call(
        "_otto/approvals/message",
        json!({"sessionId": other_session, "text": "cancel it"}),
    );
    assert_eq!(other["result"], Value::Null, "{other}");

    let (question_frames, question) = answer_dialogue(&mut client, &session_id, "What is waiting?");
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

    let (queue_frames, queued) = answer_dialogue(&mut client, &session_id, "Do another task");
    assert_eq!(queued["result"]["text"], "The task is queued.", "{queued}");
    assert_eq!(queued["result"]["queued"], true, "{queued}");
    assert!(
        !queue_frames
            .iter()
            .any(|frame| frame["method"] == "$/cancel_request")
    );

    client.send(json!({
        "jsonrpc": "2.0",
        "id": permission["id"],
        "result": {"outcome": {"outcome": "selected", "optionId": "reject_once"}},
    }));
    let (_, response) = client.finish(prompt_id, &mut |frame| {
        panic!("unexpected request: {frame}")
    });
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(!home.path().join("otto-elevated-ran").exists());
    assert_eq!(client.close(), Some(0));
}

#[test]
fn revoke_withdraws_the_request_and_a_late_card_response_cannot_run_it() {
    let command = "printf ran > \"$HOME/otto-elevated-ran\"";
    let arguments = json!({
        "command": command,
        "sandbox_permissions": "require_escalated",
        "justification": "write a marker only if approval is granted",
    })
    .to_string();
    let (home, workspace, mut client) = start_with_replies(vec![
        tool_call_reply("call-1", "bash", &arguments),
        tool_call_reply(
            "control-call",
            "approval_revoke",
            r#"{"id":"$EXACT_APPROVAL_ID"}"#,
        ),
        text_reply("Withdrawn."),
        text_reply("The request was withdrawn."),
    ]);
    let session_id = manual_session(&mut client, workspace.path());
    let (prompt_id, permission) = pending_request(&mut client, &session_id);
    let other_session = client.new_session(workspace.path());
    let (_, other) = client.call(
        "_otto/approvals/message",
        json!({"sessionId": other_session, "text": "cancel it"}),
    );
    assert_eq!(other["result"], Value::Null, "{other}");
    let (mut frames, dialogue) = answer_dialogue(&mut client, &session_id, "Cancel that command");
    assert_eq!(dialogue["result"]["text"], "Withdrawn.", "{dialogue}");
    let cancel = loop {
        if let Some(cancel) = frames
            .iter()
            .find(|frame| frame["method"] == "$/cancel_request")
        {
            break cancel.clone();
        }
        let frame = client.recv();
        if frame["method"] == "$/cancel_request" {
            break frame;
        }
        frames.push(frame);
    };
    assert_eq!(cancel["params"]["requestId"], permission["id"]);

    client.send(json!({
        "jsonrpc": "2.0",
        "id": permission["id"],
        "result": {"outcome": {"outcome": "selected", "optionId": "allow_once"}},
    }));
    let response = frames
        .iter()
        .find(|frame| frame.get("method").is_none() && frame["id"] == json!(prompt_id))
        .cloned()
        .unwrap_or_else(|| {
            client
                .finish(prompt_id, &mut |frame| {
                    panic!("unexpected request: {frame}")
                })
                .1
        });
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(!home.path().join("otto-elevated-ran").exists());
    assert_eq!(client.close(), Some(0));
}
