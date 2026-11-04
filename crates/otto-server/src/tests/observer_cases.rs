use super::*;

#[tokio::test]
async fn observer_task_list_transport_preserves_optional_parent_filter() {
    use otto::subagent::record::{Recorder, Store, TaskContext};
    use otto::subagent::tasks::{Task, TaskStatus};
    let store = Arc::new(Store::open_in_memory().expect("store"));
    for parent in ["first", "second"] {
        store.upsert(
            &TaskContext {
                parent_session: parent.into(),
                ..Default::default()
            },
            &Task {
                id: format!("{parent}-task"),
                status: TaskStatus::Succeeded,
                ..Default::default()
            },
        );
    }
    let harness = Harness::with(HarnessOptions {
        task_recorder: Some(store),
        ..Default::default()
    });
    let unfiltered = harness.send("GET", "/v1/tasks", None).await;
    assert_eq!(unfiltered.status, StatusCode::OK);
    assert_eq!(unfiltered.json()["tasks"].as_array().unwrap().len(), 2);
    let filtered = harness
        .send("GET", "/v1/tasks?parent_session=first", None)
        .await;
    assert_eq!(filtered.status, StatusCode::OK);
    let body = filtered.json();
    let rows = body["tasks"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["parent_session"], "first");
}

#[tokio::test]
async fn observer_route_is_static_session_scoped_and_tracks_terminal_status() {
    let harness = Harness::new();
    let first = harness.create().await;
    let second = harness.create().await;
    let session = harness.server.lookup(&first).expect("first session");
    let tasks = session.ctrl.subagent_tasks().expect("registry");
    let task = tasks
        .add(
            otto::subagent::tasks::Task {
                description: "first session only".into(),
                prompt: "private child prompt".into(),
                ..Default::default()
            },
            None,
            None,
        )
        .expect("task");
    tasks.mark_running(&task.id, chrono::Utc::now());
    let path = format!("/v1/sessions/{first}/tasks/observer");
    let reply = harness.send("GET", &path, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["tasks"][0]["row"]["status"], "running");
    assert_eq!(reply.json()["tasks"][0]["row"]["prompt"], "");
    assert!(
        !reply.json()["tasks"][0]["missing"]
            .as_bool()
            .expect("missing")
    );
    let other = harness
        .send(
            "GET",
            &format!("/v1/sessions/{second}/tasks/observer"),
            None,
        )
        .await;
    assert_eq!(other.status, StatusCode::OK);
    assert_eq!(other.json()["tasks"], serde_json::json!([]));
    tasks.finish(
        &task.id,
        otto::subagent::tasks::TaskStatus::Succeeded,
        chrono::Utc::now(),
        "done",
        "",
    );
    let reply = harness.send("GET", &path, None).await;
    assert_eq!(reply.json()["tasks"][0]["row"]["status"], "succeeded");
    assert_eq!(reply.json()["tasks"][0]["row"]["task_id"], task.id);
    assert_eq!(
        harness
            .send("GET", "/v1/sessions/missing/tasks/observer", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn observer_route_uses_existing_bearer_authentication() {
    let harness = Harness::with(HarnessOptions {
        token: [
            102u8, 105, 120, 116, 117, 114, 101, 45, 116, 111, 107, 101, 110,
        ]
        .into_iter()
        .map(char::from)
        .collect(),
        ..Default::default()
    });
    let path = "/v1/sessions/missing/tasks/observer";
    assert_eq!(
        harness.send("GET", path, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let authorized = harness
        .send_with(
            "GET",
            path,
            None,
            &[("authorization", b"Bearer fixture-token")],
        )
        .await;
    assert_eq!(authorized.status, StatusCode::NOT_FOUND);
}
