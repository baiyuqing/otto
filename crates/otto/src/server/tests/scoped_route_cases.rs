//! Session-scoped wake reply, approval, task and MCP route tests.

use super::*;

#[tokio::test]
async fn wake_replies_are_listed_after_the_cursor_and_unknown_sessions_are_404() {
    let harness = Harness::new();
    let id = harness.create().await;
    let path = format!("/v1/sessions/{id}/wake-replies");
    let session = harness.server.lookup(&id).expect("session");
    session.ctrl.push_notice("reflection: not a wake reply");
    session.ctrl.push_wake_reply("first");
    session.ctrl.push_wake_reply("second");

    let reply = harness.send("GET", &path, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        serde_json::json!({
            "notices": [{"id": 1, "text": "first"}, {"id": 2, "text": "second"}],
            "last": 2,
        })
    );
    let reply = harness.send("GET", &format!("{path}?after=1"), None).await;
    assert_eq!(
        reply.json()["notices"],
        serde_json::json!([{"id": 2, "text": "second"}])
    );
    let reply = harness
        .send("GET", "/v1/sessions/missing/wake-replies", None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn compacting_an_empty_session_is_a_noop() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send("POST", &format!("/v1/sessions/{id}/compact"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["noop"], true);
}

#[tokio::test]
async fn sandbox_reload_is_501_when_the_factory_wires_none() {
    let harness = Harness::new();
    let reply = harness.send("POST", "/v1/sandbox/reload", None).await;
    assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(reply.json()["error"]["code"], "not_implemented");
}

#[tokio::test]
async fn sandbox_reload_response_is_unchanged_for_a_single_workspace() {
    let harness = Harness::with(HarnessOptions {
        sandbox_reload: Some(Ok(SandboxInfo {
            mode: SandboxMode::Seatbelt,
            network: SandboxNetwork::Allowed,
            bash_available: true,
            reason: SandboxReason::None,
        })),
        ..Default::default()
    });
    let reply = harness.send("POST", "/v1/sandbox/reload", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["mode"], "seatbelt");
    assert_eq!(body["network"], "allowed");
    assert_eq!(body["bash_available"], true);
    assert!(
        body.get("workspaces").is_none(),
        "single-workspace reload gained a workspaces field: {body}"
    );
}

#[tokio::test]
async fn sandbox_reload_failure_is_unchanged_for_a_single_workspace() {
    let harness = Harness::with(HarnessOptions {
        sandbox_reload: Some(Err("boom".to_string())),
        ..Default::default()
    });
    let reply = harness.send("POST", "/v1/sandbox/reload", None).await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "sandbox_reload_failed");
    assert_eq!(reply.json()["error"]["message"], "boom");
}

#[tokio::test]
async fn sandbox_reload_reloads_every_loaded_workspace() {
    let startup = "/startup".to_string();
    let harness = Harness::with(HarnessOptions {
        info: Info {
            workspace: startup.clone(),
            ..Default::default()
        },
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            ..Default::default()
        },
        sandbox_reload: Some(Ok(SandboxInfo {
            mode: SandboxMode::Seatbelt,
            network: SandboxNetwork::Allowed,
            bash_available: true,
            reason: SandboxReason::None,
        })),
        sandbox_reload_others: vec![(
            "/other".to_string(),
            Ok(SandboxInfo {
                mode: SandboxMode::Off,
                network: SandboxNetwork::Unconfined,
                bash_available: true,
                reason: SandboxReason::None,
            }),
        )],
        ..Default::default()
    });
    let reply = harness.send("POST", "/v1/sandbox/reload", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["mode"], "seatbelt", "{body}");
    let workspaces = body["workspaces"].as_array().expect("workspaces array");
    assert_eq!(workspaces.len(), 2, "{body}");
    assert_eq!(workspaces[0]["workspace"], startup);
    assert_eq!(workspaces[0]["sandbox"]["mode"], "seatbelt");
    assert_eq!(workspaces[1]["workspace"], "/other");
    assert_eq!(workspaces[1]["sandbox"]["mode"], "off");
}

#[tokio::test]
async fn sandbox_reload_names_the_workspace_that_failed() {
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            ..Default::default()
        },
        sandbox_reload: Some(Ok(SandboxInfo::default())),
        sandbox_reload_others: vec![("/other".to_string(), Err("boom".to_string()))],
        ..Default::default()
    });
    let reply = harness.send("POST", "/v1/sandbox/reload", None).await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "sandbox_reload_failed");
    let message = reply.json()["error"]["message"]
        .as_str()
        .expect("message")
        .to_string();
    assert!(message.contains("/other"), "{message}");
    assert!(message.contains("boom"), "{message}");
}

#[tokio::test]
async fn approval_route_is_session_scoped_and_fails_closed() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/approvals/approval-1"),
            Some(r#"{"decision":"allow"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.json()["error"]["code"], "approval_failed");
    for body in [None, Some(r#"{"decision":"maybe"}"#), Some("{}")] {
        let bad = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/approvals/approval-1"),
                body,
            )
            .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{body:?}");
    }
    assert_eq!(
        harness
            .send(
                "POST",
                "/v1/sessions/missing/approvals/approval-1",
                Some(r#"{"decision":"allow"}"#),
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_task_routes_answer_empty_for_a_session_with_no_tasks() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}/tasks"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["tasks"].as_array().expect("tasks").len(), 0);

    for (method, path) in [
        ("GET", format!("/v1/sessions/{id}/tasks/t1")),
        ("POST", format!("/v1/sessions/{id}/tasks/t1/cancel")),
    ] {
        assert_eq!(
            harness.send(method, &path, None).await.status,
            StatusCode::NOT_FOUND,
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn the_mcp_route_answers_empty_for_a_session_with_no_servers_configured() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}/mcp"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json()["servers"].as_array().expect("servers").len(),
        0
    );
}

#[tokio::test]
async fn the_mcp_route_answers_404_for_an_unknown_session() {
    let harness = Harness::new();
    let reply = harness.send("GET", "/v1/sessions/missing/mcp", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.json()["error"]["code"], "not_found");
}

/// The routes read the runner's real registry. The list and detail task
/// routes, driven through the registry rather than through a scripted
/// `agent` tool call.
#[tokio::test]
async fn the_task_routes_list_and_detail_a_real_task() {
    let harness = Harness::new();
    let id = harness.create().await;
    let registry = harness
        .server
        .lookup(&id)
        .expect("session")
        .ctrl
        .subagent_tasks()
        .expect("registry");
    let added = registry
        .add(
            crate::subagent::tasks::Task {
                name: "lint".to_string(),
                agent: "reviewer".to_string(),
                description: "check the diff".to_string(),
                model: "gpt-5".to_string(),
                created_at: Some(chrono::Utc::now()),
                ..crate::subagent::tasks::Task::default()
            },
            None,
            None,
        )
        .expect("add");

    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}/tasks"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let listed = reply.json();
    let listed = listed["tasks"].as_array().expect("tasks");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], added.id);
    assert_eq!(listed[0]["name"], "lint");
    assert_eq!(listed[0]["agent"], "reviewer");
    assert_eq!(listed[0]["description"], "check the diff");
    assert_eq!(listed[0]["model"], "gpt-5");
    assert_eq!(listed[0]["status"], "queued");
    // The wire record never carries the prompt.
    assert!(listed[0].get("prompt").is_none());

    let detail = harness
        .send(
            "GET",
            &format!("/v1/sessions/{id}/tasks/{}", added.id),
            None,
        )
        .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
    assert_eq!(detail.json()["id"], added.id);
    assert_eq!(
        detail.json()["history"].as_array().expect("history").len(),
        0
    );

    let canceled = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/tasks/lint/cancel"),
            None,
        )
        .await;
    assert_eq!(canceled.status, StatusCode::OK, "{}", canceled.body);
    // Cancelling does not itself finish the task; once it is final the
    // route answers 409 `task_done`.
    assert_eq!(canceled.json()["status"], "queued");
    registry.finish(
        &added.id,
        crate::subagent::tasks::TaskStatus::Canceled,
        chrono::Utc::now(),
        "",
        "",
    );
    let repeat = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/tasks/lint/cancel"),
            None,
        )
        .await;
    assert_eq!(repeat.status, StatusCode::CONFLICT);
    assert_eq!(repeat.json()["error"]["code"], "task_done");
}

// ---- cross-process task routes (tasks.db) ----

/// A `TaskContext` for `parent_session`, workspace `/work`.
pub(super) fn agents_context(parent_session: &str) -> crate::subagent::record::TaskContext {
    crate::subagent::record::TaskContext {
        parent_session: parent_session.to_string(),
        parent_session_path: format!("/home/me/.otto/sessions/{parent_session}.jsonl"),
        workspace: "/work".to_string(),
        pid: 4_294_967_294, // a pid that cannot exist, so queued/running rows read as interrupted
        process_started_at: "2026-09-25T10:00:00Z".to_string(),
    }
}

/// A stored task row: `task_id`, `status`, and `created_at` (RFC 3339,
/// determines list order and the `before` cursor) vary per call.
pub(super) fn agents_task(
    task_id: &str,
    status: crate::subagent::tasks::TaskStatus,
    created_at: &str,
) -> crate::subagent::tasks::Task {
    crate::subagent::tasks::Task {
        id: task_id.to_string(),
        name: "reviewer".to_string(),
        agent: "code-reviewer".to_string(),
        description: "review the diff".to_string(),
        model: "gpt-5.1".to_string(),
        status,
        created_at: chrono::DateTime::parse_from_rfc3339(created_at)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc)),
        session_path: format!("/home/me/.otto/sessions/parent/{task_id}-child.jsonl"),
        ..crate::subagent::tasks::Task::default()
    }
}

#[tokio::test]
async fn the_v1_tasks_route_lists_newest_first_and_filters_by_status_and_workspace() {
    use crate::subagent::record::{Recorder, Store, TaskContext};
    use crate::subagent::tasks::TaskStatus;

    let store = Arc::new(Store::open_in_memory().expect("store"));
    store.upsert(
        &agents_context("s1"),
        &agents_task("t1", TaskStatus::Running, "2026-09-25T10:00:00Z"),
    );
    store.upsert(
        &agents_context("s1"),
        &agents_task("t2", TaskStatus::Succeeded, "2026-09-25T10:01:00Z"),
    );
    store.upsert(
        &TaskContext {
            workspace: "/other".to_string(),
            ..agents_context("s2")
        },
        &agents_task("t3", TaskStatus::Running, "2026-09-25T10:02:00Z"),
    );

    let harness = Harness::with(HarnessOptions {
        task_recorder: Some(store.clone()),
        ..HarnessOptions::default()
    });

    let reply = harness.send("GET", "/v1/tasks", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let body = reply.json();
    let tasks = body["tasks"].as_array().expect("tasks");
    // Newest first.
    assert_eq!(
        tasks
            .iter()
            .map(|t| t["task_id"].clone())
            .collect::<Vec<_>>(),
        vec!["t3", "t2", "t1"]
    );

    let by_status = harness
        .send("GET", "/v1/tasks?status=succeeded", None)
        .await
        .json();
    let by_status = by_status["tasks"].as_array().expect("tasks");
    assert_eq!(by_status.len(), 1);
    assert_eq!(by_status[0]["task_id"], "t2");

    let by_workspace = harness
        .send("GET", "/v1/tasks?workspace=/other", None)
        .await
        .json();
    let by_workspace = by_workspace["tasks"].as_array().expect("tasks");
    assert_eq!(by_workspace.len(), 1);
    assert_eq!(by_workspace[0]["task_id"], "t3");
}

#[tokio::test]
async fn the_v1_tasks_route_bounds_limit_and_pages_with_before() {
    use crate::subagent::record::{Recorder, Store};
    use crate::subagent::tasks::TaskStatus;

    let store = Arc::new(Store::open_in_memory().expect("store"));
    for n in 0..3 {
        store.upsert(
            &agents_context("s1"),
            &agents_task(
                &format!("t{n}"),
                TaskStatus::Succeeded,
                &format!("2026-09-25T10:0{n}:00Z"),
            ),
        );
    }
    let harness = Harness::with(HarnessOptions {
        task_recorder: Some(store.clone()),
        ..HarnessOptions::default()
    });

    // limit=1 returns only the newest row; next_before is an exclusive
    // cursor on its own created_at, so paging by it skips past it.
    let first_page = harness.send("GET", "/v1/tasks?limit=1", None).await.json();
    let tasks = first_page["tasks"].as_array().expect("tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["task_id"], "t2");
    let next_before = first_page["next_before"].as_str().expect("next_before");
    assert_eq!(next_before, "2026-09-25T10:02:00.000000000Z");

    let second_page = harness
        .send(
            "GET",
            &format!("/v1/tasks?limit=1&before={next_before}"),
            None,
        )
        .await
        .json();
    let tasks = second_page["tasks"].as_array().expect("tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["task_id"], "t1");

    // limit clamps to 500 rather than erroring on an out-of-range value.
    let clamped = harness
        .send("GET", "/v1/tasks?limit=100000", None)
        .await
        .json();
    assert_eq!(clamped["tasks"].as_array().expect("tasks").len(), 3);
}

#[tokio::test]
async fn the_v1_tasks_route_marks_cancelable_only_for_an_owned_non_final_task() {
    use crate::subagent::record::{Recorder, Store, TaskContext};
    use crate::subagent::tasks::TaskStatus;

    let store = Arc::new(Store::open_in_memory().expect("store"));
    let harness = Harness::with(HarnessOptions {
        task_recorder: Some(store.clone()),
        ..HarnessOptions::default()
    });
    let owned_session = harness.create().await;
    // The current process's own pid and start time, so a `queued`/
    // `running` row reads back as verifiably alive instead of
    // `interrupted` (see `record::interrupted`).
    let (pid, process_started_at) = crate::subagent::record::current_process();
    let live_context = |parent_session: &str| TaskContext {
        parent_session: parent_session.to_string(),
        parent_session_path: format!("/home/me/.otto/sessions/{parent_session}.jsonl"),
        workspace: "/work".to_string(),
        pid,
        process_started_at: process_started_at.clone(),
    };
    store.upsert(
        &live_context(&owned_session),
        &agents_task("running-owned", TaskStatus::Running, "2026-09-25T10:00:00Z"),
    );
    store.upsert(
        &live_context(&owned_session),
        &agents_task(
            "finished-owned",
            TaskStatus::Succeeded,
            "2026-09-25T10:01:00Z",
        ),
    );
    store.upsert(
        &live_context("not-open-here"),
        &agents_task(
            "running-elsewhere",
            TaskStatus::Running,
            "2026-09-25T10:02:00Z",
        ),
    );

    let body = harness.send("GET", "/v1/tasks", None).await.json();
    let tasks = body["tasks"].as_array().expect("tasks");
    let cancelable = |task_id: &str| {
        tasks.iter().find(|t| t["task_id"] == task_id).expect("row")["cancelable"]
            .as_bool()
            .expect("cancelable")
    };
    assert!(cancelable("running-owned"));
    assert!(!cancelable("finished-owned"));
    assert!(!cancelable("running-elsewhere"));
}

#[tokio::test]
async fn the_v1_tasks_detail_route_reads_the_child_transcript_and_reports_a_missing_one() {
    use crate::subagent::record::{Recorder, Store};
    use crate::subagent::tasks::TaskStatus;

    let sessions = tempfile::tempdir().expect("sessions");
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let harness = Harness::with(HarnessOptions {
        task_recorder: Some(store.clone()),
        ..HarnessOptions::default()
    });

    // A task whose child transcript file exists beside its parent.
    let parent_path = sessions.path().join("s1.jsonl");
    let child_store = crate::session::Store::create_child_lazy(
        &parent_path,
        "with-transcript-child",
        Header {
            version: CURRENT_VERSION,
            id: "t1-child".to_string(),
            workspace: "/work".to_string(),
            provider: "openai-compatible".to_string(),
            profile: "alpha".to_string(),
            model: "test-model".to_string(),
            created_at: chrono::Utc::now(),
        },
    )
    .expect("create child session");
    child_store
        .append_message(&testutil::user("hello from the child"))
        .expect("append");
    let child_path = child_store.path();
    let mut context = agents_context("s1");
    context.parent_session_path = parent_path.to_string_lossy().into_owned();
    let mut task = agents_task(
        "with-transcript",
        TaskStatus::Succeeded,
        "2026-09-25T10:00:00Z",
    );
    task.session_path = child_path;
    store.upsert(&context, &task);

    // A task whose recorded session_path does not exist on disk.
    store.upsert(
        &agents_context("s1"),
        &agents_task(
            "no-transcript",
            TaskStatus::Succeeded,
            "2026-09-25T10:01:00Z",
        ),
    );
    let mut unsafe_path = agents_task("unsafe-path", TaskStatus::Succeeded, "2026-09-25T10:02:00Z");
    unsafe_path.session_path = "/etc/passwd".to_string();
    store.upsert(&context, &unsafe_path);

    let found = harness
        .send("GET", "/v1/tasks/s1/with-transcript", None)
        .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.body);
    let found = found.json();
    assert_eq!(found["task"]["task_id"], "with-transcript");
    assert_eq!(found["transcript_missing"], false);
    assert_eq!(
        found["history"].as_array().expect("history").len(),
        1,
        "{found:?}"
    );

    let missing = harness
        .send("GET", "/v1/tasks/s1/no-transcript", None)
        .await
        .json();
    assert_eq!(missing["transcript_missing"], true);
    assert_eq!(missing["history"].as_array().expect("history").len(), 0);

    let unsafe_detail = harness
        .send("GET", "/v1/tasks/s1/unsafe-path", None)
        .await
        .json();
    assert_eq!(unsafe_detail["transcript_missing"], true);
    assert_eq!(
        unsafe_detail["history"].as_array().expect("history").len(),
        0
    );

    let not_found = harness.send("GET", "/v1/tasks/s1/nope", None).await;
    assert_eq!(not_found.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_timer_routes_list_and_cancel_a_real_timer() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reminders = harness
        .server
        .lookup(&id)
        .expect("session")
        .ctrl
        .reminders()
        .expect("registry");
    let scheduled = reminders
        .schedule(Duration::from_secs(600), "check the build".to_string())
        .expect("schedule");

    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}/timers"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let listed = reply.json();
    let listed = listed["timers"].as_array().expect("timers");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], scheduled.id);
    assert_eq!(listed[0]["message"], "check the build");
    assert!(listed[0]["fire_at"].is_string(), "{listed:?}");

    let path = format!("/v1/sessions/{id}/timers/{}/cancel", scheduled.id);
    let canceled = harness.send("POST", &path, None).await;
    assert_eq!(canceled.status, StatusCode::OK, "{}", canceled.body);
    assert_eq!(canceled.json()["id"], scheduled.id);
    assert!(reminders.list().is_empty());

    // A second cancel finds nothing left to cancel.
    let repeat = harness.send("POST", &path, None).await;
    assert_eq!(repeat.status, StatusCode::NOT_FOUND);
    assert_eq!(repeat.json()["error"]["code"], "not_found");
}
