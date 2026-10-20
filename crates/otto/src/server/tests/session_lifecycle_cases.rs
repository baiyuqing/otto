//! Session create, resume, delete, history, trace and cross-workspace tests.

use super::*;

#[tokio::test]
async fn an_update_plan_call_streams_plan_updated_and_sets_the_session_plan() {
    let harness = Harness::with(HarnessOptions {
            script: Script {
                deltas: vec!["done".to_string()],
                plan_call: Some(
                    r#"{"steps":[{"text":"read","status":"completed"},{"text":"write","status":"in_progress"}]}"#
                        .to_string(),
                ),
                ..Script::default()
            },
            ..HarnessOptions::default()
        });
    let id = harness.create().await;
    let created = harness
        .send("GET", &format!("/v1/sessions/{id}"), None)
        .await
        .json();
    assert!(created.get("plan").is_none(), "{created}");

    let response = post_turn(&harness, &id, "plan it", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let frames = SseReader::new(response).all().await;
    let plan = frames
        .iter()
        .find(|frame| frame.event == "plan_updated")
        .expect("plan_updated frame");
    let data: Value = serde_json::from_str(&plan.data).expect("frame data");
    let steps = serde_json::json!([
        {"text": "read", "status": "completed"},
        {"text": "write", "status": "in_progress"}
    ]);
    assert_eq!(data["steps"], steps);

    let session = harness
        .send("GET", &format!("/v1/sessions/{id}"), None)
        .await
        .json();
    assert_eq!(session["plan"], steps, "{session}");
}

#[tokio::test]
async fn creating_a_session_answers_201_and_counts_one_open_session() {
    let harness = Harness::new();
    let reply = harness.send("POST", "/v1/sessions", None).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert_eq!(reply.header("content-type"), "application/json");

    let body = reply.json();
    assert!(!body["id"].as_str().expect("id").is_empty());
    assert_eq!(body["provider"], "openai-compatible");
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["context_window"], 128_000);
    assert!(body["sandbox"]["summary"].as_str().is_some());

    assert!(
        harness
            .metrics_body()
            .await
            .contains("otto_sessions_open 1")
    );
    let health = harness.send("GET", "/healthz", None).await.json();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["sessions_open"], 1);
    assert_eq!(harness.factory.create_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn metrics_expose_each_open_session_context() {
    let harness = Harness::with(HarnessOptions {
        fixed_id: Some("ctx-session".to_string()),
        ..HarnessOptions::default()
    });
    harness.create().await;

    let body = harness.metrics_body().await;
    let labels = r#"{session_id="ctx-session",provider="openai-compatible",model="test-model"}"#;
    for want in [
        format!("otto_session_context_window_tokens{labels} 128000"),
        format!("otto_session_context_input_tokens_pending{labels} 0"),
    ] {
        assert!(body.contains(&want), "missing {want}:\n{body}");
    }
    // A session that has never counted its context reports no input
    // gauge at all; the metric is documented "when available".
    assert!(!body.contains(&format!("otto_session_context_input_tokens{labels}")));
}

#[tokio::test]
async fn resuming_an_unknown_session_is_404_not_found() {
    let harness = Harness::with(HarnessOptions {
        open_error: Some(SESSION_NOT_FOUND.to_string()),
        ..HarnessOptions::default()
    });
    let reply = harness
        .send("POST", "/v1/sessions", Some(r#"{"resume":"nope"}"#))
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.json()["error"]["code"], "not_found");
}

#[tokio::test]
async fn resuming_an_open_session_does_not_call_open_again() {
    let harness = Harness::new();
    let id = harness.create().await;

    let reply = harness
        .send(
            "POST",
            "/v1/sessions",
            Some(&format!(r#"{{"resume":"{id}"}}"#)),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["id"], id.as_str());
    assert_eq!(harness.factory.open_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn resuming_an_open_session_in_a_different_workspace_is_not_found() {
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let reply = harness
        .send(
            "POST",
            "/v1/sessions",
            Some(&format!(r#"{{"resume":"{id}","workspace":"/other"}}"#)),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    assert_eq!(harness.factory.open_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_resumes_of_one_id_call_open_once() {
    let gate = CancellationToken::new();
    let harness = Arc::new(Harness::with(HarnessOptions {
        fixed_id: Some("shared".to_string()),
        open_gate: Some(gate.clone()),
        ..HarnessOptions::default()
    }));

    let resumes: Vec<_> = (0..8)
        .map(|_| {
            let harness = Arc::clone(&harness);
            tokio::spawn(async move {
                harness
                    .send("POST", "/v1/sessions", Some(r#"{"resume":"shared"}"#))
                    .await
                    .status
            })
        })
        .collect();

    // Let the first resume reach `open` before any of them may return.
    while harness.factory.open_calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    gate.cancel();

    for resume in resumes {
        assert_eq!(resume.await.expect("resume"), StatusCode::OK);
    }
    assert_eq!(harness.factory.open_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_session_is_gone_after_delete() {
    let harness = Harness::new();
    assert_eq!(
        harness
            .send("GET", "/v1/sessions/does-not-exist", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let id = harness.create().await;
    assert_eq!(
        harness
            .send("GET", &format!("/v1/sessions/{id}"), None)
            .await
            .status,
        StatusCode::OK
    );
    let deleted = harness
        .send("DELETE", &format!("/v1/sessions/{id}"), None)
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(deleted.body, "");
    assert_eq!(
        harness
            .send("GET", &format!("/v1/sessions/{id}"), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert!(
        harness
            .metrics_body()
            .await
            .contains("otto_sessions_open 0")
    );
}

#[tokio::test]
async fn an_empty_history_is_an_empty_array() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}/history"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, "[]");
    assert_eq!(
        harness
            .send("GET", "/v1/sessions/missing/history", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_context_report_is_served_per_open_session() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}/context"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    // The harness runner has no prompt and no tools; the section contents
    // are covered by the otto-core and runtime-builder tests.
    let report = reply.json();
    assert_eq!(report["model"], "test-model");
    assert!(report["sections"].is_array(), "{}", reply.body);
    assert!(report["reported_input_tokens"].is_null());
    assert_eq!(
        harness
            .send("GET", "/v1/sessions/missing/context", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_trace_is_served_without_message_text_and_spans_carry_the_content() {
    let harness = Harness::with(HarnessOptions {
        script: Script {
            tool_command: Some("echo hi".to_string()),
            deltas: vec!["done".to_string()],
            ..Script::default()
        },
        elevate: true,
        persistent: true,
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let response = post_turn(&harness, &id, "run the secret prompt", false).await;
    let turn_id = turn_header(&response);
    let mut reader = SseReader::new(response);
    let requested = frame_named(&mut reader, "approval_requested").await;
    let approval = requested["approval_id"].as_str().expect("id").to_string();
    decide(&harness, &id, &approval, "allow").await;
    ends_of(reader).await;
    harness.wait_turn_done(&id, &turn_id).await;

    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}/trace"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(!reply.body.contains("elevated ok"), "{}", reply.body);
    assert!(!reply.body.contains("secret prompt"), "{}", reply.body);
    let trace = reply.json();
    let turns = trace["turns"].as_array().expect("turns");
    // The elevated command is approved on a retry turn, so the last
    // turn holds the completed tool call.
    let children = turns.last().expect("a turn")["children"]
        .as_array()
        .expect("children");
    let kinds: Vec<&str> = children
        .iter()
        .map(|span| span["kind"].as_str().expect("kind"))
        .collect();
    assert!(kinds.contains(&"provider"), "{kinds:?}");
    let tool = children
        .iter()
        .find(|span| span["kind"] == "tool" && span["status"] == "ok")
        .unwrap_or_else(|| panic!("no tool span: {}", reply.body));

    let span_id = tool["id"].as_str().expect("span id");
    let content = harness
        .send("GET", &format!("/v1/sessions/{id}/trace/{span_id}"), None)
        .await;
    assert_eq!(content.status, StatusCode::OK, "{}", content.body);
    let titles: Vec<String> = content.json()["sections"]
        .as_array()
        .expect("sections")
        .iter()
        .map(|section| section["title"].as_str().expect("title").to_string())
        .collect();
    assert_eq!(titles, ["arguments", "result"]);
    assert!(content.body.contains("elevated ok"), "{}", content.body);

    assert_eq!(
        harness
            .send(
                "GET",
                &format!("/v1/sessions/{id}/trace/no-such-span"),
                None
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        harness
            .send("GET", "/v1/sessions/missing/trace", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        harness
            .send("GET", "/v1/sessions/missing/trace/x", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn listing_merges_open_sessions_over_the_disk_list() {
    let harness = Harness::with(HarnessOptions {
        list: Some(ListResult::default()),
        ..HarnessOptions::default()
    });
    let empty = harness.send("GET", "/v1/sessions", None).await.json();
    assert_eq!(empty["sessions"].as_array().expect("rows").len(), 0);

    let id = harness.create().await;
    let body = harness.send("GET", "/v1/sessions", None).await.json();
    let rows = body["sessions"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], id.as_str());
    assert_eq!(rows[0]["open"], true);
    assert!(
        rows[0].get("path").is_none(),
        "an open-only row has no path"
    );
}

// ---- sessions across workspaces ----

#[tokio::test]
async fn creating_a_session_in_an_unadmitted_workspace_answers_403() {
    let harness = Harness::new();
    let reply = harness
        .send(
            "POST",
            "/v1/sessions",
            Some(r#"{"workspace":"/elsewhere"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "WORKSPACE_NOT_ADMITTED");
}

#[tokio::test]
async fn creating_a_session_in_an_invalid_workspace_answers_400() {
    let harness = Harness::new();
    let reply = harness
        .send("POST", "/v1/sessions", Some(r#"{"workspace":"relative"}"#))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "INVALID_WORKSPACE");
}

#[tokio::test]
async fn creating_a_session_in_an_admitted_workspace_succeeds() {
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });
    let reply = harness
        .send("POST", "/v1/sessions", Some(r#"{"workspace":"/other"}"#))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
}

#[tokio::test]
async fn resuming_by_id_finds_the_session_with_or_without_a_workspace_field() {
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });
    let created = harness
        .send("POST", "/v1/sessions", Some(r#"{"workspace":"/other"}"#))
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.json()["id"].as_str().unwrap().to_string();

    let without = harness
        .send(
            "POST",
            "/v1/sessions",
            Some(&format!(r#"{{"resume":"{id}"}}"#)),
        )
        .await;
    assert_eq!(without.status, StatusCode::OK, "{}", without.body);
    assert_eq!(without.json()["id"], id.as_str());

    // An already-open session still requires the matching workspace.
    let with = harness
        .send(
            "POST",
            "/v1/sessions",
            Some(&format!(r#"{{"resume":"{id}","workspace":"/other"}}"#)),
        )
        .await;
    assert_eq!(with.status, StatusCode::OK, "{}", with.body);
    assert_eq!(with.json()["id"], id.as_str());
}

#[tokio::test]
async fn listing_sessions_returns_every_workspace_tagged_by_default() {
    let harness = Harness::with(HarnessOptions {
        list: Some(ListResult {
            sessions: vec![
                SessionInfo {
                    id: "a1".to_string(),
                    cwd: "/workspace-a".to_string(),
                    ..SessionInfo::default()
                },
                SessionInfo {
                    id: "b1".to_string(),
                    cwd: "/workspace-b".to_string(),
                    ..SessionInfo::default()
                },
            ],
            skipped: 0,
        }),
        ..HarnessOptions::default()
    });
    let body = harness.send("GET", "/v1/sessions", None).await.json();
    let rows = body["sessions"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
    let row = |id: &str| rows.iter().find(|row| row["id"] == id).expect("row");
    assert_eq!(row("a1")["workspace"], "/workspace-a");
    assert_eq!(row("b1")["workspace"], "/workspace-b");
}

#[tokio::test]
async fn listing_sessions_with_a_workspace_query_filters_to_that_workspace() {
    let mut list_by_workspace = HashMap::new();
    list_by_workspace.insert(
        "/other".to_string(),
        ListResult {
            sessions: vec![SessionInfo {
                id: "b1".to_string(),
                cwd: "/other".to_string(),
                ..SessionInfo::default()
            }],
            skipped: 0,
        },
    );
    let harness = Harness::with(HarnessOptions {
        list: Some(ListResult {
            sessions: vec![SessionInfo {
                id: "a1".to_string(),
                cwd: "/workspace-a".to_string(),
                ..SessionInfo::default()
            }],
            skipped: 0,
        }),
        list_by_workspace,
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });

    let body = harness
        .send("GET", "/v1/sessions?workspace=/other", None)
        .await
        .json();
    let rows = body["sessions"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "b1");
    assert_eq!(rows[0]["workspace"], "/other");
}

#[tokio::test]
async fn listing_sessions_with_an_unadmitted_workspace_answers_403() {
    let harness = Harness::new();
    let reply = harness
        .send("GET", "/v1/sessions?workspace=/elsewhere", None)
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "WORKSPACE_NOT_ADMITTED");
}
