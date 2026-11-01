//! ACP-over-TCP listener tests of `otto serve`.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_tcp_shares_http_sessions_and_shutdown_closes_idle_connections() {
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    let (provider, requests) = serve(Script {
        replies: vec![text_reply("direct ACP reply")],
        served: Arc::new(AtomicUsize::new(0)),
    });
    let mut fixture = fixture(&provider, "");
    fixture
        .environment
        .push(b"OTTO_ACP_TOKEN=test-acp-listener-token".to_vec());
    let cancel = CancellationToken::new();
    // No Unix listener: ACP dispatch must remain entirely in process.
    let (stdout, stderr, handle) = run_serve(
        &fixture,
        &[
            "--sandbox",
            "off",
            "--listen",
            "127.0.0.1:0",
            "--acp-listen",
            "127.0.0.1:0",
        ],
        cancel.clone(),
    )
    .await;
    let (base, http_token) = await_startup(&stdout, &stderr).await;
    let address = stdout
        .text()
        .lines()
        .find_map(|line| line.strip_prefix("otto serve: ACP ").map(str::to_string))
        .unwrap();
    let stream = TcpStream::connect(&address).await.unwrap();
    let mut acp = BufReader::new(stream);
    acp.get_mut()
        .write_all(b"Authorization: Bearer test-acp-listener-token\n")
        .await
        .unwrap();
    let messages = [
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":1, "clientCapabilities":{}}}),
        json!({"jsonrpc":"2.0", "id":2, "method":"session/new", "params":{"cwd":fixture.workspace, "mcpServers":[]}}),
    ];
    let mut sid = String::new();
    for (index, message) in messages.iter().enumerate() {
        acp.get_mut()
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(ACP_READ, acp.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], index + 1, "{reply}");
        assert!(reply.get("error").is_none(), "{reply}");
        if index == 0 {
            assert_eq!(
                reply["result"]["agentCapabilities"]["_meta"]["otto"]["memoryReview"],
                true
            );
            assert_eq!(
                reply["result"]["agentCapabilities"]["_meta"]["otto"]["taskControl"],
                true
            );
        } else {
            sid = reply["result"]["sessionId"].as_str().unwrap().to_string();
        }
    }
    let prompt = json!({"jsonrpc":"2.0", "id":3, "method":"session/prompt",
        "params":{"sessionId":sid, "prompt":[{"type":"text", "text":"hello TCP"}]}});
    acp.get_mut()
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .unwrap();
    let mut text = String::new();
    loop {
        let mut line = String::new();
        tokio::time::timeout(ACP_READ, acp.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        if reply["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
            text.push_str(
                reply["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap(),
            );
        }
        if reply["id"] == 3 {
            assert_eq!(reply["result"]["stopReason"], "end_turn");
            break;
        }
    }
    assert_eq!(text, "direct ACP reply");
    let captured: Value = serde_json::from_str(&requests.lock().unwrap()[0]).unwrap();
    let queue = captured["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "task_queue")
        .expect("default task_queue tool");
    assert_eq!(
        queue["function"]["parameters"]["required"],
        json!(["prompt"])
    );
    assert!(
        captured["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "system"
                    && message["content"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("task_control_policy")
            })
    );

    // Control extensions remain available with the default configuration,
    // and an idle session produces only a JSON reply, without a model call.
    for (id, method) in [(4, "_otto/tasks/control"), (5, "_otto/tasks/steer")] {
        let request = json!({"jsonrpc":"2.0", "id":id, "method":method,
            "params":{"sessionId":sid, "text":"investigate first"}});
        acp.get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(ACP_READ, acp.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], id, "{reply}");
        if id == 4 {
            assert!(reply.get("result").is_some_and(Value::is_null), "{reply}");
        } else {
            assert!(reply.get("error").is_some(), "{reply}");
        }
    }
    assert_eq!(requests.lock().unwrap().len(), 1);
    let response = otto::tls_provider::http_client()
        .get(format!("{base}/v1/sessions/{sid}/history"))
        .bearer_auth(&http_token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let history = response.text().await.unwrap();
    assert!(
        history.contains("hello TCP") && history.contains("direct ACP reply"),
        "{history}"
    );
    // Shutdown must wake both authenticated protocol readers and clients that
    // never finish authentication; neither should retain blocking threads.
    let mut unauthenticated = TcpStream::connect(&address).await.unwrap();
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(7), handle)
            .await
            .expect("serve shutdown hung on ACP")
            .unwrap(),
        0,
        "{}",
        stderr.text()
    );
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(1), acp.read_to_end(&mut buf))
        .await
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(1), unauthenticated.read_u8())
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        closed.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
    ));
    assert!(!stdout.text().contains("test-acp-listener-token"));
    assert!(!stderr.text().contains("test-acp-listener-token"));
}

async fn acp_rpc(
    stream: &mut tokio::io::BufReader<tokio::net::TcpStream>,
    request: serde_json::Value,
) -> serde_json::Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    stream
        .get_mut()
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    loop {
        let mut line = String::new();
        let read = tokio::time::timeout(ACP_READ, stream.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(read > 0, "ACP connection closed before replying");
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        if frame["id"] == request["id"] {
            return frame;
        }
        if frame["method"] == "session/update"
            && let Some(session) = request.pointer("/params/sessionId")
        {
            assert_eq!(
                &frame["params"]["sessionId"], session,
                "update crossed sessions"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_tcp_clients_select_admitted_workspaces_without_crossing_history_or_tools() {
    use serde_json::json;
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;
    let (provider, captured) = serve(Script {
        replies: vec![
            tool_call_reply("a", "read", r#"{"path":"marker.txt"}"#),
            text_reply("A done"),
            tool_call_reply("b", "read", r#"{"path":"marker.txt"}"#),
            text_reply("B done"),
            text_reply("concurrent"),
            text_reply("concurrent"),
        ],
        served: Arc::new(AtomicUsize::new(0)),
    });
    let mut fixture = fixture(&provider, "");
    fixture
        .environment
        .push(b"OTTO_ACP_TOKEN=workspace-test-token".to_vec());
    let other = tempfile::tempdir().unwrap();
    let other_path = std::fs::canonicalize(other.path()).unwrap();
    std::fs::write(
        std::path::Path::new(&fixture.workspace).join("marker.txt"),
        "ONLY_A_MARKER",
    )
    .unwrap();
    std::fs::write(other_path.join("marker.txt"), "ONLY_B_MARKER").unwrap();
    std::fs::create_dir(other_path.join(".otto")).unwrap();
    std::fs::write(
        other_path.join(".otto/config.toml"),
        "[profiles.test]\nmodel='workspace-b-model'\n",
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let (stdout, stderr, handle) = run_serve(
        &fixture,
        &[
            "--sandbox",
            "off",
            "--listen",
            "127.0.0.1:0",
            "--acp-listen",
            "127.0.0.1:0",
        ],
        cancel.clone(),
    )
    .await;
    let (base, token) = await_startup(&stdout, &stderr).await;
    let http = otto::tls_provider::http_client();
    let admitted = http
        .post(format!("{base}/v1/workspaces"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body(json!({"path":other_path,"trust":true}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(admitted.status(), 201);
    let address = stdout
        .text()
        .lines()
        .find_map(|line| line.strip_prefix("otto serve: ACP ").map(str::to_string))
        .unwrap();
    let mut a = BufReader::new(TcpStream::connect(&address).await.unwrap());
    let mut b = BufReader::new(TcpStream::connect(&address).await.unwrap());
    for stream in [&mut a, &mut b] {
        stream
            .get_mut()
            .write_all(b"Authorization: Bearer workspace-test-token\n")
            .await
            .unwrap();
        let reply = acp_rpc(stream, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}})).await;
        assert!(reply.get("error").is_none(), "{reply}");
    }
    let first = acp_rpc(&mut a, json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":fixture.workspace,"mcpServers":[]}})).await;
    let second = acp_rpc(&mut b, json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":other_path,"mcpServers":[]}})).await;
    let id_a = first["result"]["sessionId"].as_str().unwrap();
    let id_b = second["result"]["sessionId"].as_str().unwrap();
    let info: serde_json::Value = serde_json::from_str(
        &http
            .get(format!("{base}/v1/sessions/{id_b}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(info["workspace"], other_path.to_string_lossy().as_ref());
    assert_eq!(info["model"], "workspace-b-model");
    for (stream, id, text) in [(&mut a, id_a, "read A"), (&mut b, id_b, "read B")] {
        let reply = acp_rpc(stream, json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":id,"prompt":[{"type":"text","text":text}]}})).await;
        assert_eq!(reply["result"]["stopReason"], "end_turn");
    }
    let requests = captured.lock().unwrap().clone();
    assert!(requests[1].contains("ONLY_A_MARKER") && !requests[1].contains("ONLY_B_MARKER"));
    assert!(requests[3].contains("ONLY_B_MARKER") && !requests[3].contains("ONLY_A_MARKER"));
    let (left, right) = tokio::join!(
        acp_rpc(
            &mut a,
            json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":id_a,"prompt":[{"type":"text","text":"only A followup"}]}})
        ),
        acp_rpc(
            &mut b,
            json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":id_b,"prompt":[{"type":"text","text":"only B followup"}]}})
        )
    );
    assert_eq!(left["result"]["stopReason"], "end_turn");
    assert_eq!(right["result"]["stopReason"], "end_turn");
    for (id, own, absent) in [
        (id_a, "only A followup", "only B followup"),
        (id_b, "only B followup", "only A followup"),
    ] {
        let history = http
            .get(format!("{base}/v1/sessions/{id}/history"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            history.contains(own) && !history.contains(absent),
            "{history}"
        );
    }
    let mismatch = acp_rpc(&mut b, json!({"jsonrpc":"2.0","id":5,"method":"session/load","params":{"cwd":other_path,"sessionId":id_a,"mcpServers":[]}})).await;
    assert!(mismatch.get("error").is_some(), "{mismatch}");
    let loaded = acp_rpc(&mut a, json!({"jsonrpc":"2.0","id":5,"method":"session/load","params":{"cwd":other_path,"sessionId":id_b,"mcpServers":[]}})).await;
    assert_eq!(loaded["result"]["sessionId"], id_b);
    let listed = acp_rpc(
        &mut a,
        json!({"jsonrpc":"2.0","id":6,"method":"session/list","params":{"cwd":other_path}}),
    )
    .await;
    let rows = listed["result"]["sessions"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["sessionId"] == id_b));
    assert!(!rows.iter().any(|row| row["sessionId"] == id_a));
    let default_list = acp_rpc(
        &mut a,
        json!({"jsonrpc":"2.0","id":7,"method":"session/list","params":{}}),
    )
    .await;
    assert!(
        default_list["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["sessionId"] == id_a)
    );
    let untrusted = tempfile::tempdir().unwrap();
    let rejected = acp_rpc(&mut a, json!({"jsonrpc":"2.0","id":8,"method":"session/new","params":{"cwd":std::fs::canonicalize(untrusted.path()).unwrap(),"mcpServers":[]}})).await;
    assert!(rejected.get("error").is_some(), "{rejected}");
    drop(a);
    drop(b);
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .unwrap()
            .unwrap(),
        0,
        "{}",
        stderr.text()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_tcp_client_receives_the_reply_of_a_wake_turn_started_by_serve() {
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    let (provider, _requests) = serve(Script {
        replies: vec![
            tool_call_reply("c1", "remind", r#"{"seconds":1,"message":"ping"}"#),
            text_reply("timer set"),
            text_reply("the timer fired"),
        ],
        served: Arc::new(AtomicUsize::new(0)),
    });
    let mut fixture = fixture(&provider, "");
    fixture
        .environment
        .push(b"OTTO_ACP_TOKEN=test-acp-listener-token".to_vec());
    let cancel = CancellationToken::new();
    let (stdout, stderr, handle) = run_serve(
        &fixture,
        &[
            "--sandbox",
            "off",
            "--listen",
            "127.0.0.1:0",
            "--acp-listen",
            "127.0.0.1:0",
        ],
        cancel.clone(),
    )
    .await;
    let _ = await_startup(&stdout, &stderr).await;
    let address = stdout
        .text()
        .lines()
        .find_map(|line| line.strip_prefix("otto serve: ACP ").map(str::to_string))
        .unwrap();
    let mut acp = BufReader::new(TcpStream::connect(&address).await.unwrap());
    acp.get_mut()
        .write_all(b"Authorization: Bearer test-acp-listener-token\n")
        .await
        .unwrap();
    let mut sid = String::new();
    let requests = [
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":1, "clientCapabilities":{}}}),
        json!({"jsonrpc":"2.0", "id":2, "method":"session/new", "params":{"cwd":fixture.workspace, "mcpServers":[]}}),
        json!({"jsonrpc":"2.0", "id":3, "method":"session/prompt",
            "params":{"sessionId":"", "prompt":[{"type":"text", "text":"remind me"}]}}),
    ];
    let mut wake_reply = None;
    let mut prompt_done = false;
    for mut request in requests {
        if request["id"] == 3 {
            request["params"]["sessionId"] = json!(sid);
        }
        acp.get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let mut line = String::new();
            tokio::time::timeout(ACP_READ, acp.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["id"] == request["id"] {
                if request["id"] == 2 {
                    sid = frame["result"]["sessionId"].as_str().unwrap().to_string();
                }
                prompt_done = request["id"] == 3;
                break;
            }
        }
    }
    assert!(prompt_done);
    while wake_reply.is_none() {
        let mut line = String::new();
        tokio::time::timeout(ACP_READ, acp.read_line(&mut line))
            .await
            .expect("no wake reply in time")
            .unwrap();
        let frame: Value = serde_json::from_str(&line).unwrap();
        let update = &frame["params"]["update"];
        if update["_meta"]["otto"]["wakeReply"] == true {
            wake_reply = update["content"]["text"].as_str().map(str::to_string);
        }
    }
    assert_eq!(wake_reply.unwrap(), "the timer fired");
    cancel.cancel();
    assert_eq!(handle.await.unwrap(), 0, "{}", stderr.text());
}
