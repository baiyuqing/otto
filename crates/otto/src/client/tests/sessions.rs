//! Tests for session, turn, status and approval-policy calls.

use super::*;

#[tokio::test]
async fn open_session_omits_workspace_when_none() {
    let (client, seen, _dir) = stub(|_, _| json_body(StatusCode::OK, r#"{"id":"s1"}"#));
    assert_eq!(client.open_session(None, Some("old")).await.unwrap(), "s1");
    client.open_session(Some("/w"), None).await.unwrap();
    assert_eq!(
        request(&seen, 0),
        ("POST /v1/sessions".into(), json!({ "resume": "old" }))
    );
    assert_eq!(request(&seen, 1).1, json!({ "workspace": "/w" }));
}

#[tokio::test]
async fn history_sends_before_turn_only_when_given() {
    let (client, seen, _dir) = stub(|_, _| json_body(StatusCode::OK, "[]"));
    assert!(client.history("s1", Some("t9")).await.unwrap().is_empty());
    client.history("s1", None).await.unwrap();
    assert_eq!(
        request(&seen, 0).0,
        "GET /v1/sessions/s1/history?before_turn=t9"
    );
    assert_eq!(request(&seen, 1).0, "GET /v1/sessions/s1/history");
}

#[tokio::test]
async fn start_turn_sends_the_image_as_data_and_mime_type() {
    let frames = "id: 0\nevent: turn_end\ndata: {\"type\":\"turn_end\"}\n\n";
    let (client, seen, _dir) = stub(move |_, _| sse("t1", frames));
    let image = Block::image("QUJD", "image/png");
    let stream = client.start_turn("s1", "look", Some(&image)).await.unwrap();
    assert_eq!(stream.turn_id, "t1");
    client.start_turn("s1", "plain", None).await.unwrap();
    client
        .start_turn_with_channel(
            "s1",
            "chat text",
            None,
            Some(Channel::Feishu),
            Some(&Origin::chat("feishu", "oc_1").unwrap()),
            None,
            Some("chat:feishu:oc_1:m1"),
        )
        .await
        .unwrap();
    assert_eq!(
        request(&seen, 0),
        (
            "POST /v1/sessions/s1/turns".into(),
            json!({
                "text": "look",
                "queue": true,
                "origin": "terminal",
                "image": { "data": "QUJD", "mime_type": "image/png" }
            })
        )
    );
    assert_eq!(
        request(&seen, 1).1,
        json!({ "text": "plain", "queue": true, "origin": "terminal" })
    );
    assert_eq!(
        request(&seen, 2).1,
        json!({
            "text": "chat text",
            "queue": true,
            "channel": "feishu",
            "origin": "chat:feishu:oc_1",
            "input_id": "chat:feishu:oc_1:m1"
        })
    );
}

#[tokio::test]
async fn start_turn_sends_the_request_id_header_only_when_given() {
    let dir = tempfile::tempdir().expect("dir");
    let socket = dir.path().join("serve.sock");
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let log = Arc::clone(&seen);
    let app = Router::new().fallback(move |headers: axum::http::HeaderMap| {
        let log = Arc::clone(&log);
        async move {
            log.lock().unwrap().push(
                headers
                    .get("x-request-id")
                    .map(|value| value.to_str().unwrap().to_string()),
            );
            sse(
                "t1",
                "id: 0\nevent: turn_end\ndata: {\"type\":\"turn_end\"}\n\n",
            )
        }
    });
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let client = Client::new(&socket).expect("client");
    client
        .start_turn_with_channel("s1", "a", None, None, None, Some("req-1"), None)
        .await
        .unwrap();
    client.start_turn("s1", "b", None).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![Some("req-1".to_string()), None]);
}

#[tokio::test]
async fn turn_events_sends_after_and_takes_the_turn_id_from_the_path() {
    let frames = "id: 3\nevent: turn_end\ndata: {\"type\":\"turn_end\"}\n\n";
    let (client, seen, _dir) =
        stub(move |_, _| Response::builder().body(Body::from(frames)).unwrap());
    let mut stream = client.turn_events("s1", "t7", Some(2)).await.unwrap();
    assert_eq!(stream.turn_id, "t7");
    assert!(stream.next().await.unwrap().is_some());
    assert!(stream.next().await.unwrap().is_none());
    client.turn_events("s1", "t7", None).await.unwrap();
    assert_eq!(
        request(&seen, 0).0,
        "GET /v1/sessions/s1/turns/t7/events?after=2"
    );
    assert_eq!(request(&seen, 1).0, "GET /v1/sessions/s1/turns/t7/events");
}

#[tokio::test]
async fn status_yields_each_snapshot_then_none() {
    let frames = concat!(
        "event: status\ndata: {\"sessions\":[]}\n\n",
        "event: status\ndata: {\"sessions\":[{\"id\":\"s1\",\"workspace\":\"/w\",",
        "\"turn\":\"running\",\"turn_id\":\"t1\",\"queued\":2,\"approvals\":1,\"tasks\":3},",
        "{\"id\":\"s2\",\"workspace\":\"/w\",\"turn\":null,\"turn_id\":null,",
        "\"queued\":0,\"approvals\":0,\"tasks\":0}]}\n\n",
    );
    let (client, seen, _dir) =
        stub(move |_, _| Response::builder().body(Body::from(frames)).unwrap());
    let mut stream = client.status().await.unwrap();
    assert_eq!(stream.next().await.unwrap(), Some(vec![]));
    let rows = stream.next().await.unwrap().expect("second snapshot");
    assert_eq!(
        rows[0],
        StatusRow {
            auto_mode: false,
            approval_mode: None,
            id: "s1".into(),
            workspace: "/w".into(),
            turn: Some("running".into()),
            turn_id: Some("t1".into()),
            queued: 2,
            approvals: 1,
            tasks: 3,
        }
    );
    assert_eq!(rows[1].turn, None);
    assert_eq!(stream.next().await.unwrap(), None);
    assert_eq!(request(&seen, 0).0, "GET /v1/status");
}

#[tokio::test]
async fn legacy_mode_command_labels_actual_never_and_supports_old_boolean_servers() {
    let (client, seen, _dir) = stub(move |_, _| {
        json_body(
            StatusCode::OK,
            r#"{"mode":"auto","automatic":true,"policy":"never"}"#,
        )
    });
    assert_eq!(
        client.auto_mode_command("session", "").await.unwrap(),
        "Approval mode: NEVER (reviewer disabled)"
    );
    assert_eq!(
        request(&seen, 0).0,
        "GET /v1/sessions/session/approval-mode"
    );
    let (client, _, _dir) = stub(move |_, _| json_body(StatusCode::OK, r#"{"automatic":false}"#));
    assert_eq!(
        client.auto_mode_command("session", "").await.unwrap(),
        "Approval mode: MANUAL"
    );
}

#[tokio::test]
async fn policy_mutation_keeps_revision_and_actual_never_policy() {
    let (client, seen, _dir) = stub(move |_, _| {
        json_body(
            StatusCode::OK,
            r#"{"policy":"never","reviewer":"user","revision":8,"mode":"auto","automatic":true,"reviewer_active":false}"#,
        )
    });
    let settings = client
        .approval_policy(
            "session",
            Some((
                7,
                otto_core::approval::ApprovalPolicy::Never,
                otto_core::approval::ApprovalReviewer::User,
            )),
        )
        .await
        .unwrap();
    assert_eq!(settings.policy, otto_core::approval::ApprovalPolicy::Never);
    assert_eq!(
        settings.reviewer,
        otto_core::approval::ApprovalReviewer::User
    );
    assert_eq!(settings.revision, 8);
    let (path, body) = request(&seen, 0);
    assert_eq!(path, "PUT /v1/sessions/session/approval-policy");
    assert_eq!(
        body,
        json!({"expected_revision":7,"policy":"never","reviewer":"user"})
    );
}

#[tokio::test]
async fn approval_activity_accepts_legacy_snapshot_fields() {
    let (client, seen, _dir) =
        stub(move |_, _| json_body(StatusCode::OK, r#"{"requests":[],"cursor":9}"#));
    let activity = client.approval_activity("session").await.unwrap();
    assert_eq!(activity.cursor, 9);
    assert_eq!(activity.counts.human_pending, 0);
    assert_eq!(activity.counts.reviewing, 0);
    assert!(activity.recent_decisions.is_empty());
    assert_eq!(request(&seen, 0).0, "GET /v1/approvals?session_id=session");
}

#[tokio::test]
async fn session_maps_the_wire_into_info() {
    let body = json!({
        "id": "s1", "name": "work", "workspace": "/w", "provider": "chatgpt",
        "profile": "p", "model": "m", "thinking": "high",
        "context_window": 1000,
        "usage": { "input_tokens": 5, "output_tokens": 7 },
        "context_input_tokens": 42,
        "sandbox": { "mode": "seatbelt", "network": "denied", "bash_available": true,
                     "summary": "x" },
        "turn": null
    })
    .to_string();
    let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
    let info = client.session("s1").await.unwrap();
    assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1");
    assert_eq!(info.session_id, "s1");
    assert_eq!(info.session_name, "work");
    assert_eq!(info.model, "m");
    assert_eq!(info.context_window, 1000);
    assert_eq!(info.usage.input_tokens, 5);
    assert!(info.usage_present);
    assert_eq!(info.context_input_tokens, 42);
    assert!(info.context_input_tokens_present);
    assert!(!info.context_input_tokens_pending);
    assert!(info.session_path.is_empty());
    assert_eq!(info.sandbox.mode, SandboxMode::Seatbelt);
    assert_eq!(info.sandbox.network, SandboxNetwork::Denied);
    assert!(info.sandbox.bash_available);
}

#[tokio::test]
async fn session_maps_unknown_sandbox_strings_to_defaults() {
    let body =
        r#"{"id":"s1","sandbox":{"mode":"future","network":"future","bash_available":false}}"#;
    let (client, _seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, body));
    let info = client.session("s1").await.unwrap();
    assert_eq!(info.sandbox.mode, SandboxMode::default());
    assert_eq!(info.sandbox.network, SandboxNetwork::default());
    assert!(!info.usage_present);
    assert!(!info.context_input_tokens_present);
}

#[tokio::test]
async fn rename_session_patches_the_name() {
    let (client, seen, _dir) = stub(|_, _| json_body(StatusCode::OK, r#"{"id":"s1"}"#));
    client.rename_session("s1", "new name").await.unwrap();
    assert_eq!(
        request(&seen, 0),
        (
            "PATCH /v1/sessions/s1".into(),
            json!({ "name": "new name" })
        )
    );
}

#[tokio::test]
async fn fork_calls_post_the_fork_point_and_decode_the_new_session() {
    let (client, seen, _dir) = stub(|_, path| {
        if path.ends_with("/fork-points") {
            json_body(
                StatusCode::OK,
                r#"{"fork_points":[{"entry_id":"00000001","prompt":"first"}]}"#,
            )
        } else {
            json_body(StatusCode::CREATED, r#"{"id":"s2","prompt":"first"}"#)
        }
    });
    assert_eq!(
        client.fork_points("s1").await.unwrap(),
        [("00000001".to_string(), "first".to_string())]
    );
    assert_eq!(
        client.fork_session("s1", Some("00000001")).await.unwrap(),
        ("s2".to_string(), Some("first".to_string()))
    );
    assert_eq!(
        request(&seen, 1),
        (
            "POST /v1/sessions/s1/fork".into(),
            json!({ "before": "00000001" })
        )
    );
}
