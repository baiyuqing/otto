//! Turn start, event stream, image turn and cancel tests.

use super::*;

// ---- turns ----

#[tokio::test]
async fn posting_a_turn_streams_server_sent_events() {
    let harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec!["hel".to_string(), "lo".to_string()],
            usage: Some(Usage {
                input_tokens: 5,
                output_tokens: 7,
                cached_input_tokens: 0,
            }),
            ..Script::default()
        },
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let (reader, turn_id) = start_stream(&harness, &id).await;

    let frames = reader.all().await;
    let events: Vec<&str> = frames.iter().map(|frame| frame.event.as_str()).collect();
    assert!(events.contains(&"text_delta"), "frames = {events:?}");
    assert_eq!(events.last(), Some(&"turn_end"), "frames = {events:?}");
    assert_eq!(events[events.len() - 2], "agent_finished");
    let last: Value = serde_json::from_str(&frames.last().expect("frame").data).expect("json");
    assert_eq!(last["status"], turn::TURN_OK);

    let summary = harness.wait_turn_done(&id, &turn_id).await;
    assert_eq!(summary["text"], "hello");
    assert_eq!(summary["status"], turn::TURN_OK);
    assert_eq!(summary["usage"]["input_tokens"], 5);
    assert_eq!(summary["usage"]["output_tokens"], 7);
    assert_eq!(summary["usage_present"], true);

    let body = harness.metrics_body().await;
    for want in [
        r#"otto_turns_total{status="ok"} 1"#,
        r#"otto_provider_tokens_total{kind="input"} 5"#,
        r#"otto_provider_tokens_total{kind="output"} 7"#,
    ] {
        assert!(body.contains(want), "missing {want}:\n{body}");
    }
}

#[tokio::test]
async fn posting_a_turn_with_stream_false_answers_the_summary() {
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
    assert_eq!(reply.header("content-type"), "application/json");
    let summary = reply.json();
    assert_eq!(summary["status"], turn::TURN_OK);
    assert_eq!(summary["text"], "ok");
    assert!(summary["finished_at"].is_string());
}

#[tokio::test]
async fn posting_an_image_persists_it_with_the_user_prompt() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(
                    r#"{"text":"read it","image":{"data":"iVBORw0KGgo=","mime_type":"image/png"},"stream":false}"#,
                ),
            )
            .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

    let history = harness
        .send("GET", &format!("/v1/sessions/{id}/history"), None)
        .await
        .json();
    assert_eq!(history[0]["blocks"][1]["type"], "image");
    assert_eq!(history[0]["blocks"][1]["mime_type"], "image/png");
    assert_eq!(history[0]["blocks"][1]["data"], "iVBORw0KGgo=");
}

#[tokio::test]
async fn posting_an_image_without_text_starts_a_turn() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(
                    r#"{"text":"","image":{"data":"iVBORw0KGgo=","mime_type":"image/png"},"stream":false}"#,
                ),
            )
            .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

    let history = harness
        .send("GET", &format!("/v1/sessions/{id}/history"), None)
        .await
        .json();
    assert_eq!(history[0]["blocks"].as_array().unwrap().len(), 1);
    assert_eq!(history[0]["blocks"][0]["type"], "image");
}

#[tokio::test]
async fn image_turns_override_the_small_default_body_limit() {
    let harness = Harness::new();
    let id = harness.create().await;
    let mut bytes = vec![0; 1 << 20];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let body = serde_json::json!({
        "text": "read it",
        "image": { "data": BASE64.encode(bytes), "mime_type": "image/png" },
        "stream": false
    })
    .to_string();

    let reply = harness
        .send("POST", &format!("/v1/sessions/{id}/turns"), Some(&body))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

#[tokio::test]
async fn posting_a_malformed_image_is_a_bad_request() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(
                    r#"{"text":"read it","image":{"data":"/9j/","mime_type":"image/png"},"stream":false}"#,
                ),
            )
            .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["error"]["message"], "image is invalid");
}

#[tokio::test]
async fn a_started_turn_carries_the_user_trigger_everywhere() {
    let harness = Harness::new();
    let id = harness.create().await;
    let summary = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(r#"{"text":"hi","stream":false}"#),
        )
        .await
        .json();
    assert_eq!(summary["trigger"], turn::TRIGGER_USER);
    let turn_id = summary["id"].as_str().expect("turn id");

    let got = harness
        .send("GET", &format!("/v1/sessions/{id}/turns/{turn_id}"), None)
        .await
        .json();
    assert_eq!(got["trigger"], turn::TRIGGER_USER);

    let session = harness
        .send("GET", &format!("/v1/sessions/{id}"), None)
        .await
        .json();
    assert_eq!(session["turn"]["trigger"], turn::TRIGGER_USER);
    assert_eq!(session["turn"]["id"], turn_id);
}

#[tokio::test]
async fn an_unknown_turn_is_404_on_every_turn_route() {
    let harness = Harness::new();
    let id = harness.create().await;
    for (method, path) in [
        ("GET", format!("/v1/sessions/{id}/turns/nope")),
        ("GET", format!("/v1/sessions/{id}/turns/nope/events")),
        ("POST", format!("/v1/sessions/{id}/turns/nope/cancel")),
        ("GET", "/v1/sessions/missing/turns/nope".to_string()),
    ] {
        let reply = harness.send(method, &path, None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {path}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_events_resume_from_after_and_from_last_event_id() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (_primary, turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    // after=0 replays from sequence 1, skipping the user_message frame.
    let response = harness
        .raw(
            "GET",
            &format!("/v1/sessions/{id}/turns/{turn_id}/events?after=0"),
            None,
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut resumed = SseReader::new(response);
    assert_eq!(resumed.next().await.expect("a frame").id, Some(1));

    // Last-Event-ID: 0 means the same thing.
    let response = harness
        .raw(
            "GET",
            &format!("/v1/sessions/{id}/turns/{turn_id}/events"),
            None,
            &[("last-event-id", b"0")],
        )
        .await;
    let mut replayed = SseReader::new(response);
    assert_eq!(replayed.next().await.expect("a frame").id, Some(1));

    gate.cancel();
    let tail = resumed.all().await;
    assert_eq!(
        tail.last().map(|frame| frame.event.as_str()),
        Some("turn_end")
    );

    let reply = harness
        .send(
            "GET",
            &format!("/v1/sessions/{id}/turns/{turn_id}/events?after=x"),
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["error"]["code"], "bad_request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starting_a_turn_validates_the_body_and_refuses_a_second_turn() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;

    for body in [
        None,
        Some("{}"),
        Some(r#"{"text":""}"#),
        Some(r#"{"text":"   "}"#),
        Some(r#"{"text":"hi","channel":"slack"}"#),
    ] {
        let reply = harness
            .send("POST", &format!("/v1/sessions/{id}/turns"), body)
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "body {body:?}");
        assert_eq!(reply.json()["error"]["code"], "bad_request");
    }
    assert_eq!(
        harness
            .send(
                "POST",
                "/v1/sessions/missing/turns",
                Some(r#"{"text":"hi"}"#)
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let (_primary, _turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(r#"{"text":"again"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.json()["error"]["code"], "turn_active");
    gate.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_context_is_transient_and_queued_turns_keep_their_own_channel() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let first = post_turn_with_channel(&harness, &id, "first", Some("telegram"), false).await;
    assert_eq!(first.status(), StatusCode::OK);
    harness.provider.wait_started(1).await;
    let second = post_turn_with_channel(&harness, &id, "second", Some("feishu"), true).await;
    let third = post_turn_with_channel(&harness, &id, "third", None, true).await;
    gate.cancel();
    for response in [first, second, third] {
        assert_eq!(ends_of(SseReader::new(response)).await.2, "ok");
    }

    assert_eq!(harness.provider.texts(), ["first", "second", "third"]);
    let prompts = harness.provider.system_prompts();
    assert_eq!(prompts.len(), 3);
    assert!(prompts[0].contains("Telegram"));
    assert!(prompts[1].contains("Feishu (Lark)"));
    assert!(!prompts[2].contains("Telegram"));
    assert!(!prompts[2].contains("Feishu (Lark)"));
    assert!(prompts[0].contains("does not grant additional tools"));
    assert!(prompts[1].contains("does not grant additional tools"));
}

pub(super) async fn post_turn_with_origin(harness: &Harness, session: &str, extra: Value) -> Reply {
    let mut body = serde_json::json!({"text": "hi", "stream": false});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    harness
        .send(
            "POST",
            &format!("/v1/sessions/{session}/turns"),
            Some(&body.to_string()),
        )
        .await
}

/// Records a confirmed web or terminal delivery for `session`, which the
/// next user turn of that session appends when its origin matches.
pub(super) fn confirmed_delivery(
    store: &crate::schedule::Store,
    origin: crate::schedule::Origin,
    session: &str,
) {
    let now = chrono::Utc::now();
    let item = store
        .create(
            crate::schedule::NewItem {
                origin,
                session_id: session.into(),
                workspace: "/w".into(),
                kind: crate::schedule::Kind::Text,
                body: "stretch".into(),
                repeat: None,
                local_time: None,
                due_at: now - chrono::TimeDelta::seconds(1),
            },
            now,
        )
        .expect("create");
    store
        .claim_deliveries(
            &crate::schedule::Scope::Any,
            Duration::from_secs(60),
            10,
            now,
        )
        .expect("claim");
    assert!(
        store
            .delivered(item.id, item.due_at, now)
            .expect("delivered")
    );
}

pub(super) fn pending(
    store: &crate::schedule::Store,
    origin: &crate::schedule::Origin,
    session: &str,
) -> usize {
    store
        .pending_writebacks(origin, session, 10)
        .expect("pending")
        .len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_origin_defaults_to_web_and_is_validated() {
    let store = Arc::new(crate::schedule::Store::open_in_memory().expect("store"));
    let harness = Harness::with(HarnessOptions {
        schedule: Some(store.clone()),
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let web = crate::schedule::Origin::web();
    confirmed_delivery(&store, web.clone(), &id);

    let bad = post_turn_with_origin(&harness, &id, serde_json::json!({"origin": "mars"})).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(
        bad.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("origin")
    );
    let mismatch = post_turn_with_origin(
        &harness,
        &id,
        serde_json::json!({"origin": "chat:feishu:c1", "channel": "telegram"}),
    )
    .await;
    assert_eq!(mismatch.status, StatusCode::BAD_REQUEST);
    assert!(harness.provider.texts().is_empty());
    assert_eq!(pending(&store, &web, &id), 1);

    // A chat turn does not take a web delivery; a turn without an origin
    // is a web turn and does. A turn with no origin at all takes nothing.
    let chat = post_turn_with_origin(
        &harness,
        &id,
        serde_json::json!({"origin": "chat:telegram:42", "channel": "telegram"}),
    )
    .await;
    assert_eq!(chat.status, StatusCode::OK, "{}", chat.body);
    assert_eq!(pending(&store, &web, &id), 1);
    let absent = post_turn_with_origin(&harness, &id, serde_json::json!({})).await;
    assert_eq!(absent.status, StatusCode::OK, "{}", absent.body);
    assert_eq!(pending(&store, &web, &id), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_turn_keeps_its_own_origin() {
    let store = Arc::new(crate::schedule::Store::open_in_memory().expect("store"));
    let (gate, mut options) = gated();
    options.schedule = Some(store.clone());
    let harness = Harness::with(options);
    let id = harness.create().await;
    let chat = crate::schedule::Origin::parse("chat:feishu:c1").expect("origin");
    confirmed_delivery(&store, chat.clone(), &id);

    let first = post_turn_with_channel(&harness, &id, "first", None, false).await;
    harness.provider.wait_started(1).await;
    let body = r#"{"text":"second","queue":true,"origin":"chat:feishu:c1","channel":"feishu"}"#;
    let second = harness
        .raw("POST", &format!("/v1/sessions/{id}/turns"), Some(body), &[])
        .await;
    gate.cancel();
    for response in [first, second] {
        assert_eq!(ends_of(SseReader::new(response)).await.2, "ok");
    }
    assert_eq!(pending(&store, &chat, &id), 0);
}

pub(super) fn schedule_harness() -> (Harness, Arc<crate::schedule::Store>) {
    let store = Arc::new(crate::schedule::Store::open_in_memory().expect("store"));
    let harness = Harness::with(HarnessOptions {
        schedule: Some(store.clone()),
        ..HarnessOptions::default()
    });
    (harness, store)
}

pub(super) fn due_text_item(
    store: &crate::schedule::Store,
    origin: crate::schedule::Origin,
    body: &str,
) -> crate::schedule::Item {
    let now = chrono::Utc::now();
    store
        .create(
            crate::schedule::NewItem {
                origin,
                session_id: "sess".into(),
                workspace: "/w".into(),
                kind: crate::schedule::Kind::Text,
                body: body.into(),
                repeat: None,
                local_time: None,
                due_at: now - chrono::TimeDelta::seconds(1),
            },
            now,
        )
        .expect("create")
}

#[tokio::test]
async fn claim_returns_due_web_items_once_and_delivered_confirms_them() {
    let (harness, store) = schedule_harness();
    let web = due_text_item(&store, crate::schedule::Origin::web(), "stretch");
    due_text_item(&store, crate::schedule::Origin::terminal(), "terminal only");

    let claim = harness.send("POST", "/v1/schedule/claim", Some("{}")).await;
    assert_eq!(claim.status, StatusCode::OK, "{}", claim.body);
    let items = claim.json()["items"].as_array().expect("items").clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], format!("s{}", web.id));
    assert_eq!(items[0]["text"], "⏰ stretch");
    let due_at = items[0]["due_at"].as_i64().expect("due_at");
    assert_eq!(due_at, web.due_at.timestamp_millis());

    let again = harness.send("POST", "/v1/schedule/claim", Some("{}")).await;
    assert_eq!(again.json()["items"].as_array().unwrap().len(), 0);

    let path = format!("/v1/schedule/s{}/delivered", web.id);
    let body = serde_json::json!({"due_at": due_at}).to_string();
    let done = harness.send("POST", &path, Some(&body)).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(done.json(), serde_json::json!({}));
    let writebacks = store
        .pending_writebacks(&crate::schedule::Origin::web(), "sess", 10)
        .expect("writebacks");
    assert_eq!(writebacks.len(), 1);
    assert_eq!(writebacks[0].item_id, web.id);
    // A repeated confirmation is still 200.
    let repeat = harness.send("POST", &path, Some(&body)).await;
    assert_eq!(repeat.status, StatusCode::OK);
}

#[tokio::test]
async fn delivered_rejects_a_bad_id_or_body() {
    let (harness, _store) = schedule_harness();
    let bad_id = harness
        .send("POST", "/v1/schedule/x7/delivered", Some(r#"{"due_at":1}"#))
        .await;
    assert_eq!(bad_id.status, StatusCode::NOT_FOUND);
    let no_due = harness
        .send("POST", "/v1/schedule/s1/delivered", Some("{}"))
        .await;
    assert_eq!(no_due.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn schedule_routes_answer_503_without_a_store() {
    let harness = Harness::new();
    let claim = harness.send("POST", "/v1/schedule/claim", Some("{}")).await;
    assert_eq!(claim.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(claim.json()["error"]["code"], "schedule_unavailable");
    let delivered = harness
        .send("POST", "/v1/schedule/s1/delivered", Some(r#"{"due_at":1}"#))
        .await;
    assert_eq!(delivered.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn the_factory_runner_maps_load_and_create_errors_to_outcomes() {
    use crate::schedule::RunOutcome;
    use crate::schedule::worker::PromptRunner;
    let cancel = CancellationToken::new();

    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            error: Some(("/other".to_string(), "mcp config broken".to_string())),
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });
    let runner = schedule::FactoryRunner(Arc::clone(&harness.factory) as Arc<dyn Factory>);
    assert_eq!(
        runner.run("/other", "go", &cancel).await,
        RunOutcome::Failed("mcp config broken".into())
    );

    let harness = Harness::with(HarnessOptions {
        create_error: Some("no session".to_string()),
        ..HarnessOptions::default()
    });
    let startup = harness.factory.builder.workspace_path.clone();
    let runner = schedule::FactoryRunner(Arc::clone(&harness.factory) as Arc<dyn Factory>);
    assert_eq!(
        runner.run(&startup, "go", &cancel).await,
        RunOutcome::FailedBeforeTools("no session".into())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_factory_runner_runs_the_prompt_and_returns_the_reply() {
    use crate::schedule::RunOutcome;
    use crate::schedule::worker::PromptRunner;
    let harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec!["it is sunny".to_string()],
            ..Script::default()
        },
        ..HarnessOptions::default()
    });
    let startup = harness.factory.builder.workspace_path.clone();
    let runner = schedule::FactoryRunner(Arc::clone(&harness.factory) as Arc<dyn Factory>);
    let outcome = runner
        .run(&startup, "weather?", &CancellationToken::new())
        .await;
    assert_eq!(outcome, RunOutcome::Reply("it is sunny".into()));
    assert_eq!(harness.provider.texts(), ["weather?"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_turn_finishes_it_as_canceled() {
    let (_gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (_stream, turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns/{turn_id}/cancel"),
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.body, "");

    let summary = harness.wait_turn_done(&id, &turn_id).await;
    assert_eq!(summary["status"], turn::TURN_CANCELED);
    assert!(
        harness
            .metrics_body()
            .await
            .contains(r#"otto_turns_total{status="canceled"} 1"#)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_session_cancels_its_active_turn() {
    let (_gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (_stream, _turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;
    let active = harness.active_turn(&id);

    let started = Instant::now();
    let reply = harness
        .send("DELETE", &format!("/v1/sessions/{id}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert!(started.elapsed() < Duration::from_secs(5), "delete blocked");

    Harness::wait_done(&active).await;
    assert_eq!(active.summary().status, turn::TURN_CANCELED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_run_their_turns_at_the_same_time() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let first = harness.create().await;
    let second = harness.create().await;

    let (_a, _) = start_stream(&harness, &first).await;
    harness.provider.wait_started(1).await;
    let (_b, _) = start_stream(&harness, &second).await;
    // The second turn reaching the provider while the first still holds
    // the gate is the assertion; a per-process lock would hang here.
    harness.provider.wait_started(2).await;
    gate.cancel();
}

#[tokio::test]
async fn a_failing_turn_finishes_with_the_error_status() {
    let harness = Harness::with(HarnessOptions {
        script: Script {
            error: Some("provider exploded".to_string()),
            ..Script::default()
        },
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let summary = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(r#"{"text":"hi","stream":false}"#),
        )
        .await
        .json();
    assert_eq!(summary["status"], turn::TURN_ERROR);
    assert!(!summary["error"].as_str().expect("error").is_empty());
    assert!(
        harness
            .metrics_body()
            .await
            .contains(r#"otto_turns_total{status="error"} 1"#)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_event_stream_does_not_cancel_the_turn() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (stream, turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    drop(stream); // the browser disconnects mid-turn
    gate.cancel();

    let summary = harness.wait_turn_done(&id, &turn_id).await;
    assert_eq!(summary["status"], turn::TURN_OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_the_server_cancels_the_active_turn() {
    let (_gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (_stream, _turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;
    let active = harness.active_turn(&id);

    let started = Instant::now();
    harness.server.close().await.expect("close");
    assert!(started.elapsed() < Duration::from_secs(5), "close blocked");

    Harness::wait_done(&active).await;
    assert_eq!(active.summary().status, turn::TURN_CANCELED);
    assert!(
        harness
            .metrics_body()
            .await
            .contains("otto_sessions_open 0")
    );
}
