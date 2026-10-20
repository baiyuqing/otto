//! Turn queue, stream frame, approval wait and request id tests.

use super::*;

// ---- turn queue, stream frames, approvals ----

pub(super) async fn post_turn(
    harness: &Harness,
    session: &str,
    text: &str,
    queue: bool,
) -> Response {
    let body = serde_json::json!({"text": text, "queue": queue}).to_string();
    harness
        .raw(
            "POST",
            &format!("/v1/sessions/{session}/turns"),
            Some(&body),
            &[],
        )
        .await
}

pub(super) async fn post_turn_with_channel(
    harness: &Harness,
    session: &str,
    text: &str,
    channel: Option<&str>,
    queue: bool,
) -> Response {
    let mut body = serde_json::json!({"text": text, "queue": queue});
    if let Some(channel) = channel {
        body["channel"] = serde_json::Value::String(channel.to_string());
    }
    harness
        .raw(
            "POST",
            &format!("/v1/sessions/{session}/turns"),
            Some(&body.to_string()),
            &[],
        )
        .await
}

pub(super) fn turn_header(response: &Response) -> String {
    response
        .headers()
        .get("otto-turn-id")
        .and_then(|value| value.to_str().ok())
        .expect("Otto-Turn-Id header")
        .to_string()
}

/// Reads frames until one named `name`, and returns its data.
pub(super) async fn frame_named_untimed(reader: &mut SseReader, name: &str) -> Value {
    loop {
        let frame = reader
            .next()
            .await
            .unwrap_or_else(|| panic!("stream ended before {name}"));
        if frame.event == name {
            return serde_json::from_str(&frame.data).expect("frame data");
        }
    }
}

pub(super) async fn frame_named(reader: &mut SseReader, name: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), frame_named_untimed(reader, name))
        .await
        .unwrap_or_else(|_| panic!("no {name} frame in time"))
}

/// The events of a finished stream: first and last frame names, and the
/// `turn_end` status.
pub(super) async fn ends_of(reader: SseReader) -> (String, String, String) {
    let frames = tokio::time::timeout(Duration::from_secs(10), reader.all())
        .await
        .expect("stream did not end");
    let last = frames.last().expect("frames");
    let status = serde_json::from_str::<Value>(&last.data).expect("data")["status"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    (frames[0].event.clone(), last.event.clone(), status)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_turns_run_in_arrival_order() {
    let (gate, mut options) = gated();
    options.script.echo = true;
    let harness = Harness::with(options);
    let id = harness.create().await;

    let first = post_turn(&harness, &id, "one", false).await;
    assert_eq!(first.status(), StatusCode::OK);
    harness.provider.wait_started(1).await;
    let mut readers = vec![SseReader::new(first)];
    let mut ids = Vec::new();
    for text in ["two", "three"] {
        let queued = post_turn(&harness, &id, text, true).await;
        assert_eq!(queued.status(), StatusCode::OK);
        ids.push(turn_header(&queued));
        readers.push(SseReader::new(queued));
    }
    let waiting = harness
        .send("GET", &format!("/v1/sessions/{id}/turns/{}", ids[0]), None)
        .await
        .json();
    assert_eq!(waiting["status"], turn::TURN_QUEUED);

    gate.cancel();
    for reader in readers {
        let (first, last, status) = ends_of(reader).await;
        assert_eq!(first, "user_message");
        assert_eq!(last, "turn_end");
        assert_eq!(status, "ok");
    }
    assert_eq!(harness.provider.texts(), ["one", "two", "three"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_session_refuses_a_turn_without_queue_and_the_seventeenth_with_it() {
    let (_gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let first = post_turn(&harness, &id, "one", false).await;
    harness.provider.wait_started(1).await;

    let refused = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(r#"{"text":"x"}"#),
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.json()["error"]["code"], "turn_active");

    let mut held = vec![first];
    for index in 0..MAX_QUEUED_TURNS {
        let queued = post_turn(&harness, &id, &format!("q{index}"), true).await;
        assert_eq!(queued.status(), StatusCode::OK, "queued turn {index}");
        held.push(queued);
    }
    let full = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(r#"{"text":"late","queue":true}"#),
        )
        .await;
    assert_eq!(full.status, StatusCode::CONFLICT, "{}", full.body);
    assert_eq!(full.json()["error"]["code"], "queue_full");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_queued_turn_removes_it_and_ends_its_events() {
    let (gate, mut options) = gated();
    options.script.echo = true;
    let harness = Harness::with(options);
    let id = harness.create().await;
    let first = post_turn(&harness, &id, "one", false).await;
    harness.provider.wait_started(1).await;
    let second = post_turn(&harness, &id, "two", true).await;
    let second_id = turn_header(&second);
    let third = post_turn(&harness, &id, "three", true).await;

    let cancel = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns/{second_id}/cancel"),
            None,
        )
        .await;
    assert_eq!(cancel.status, StatusCode::ACCEPTED);
    let (_, last, status) = ends_of(SseReader::new(second)).await;
    assert_eq!((last.as_str(), status.as_str()), ("turn_end", "canceled"));

    gate.cancel();
    for reader in [SseReader::new(first), SseReader::new(third)] {
        assert_eq!(ends_of(reader).await.2, "ok");
    }
    assert_eq!(harness.provider.texts(), ["one", "three"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_turn_does_not_start_while_a_turn_is_queued() {
    let (harness, tasks) = wake_harness(Script {
        deltas: vec!["ok".to_string()],
        ..Script::default()
    });
    let id = harness.create().await;
    let session = harness.server.lookup(&id).expect("session");
    session.lock().queue.push_back(QueuedTurn {
        turn: Arc::new(Turn::queued(
            "q1".to_string(),
            turn::TRIGGER_USER,
            CancellationToken::new(),
        )),
        text: "x".to_string(),
        image: None,
        channel: None,
        origin: None,
        input_id: None,
    });

    tasks.notifications().push(notification());
    harness.server.wake_turn(&session).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(harness.provider.roles().is_empty());
    assert!(session.current_turn().is_none());

    // With the queue empty the same notification starts a wake turn.
    session.lock().queue.clear();
    harness.server.wake_turn(&session).await;
    assert_eq!(harness.provider.roles(), vec![Role::Context]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_the_server_cancels_queued_turns() {
    let (_gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let first = post_turn(&harness, &id, "one", false).await;
    harness.provider.wait_started(1).await;
    let second = post_turn(&harness, &id, "two", true).await;

    harness.server.close().await.expect("close");

    assert_eq!(ends_of(SseReader::new(first)).await.2, "canceled");
    let (_, last, status) = ends_of(SseReader::new(second)).await;
    assert_eq!((last.as_str(), status.as_str()), ("turn_end", "canceled"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compact_answers_409_while_a_turn_is_running_with_a_queue() {
    let (_gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let _first = post_turn(&harness, &id, "one", false).await;
    harness.provider.wait_started(1).await;
    let _second = post_turn(&harness, &id, "two", true).await;

    let reply = harness
        .send("POST", &format!("/v1/sessions/{id}/compact"), None)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "turn_active");
}

#[test]
fn a_session_with_only_a_queued_turn_is_busy() {
    let mut state = SessionState::default();
    assert!(!state.busy());
    let turn = Arc::new(Turn::queued(
        "t1".to_string(),
        turn::TRIGGER_USER,
        CancellationToken::new(),
    ));
    state.queue.push_back(QueuedTurn {
        turn,
        text: "x".to_string(),
        image: None,
        channel: None,
        origin: None,
        input_id: None,
    });
    assert!(state.busy());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_stream_starts_with_user_message_and_ends_with_turn_end() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (mut reader, turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;
    gate.cancel();
    let user = harness
        .send("GET", &format!("/v1/sessions/{id}/turns/{turn_id}"), None)
        .await;
    assert_eq!(user.status, StatusCode::OK);

    let mut last = None;
    while let Some(frame) = reader.next().await {
        last = Some(frame);
    }
    let last = last.expect("frames");
    assert_eq!(last.event, "turn_end");
    let last_id = last.id.expect("id");

    // A reconnect after the last delta frame still receives turn_end.
    let response = harness
        .raw(
            "GET",
            &format!(
                "/v1/sessions/{id}/turns/{turn_id}/events?after={}",
                last_id - 1
            ),
            None,
            &[],
        )
        .await;
    let frames = SseReader::new(response).all().await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event, "turn_end");
    assert_eq!(
        serde_json::from_str::<Value>(&frames[0].data).expect("data")["status"],
        "ok"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_before_turn_omits_the_running_turns_messages() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (_reader, turn_id) = start_stream(&harness, &id).await;
    // The provider is called after the prompt is appended.
    harness.provider.wait_started(1).await;
    let count = |path: String| {
        let harness = &harness;
        async move {
            let response = harness.send("GET", &path, None).await;
            assert_eq!(response.status, StatusCode::OK, "{path}");
            response.json().as_array().expect("array").len()
        }
    };
    assert_eq!(count(format!("/v1/sessions/{id}/history")).await, 1);
    assert_eq!(
        count(format!("/v1/sessions/{id}/history?before_turn={turn_id}")).await,
        0
    );
    let unknown = harness
        .send(
            "GET",
            &format!("/v1/sessions/{id}/history?before_turn=missing"),
            None,
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    gate.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_user_message_frame_carries_text_and_image_flag() {
    let harness = Harness::new();
    let id = harness.create().await;
    let response = harness
        .raw(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(r#"{"text":"look","image":{"data":"iVBORw0KGgo=","mime_type":"image/png"}}"#),
            &[],
        )
        .await;
    let mut reader = SseReader::new(response);
    let user = frame_named(&mut reader, "user_message").await;
    assert_eq!(user["text"], "look");
    assert_eq!(user["image"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_provider_error_is_redacted_in_the_frame_the_summary_and_the_log() {
    let harness = Harness::with(HarnessOptions {
        script: Script {
            error: Some("upstream rejected key sk-alpha-secret".to_string()),
            ..Script::default()
        },
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let response = post_turn(&harness, &id, "hi", false).await;
    let turn_id = turn_header(&response);
    let frames = SseReader::new(response).all().await;
    let end = frames.last().expect("frames");
    assert_eq!(end.event, "turn_end");
    assert!(frames.iter().any(|frame| frame.event == "agent_error"));
    for frame in &frames {
        assert!(!frame.data.contains("sk-alpha-secret"), "{}", frame.data);
    }
    assert!(end.data.contains("upstream rejected key"), "{}", end.data);

    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}/turns/{turn_id}"), None)
        .await;
    assert!(!reply.body.contains("sk-alpha-secret"), "{}", reply.body);
    assert_eq!(reply.json()["status"], "error");
    assert!(!harness.logged().contains("sk-alpha-secret"));
}

#[tokio::test]
async fn list_rows_carry_the_last_user_text_and_modified_time() {
    let long = "x".repeat(200);
    let harness = Harness::with(HarnessOptions {
        list: Some(ListResult {
            sessions: vec![SessionInfo {
                id: "a1".to_string(),
                last_user_text: long,
                ..SessionInfo::default()
            }],
            skipped: 0,
        }),
        ..HarnessOptions::default()
    });
    let body = harness.send("GET", "/v1/sessions", None).await.json();
    let row = &body["sessions"][0];
    assert_eq!(row["last_user_text"].as_str().expect("text").len(), 80);
    let modified = row["modified"].as_str().expect("modified");
    assert!(
        chrono::DateTime::parse_from_rfc3339(modified).is_ok(),
        "{modified}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_rows_carry_the_turn_id_and_the_queued_count() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let response = harness.raw("GET", "/v1/status", None, &[]).await;
    let mut reader = SseReader::new(response);
    let idle = status_snapshot(&mut reader).await;
    assert_eq!(idle["sessions"][0]["queued"], 0);

    let first = post_turn(&harness, &id, "one", false).await;
    let turn_id = turn_header(&first);
    harness.provider.wait_started(1).await;
    let _second = post_turn(&harness, &id, "two", true).await;
    let _third = post_turn(&harness, &id, "three", true).await;

    let row = loop {
        let snapshot = status_snapshot(&mut reader).await;
        let row = snapshot["sessions"][0].clone();
        if row["queued"] == 2 {
            break row;
        }
    };
    assert_eq!(row["turn_id"], turn_id.as_str());
    gate.cancel();
}

// ---- observability: request ID and step events ----

pub(super) fn log_lines(harness: &Harness, event: &str) -> Vec<String> {
    let needle = format!("msg={event} ");
    harness
        .logged()
        .lines()
        .filter(|line| line.contains(&needle))
        .map(str::to_string)
        .collect()
}

/// [`log_lines`] once at least one line is there. A turn is marked done
/// before `turn_finished` is written, so a test that has seen the turn
/// end can still be ahead of that line.
pub(super) async fn wait_log_lines(harness: &Harness, event: &str) -> Vec<String> {
    for _ in 0..600 {
        let lines = log_lines(harness, event);
        if !lines.is_empty() {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Vec::new()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_logs_its_request_id_on_every_event() {
    let harness = approval_harness();
    let id = harness.create().await;
    let body = serde_json::json!({"text": "run it", "queue": false}).to_string();
    let response = harness
        .raw(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(&body),
            &[("x-request-id", b"req-abc")],
        )
        .await;
    let turn_id = turn_header(&response);
    let mut reader = SseReader::new(response);
    let requested = frame_named(&mut reader, "approval_requested").await;
    let approval = requested["approval_id"].as_str().expect("id").to_string();
    decide(&harness, &id, &approval, "allow").await;
    ends_of(reader).await;
    harness.wait_turn_done(&id, &turn_id).await;

    let ids = format!("session_id={id} turn_id={turn_id} request_id=req-abc");
    for event in [
        "turn_started",
        "provider_request_finished",
        "tool_call_finished",
        "turn_finished",
    ] {
        let lines = wait_log_lines(&harness, event).await;
        assert!(!lines.is_empty(), "no {event}:\n{}", harness.logged());
        assert!(
            lines.iter().all(|line| line.contains(&ids)),
            "{event}: {lines:?}"
        );
    }
    let finished = log_lines(&harness, "turn_finished").remove(0);
    for field in [
        "provider_requests=4 ",
        "provider_ms=",
        "tool_calls=",
        "tool_ms=",
        "input_tokens=",
        "output_tokens=",
    ] {
        assert!(finished.contains(field), "{field}: {finished}");
    }
    let provider = log_lines(&harness, "provider_request_finished").remove(0);
    for field in [
        "provider=",
        "model=",
        "status=",
        "attempts=",
        "duration_ms=",
    ] {
        assert!(provider.contains(field), "{field}: {provider}");
    }
    let tool = log_lines(&harness, "tool_call_finished").remove(0);
    assert!(tool.contains("tool=bash"), "{tool}");
    assert!(tool.contains("status=ok") || tool.contains("status=error"));
    assert!(
        log_lines(&harness, "http_request").iter().any(|line| line
            .contains("route=\"POST /v1/sessions/{id}/turns\"")
            && line.contains("request_id=req-abc")),
        "{}",
        harness.logged()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_without_the_header_logs_no_request_id_field() {
    let harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec!["ok".to_string()],
            ..Script::default()
        },
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(r#"{"text":"hi","stream":false}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    for event in ["turn_started", "provider_request_finished", "turn_finished"] {
        let lines = wait_log_lines(&harness, event).await;
        assert!(!lines.is_empty(), "no {event}");
        for line in lines {
            assert!(line.contains(&format!("session_id={id} ")), "{line}");
            assert!(!line.contains("request_id="), "{line}");
        }
    }
}
