use super::*;

fn review_reply(outcome: &str) -> String {
    text_reply(
        &json!({
            "risk": "medium",
            "user_authorization": "high",
            "outcome": outcome,
            "reason": "The user requested this exact fixture operation."
        })
        .to_string(),
    )
}

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

fn reviewed_escalation(use_script: bool) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let target = home.path().join("reviewed-effect.txt");
    let script = workspace.path().join("reviewed.sh");
    let script_body = format!("#!/bin/sh\nprintf 'effect\\n' >> '{}'\n", target.display());
    std::fs::write(&script, &script_body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let command = if use_script {
        "./reviewed.sh".to_string()
    } else {
        format!("printf 'effect\\n' >> '{}'", target.display())
    };
    let task = format!("Append exactly one effect line to {}.", target.display());
    let (base_url, requests) = serve(Script {
        replies: vec![
            tool_call_reply(
                "reviewed-call",
                "bash",
                &json!({"command": command, "sandbox_permissions": "require_escalated",
                    "justification": "Append the line the user requested outside the workspace."})
                .to_string(),
            ),
            review_reply("allow"),
            text_reply("done"),
        ],
        served: Arc::new(AtomicUsize::new(0)),
    });
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "seatbelt");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    // No explicit reviewer registration or mode setter: exercise native startup.
    let (frames, response) = client.call("session/prompt", prompt(&session_id, &task));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_terminal_tool_update(&frames, "reviewed-call", "completed");
    assert_eq!(
        message_text(&frames),
        "done",
        "review audit must not add assistant messages"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "effect\n");
    assert!(
        frames
            .iter()
            .all(|frame| frame["method"] != "session/request_permission")
    );
    let bodies = requests.lock().unwrap();
    assert_eq!(bodies.len(), 3, "{bodies:?}");
    let review: Value = serde_json::from_str(&bodies[1]).unwrap();
    assert!(
        review["tools"].as_array().is_none_or(Vec::is_empty),
        "{review}"
    );
    let context: Value =
        serde_json::from_str(review["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(
        context["trusted_user_messages"]
            .as_array()
            .unwrap()
            .contains(&json!(task)),
        "{context}"
    );
    let operation: Value =
        serde_json::from_str(context["operation"]["subject"]["details"].as_str().unwrap()).unwrap();
    assert_eq!(operation["command"], command, "{context}");
    if use_script {
        assert!(
            operation["native_facts"]["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["body"] == script_body),
            "the review must include the exact script payload: {operation}"
        );
    }
    drop(bodies);
    let (_, snapshot) = client.call("_otto/approvals/list", json!({"sessionId": session_id}));
    assert_eq!(snapshot["result"]["requests"], json!([]), "{snapshot}");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn auto_startup_reviews_and_executes_the_exact_escalated_call_once_without_a_dialog() {
    reviewed_escalation(false);
}

#[test]
fn auto_startup_reviews_the_frozen_script_and_executes_once_without_a_dialog() {
    reviewed_escalation(true);
}

#[test]
fn never_rejects_escalation_without_a_reviewer_or_permission_dialog() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let target = home.path().join("forbidden-effect.txt");
    let (base_url, requests) =
        serve(Script {
            replies: vec![
            tool_call_reply("never-call", "bash", &json!({
                "command": format!("printf effect > '{}'", target.display()),
                "sandbox_permissions": "require_escalated", "justification": "write the fixture"
            }).to_string()),
            text_reply("denied"),
        ],
            served: Arc::new(AtomicUsize::new(0)),
        });
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "seatbelt");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    let (_, policy) = client.call(
        "_otto/approval_policy/get",
        json!({"sessionId": session_id}),
    );
    let (_, changed) = client.call(
        "_otto/approval_policy/set",
        json!({
            "sessionId": session_id, "expectedRevision": policy["result"]["revision"],
            "policy": "never", "reviewer": "auto_review"
        }),
    );
    assert_eq!(changed["result"]["reviewer_active"], false, "{changed}");
    let (frames, response) =
        client.call("session/prompt", prompt(&session_id, "write the fixture"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(!target.exists());
    assert!(
        frames
            .iter()
            .all(|frame| frame["method"] != "session/request_permission")
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "NEVER must not call the reviewer"
    );
    assert_eq!(client.close(), Some(0));
}

#[test]
fn auto_review_ask_and_invalid_reply_prompt_only_after_review_then_apply_the_exact_call() {
    use std::os::unix::fs::PermissionsExt;
    for review in [review_reply("ask"), text_reply("invalid assessment")] {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let target = home.path().join("human-effect.txt");
        let script = workspace.path().join("fallback.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf 'effect\\n' >> '{}'\n", target.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (base_url, served, release) = serve_gated_at(
            vec![
                tool_call_reply(
                    "fallback-call",
                    "bash",
                    &json!({
                        "command":"./fallback.sh", "sandbox_permissions":"require_escalated",
                        "justification":"Append exactly one fixture line outside the workspace."
                    })
                    .to_string(),
                ),
                review,
                text_reply("done"),
            ],
            1,
        );
        configure(home.path(), &base_url);
        let mut client = Client::spawn(home.path(), workspace.path(), "seatbelt");
        client.initialize();
        let session_id = client.new_session(workspace.path());
        let id = client.start(
            "session/prompt",
            prompt(
                &session_id,
                &format!("Append exactly one effect line to {}.", target.display()),
            ),
        );
        let started = Instant::now();
        while served.load(Ordering::SeqCst) < 2 {
            assert!(
                started.elapsed() < TIMEOUT,
                "reviewer request did not arrive"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // The provider is held: no approval action may be offered during review.
        let (_, activity) = client.call("_otto/approvals/list", json!({"sessionId":session_id}));
        assert_eq!(
            activity["result"]["counts"]["human_pending"], 0,
            "{activity}"
        );
        assert_eq!(activity["result"]["counts"]["reviewing"], 1, "{activity}");
        assert!(!target.exists());
        release.send(()).unwrap();
        let mut permissions = 0;
        let (_, response) = client.finish(id, &mut |frame| {
            assert_eq!(frame["method"], "session/request_permission");
            assert!(
                !target.exists(),
                "the exact effect must wait for the human decision"
            );
            permissions += 1;
            json!({"outcome":{"outcome":"selected","optionId":"allow_once"}})
        });
        assert_eq!(
            permissions, 1,
            "review fallback must ask rather than wait invisibly"
        );
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "effect\n");
        assert_eq!(served.load(Ordering::SeqCst), 3);
        assert_eq!(client.close(), Some(0));
    }
}

#[test]
fn tightening_mode_while_a_card_is_open_rejects_both_stale_answers_and_reprompts() {
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
            review_reply("ask"),text_reply("done")
        ],served:served.clone()});
        configure(home.path(), &base_url);
        let mut client = Client::spawn(home.path(), workspace.path(), "seatbelt");
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
fn yolo_mode_approves_an_escalated_command_without_a_permission_request() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("elevated.txt"), "outside-seatbelt\n").unwrap();
    let (base_url, _) = serve(Script {
        replies: vec![escalated("call-1"), text_reply("done")],
        served: Arc::new(AtomicUsize::new(0)),
    });
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "seatbelt");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    let (_, set) = client.call(
        "_otto/approval_mode/set",
        json!({"sessionId": session_id, "mode": "yolo"}),
    );
    assert_eq!(set["result"]["mode"], "yolo", "{set}");
    assert_eq!(set["result"]["automatic"], true, "{set}");
    let (_, fetched) = client.call("_otto/approval_mode/get", json!({"sessionId": session_id}));
    assert_eq!(fetched["result"]["mode"], "yolo", "{fetched}");
    // A boolean that contradicts the mode is refused, not guessed.
    let (_, refused) = client.call(
        "_otto/approval_mode/set",
        json!({"sessionId": session_id, "mode": "yolo", "automatic": false}),
    );
    assert!(refused["error"].is_object(), "{refused}");
    let id = client.start("session/prompt", prompt(&session_id, "read the fixture"));
    let (frames, response) = client.finish(id, &mut |frame| {
        panic!("a YOLO session must not ask for a permission: {frame}")
    });
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(
        !frames
            .iter()
            .any(|frame| frame["method"] == "session/request_permission"),
        "{frames:?}"
    );
    assert!(
        serde_json::to_string(&frames)
            .unwrap()
            .contains("outside-seatbelt"),
        "the unsandboxed command must have run"
    );
    assert_eq!(client.close(), Some(0));
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
