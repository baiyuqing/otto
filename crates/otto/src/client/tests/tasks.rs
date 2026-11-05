//! Tests for sub-agent task calls and sandbox reload.

use super::*;

fn sample_task(id: &str) -> crate::app::tasks::Task {
    crate::app::tasks::Task {
        id: id.into(),
        name: String::new(),
        agent: "explorer".into(),
        description: "look around".into(),
        model: String::new(),
        status: crate::app::tasks::TaskStatus::Canceled,
        created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        started_at: None,
        finished_at: None,
        steps: 3,
        tool_calls: 2,
        last_tool: String::new(),
        last_text: "done".into(),
        usage: Usage::default(),
        usage_present: false,
        result: String::new(),
        error: "boom".into(),
        session_path: String::new(),
    }
}

#[tokio::test]
async fn session_tasks_converts_wire_tasks_to_registry_tasks() {
    let body = serde_json::to_string(&json!({ "tasks": [sample_task("a1")] })).unwrap();
    let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
    let tasks = client.session_tasks("s1").await.unwrap();
    assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1/tasks");
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    assert_eq!(task.id, "a1");
    assert_eq!(task.agent, "explorer");
    assert_eq!(task.status, crate::subagent::tasks::TaskStatus::Canceled);
    assert_eq!(task.created_at.map(|t| t.timestamp()), Some(1_700_000_000));
    assert_eq!((task.steps, task.tool_calls), (3, 2));
    assert_eq!(task.error, "boom");
    assert!(task.name.is_empty() && task.prompt.is_empty() && task.context.is_empty());
}

#[tokio::test]
async fn observer_snapshot_uses_static_session_route_and_decodes_missing_and_truncation() {
    let body = json!({ "tasks": [{ "row": sample_row(), "entries": [{
        "kind": "assistant", "text": "streaming", "tool_name": "", "tool_args": "", "tool_output": "",
        "tool_done": false, "tool_error": false
    }], "missing": false }], "truncated": true }).to_string();
    let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
    let snapshot = client.observer_snapshot("s1").await.expect("observer");
    assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1/tasks/observer");
    assert_eq!(snapshot.tasks[0].entries[0].text, "streaming");
    assert!(!snapshot.tasks[0].missing);
    assert!(snapshot.truncated);
}

fn sample_row() -> TaskRow {
    TaskRow {
        parent_session: "s1".into(),
        task_id: "a1".into(),
        workspace: "/w".into(),
        agent: "explorer".into(),
        status: "succeeded".into(),
        created_at: "2026-10-01T00:00:00Z".into(),
        steps: 4,
        result: "ok".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn tasks_sends_only_the_query_fields_that_are_set() {
    let mut row = serde_json::to_value(sample_row()).unwrap();
    row["cancelable"] = json!(false);
    let body = json!({ "tasks": [row], "next_before": "2026-09-30T00:00:00Z" }).to_string();
    let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
    let query = ListQuery {
        parent_session: Some("parent session".into()),
        status: Some("running".into()),
        workspace: None,
        limit: Some(5),
        before: Some("2026-10-01T00:00:00Z".into()),
    };
    let result = client.tasks(&query).await.unwrap();
    assert_eq!(result.tasks, vec![sample_row()]);
    assert_eq!(result.next_before, "2026-09-30T00:00:00Z");
    client.tasks(&ListQuery::default()).await.unwrap();
    let (line, _) = request(&seen, 0);
    let (path, params) = line.split_once('?').expect("query string");
    assert_eq!(path, "GET /v1/tasks");
    let mut params: Vec<&str> = params.split('&').collect();
    params.sort_unstable();
    assert_eq!(
        params,
        [
            "before=2026-10-01T00%3A00%3A00Z",
            "limit=5",
            "parent_session=parent+session",
            "status=running"
        ]
    );
    assert_eq!(request(&seen, 1).0, "GET /v1/tasks");
}

#[tokio::test]
async fn task_decodes_the_detail_and_maps_404_to_none() {
    let mut row = serde_json::to_value(sample_row()).unwrap();
    row["cancelable"] = json!(false);
    let body = json!({ "task": row, "history": [], "transcript_missing": true }).to_string();
    let (client, seen, _dir) = stub(move |_, path| {
        if path.ends_with("/a1") {
            json_body(StatusCode::OK, &body)
        } else {
            json_body(
                StatusCode::NOT_FOUND,
                r#"{"error":{"code":"not_found","message":"task not found"}}"#,
            )
        }
    });
    assert_eq!(client.task("s1", "a1").await.unwrap(), Some(sample_row()));
    assert_eq!(client.task("s1", "zz").await.unwrap(), None);
    assert_eq!(request(&seen, 0).0, "GET /v1/tasks/s1/a1");
}

#[tokio::test]
async fn task_detail_keeps_server_history_and_missing_transcript_diagnostic() {
    let history = vec![Message {
        role: otto_core::model::Role::User,
        blocks: vec![Block::text("child transcript")],
        ..Message::default()
    }];
    let mut row = serde_json::to_value(sample_row()).unwrap();
    row["cancelable"] = json!(false);
    let body = json!({
        "task": row,
        "history": history,
        "transcript_missing": false
    })
    .to_string();
    let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
    let detail = client
        .task_detail("s1", "a1")
        .await
        .unwrap()
        .expect("task detail");
    assert_eq!(detail.task, sample_row());
    assert_eq!(detail.history[0].text(), "child transcript");
    assert!(!detail.transcript_missing);
    assert_eq!(request(&seen, 0).0, "GET /v1/tasks/s1/a1");

    let body = json!({
        "task": serde_json::to_value(sample_row()).unwrap(),
        "history": [],
        "transcript_missing": true
    })
    .to_string();
    let (client, _, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
    let detail = client
        .task_detail("s1", "a1")
        .await
        .unwrap()
        .expect("task row remains available");
    assert!(detail.history.is_empty());
    assert!(detail.transcript_missing);
    assert!(matches!(
        client.task_history("s1", "a1").await,
        Err(Error::Http { status: 404, code, .. }) if code == "transcript_missing"
    ));
}

#[tokio::test]
async fn task_other_http_errors_are_not_swallowed() {
    let (client, _seen, _dir) = stub(|_, _| {
        json_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":{"code":"internal","message":"x"}}"#,
        )
    });
    let error = client.task("s1", "a1").await.expect_err("500");
    assert!(matches!(error, Error::Http { status: 500, .. }), "{error}");
}

#[tokio::test]
async fn reload_sandbox_returns_the_summary() {
    let (client, seen, _dir) = stub(|_, _| {
        json_body(
            StatusCode::OK,
            r#"{"mode":"seatbelt","network":"allowed","bash_available":true,"summary":"seatbelt line"}"#,
        )
    });
    assert_eq!(client.reload_sandbox().await.unwrap(), "seatbelt line");
    assert_eq!(request(&seen, 0).0, "POST /v1/sandbox/reload");
}
