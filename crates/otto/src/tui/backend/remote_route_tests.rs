//! Attached `/task`, `/timers`, `/mcp`, `/archive` and `/delete` dispatch
//! against a stand-in serve.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Method, StatusCode, Uri};
use axum::response::Response;
use serde_json::json;

use super::*;
use crate::client::Client;

type RequestLog = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

fn reply(status: StatusCode, body: serde_json::Value) -> Response {
    let body = if status == StatusCode::NO_CONTENT {
        String::new()
    } else {
        body.to_string()
    };
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

fn task_record() -> serde_json::Value {
    json!({
        "id": "a1", "agent": "explorer", "description": "look around", "status": "running",
        "created_at": "2023-11-14T22:13:20Z", "steps": 1, "tool_calls": 0,
        "usage": serde_json::to_value(otto_core::model::Usage::default()).unwrap(),
        "usage_present": false,
    })
}

fn route(method: &str, path: &str) -> (StatusCode, serde_json::Value) {
    let ok = StatusCode::OK;
    match (method, path) {
        ("GET", "/v1/sessions/s1/tasks") => (ok, json!({ "tasks": [task_record()] })),
        ("GET", "/v1/sessions/s1/tasks/a1") => {
            let mut detail = task_record();
            detail["history"] = json!([]);
            (ok, detail)
        }
        ("POST", "/v1/sessions/s1/tasks/a1/cancel") => (ok, task_record()),
        ("POST", "/v1/sessions/s1/tasks/done/cancel") => (
            StatusCode::CONFLICT,
            json!({ "error": { "code": "task_done", "message": "task already finished" } }),
        ),
        ("GET", "/v1/sessions/s1/timers") => (ok, json!({ "timers": [timer_record()] })),
        ("POST", "/v1/sessions/s1/timers/t1/cancel") => (ok, timer_record()),
        ("GET", "/v1/sessions/s1/mcp") => (
            ok,
            json!({ "servers": [
                { "name": "docs", "transport": "http", "protocol_version": "2026-07-28",
                  "state": "connected", "tools": 3, "error": null },
                { "name": "broken", "transport": "stdio", "protocol_version": null,
                  "state": "failed", "tools": 0, "error": "spawn failed" },
            ] }),
        ),
        ("POST", "/v1/sessions/s1/sandbox/changes") => (
            ok,
            json!({
                "summary": "Seatbelt sandbox, network allowed",
                "change_summary": "network allow",
                "config_path": "/c/config.toml",
                "followup": "retry the command",
            }),
        ),
        ("POST", "/v1/sessions/s1/approvals/ap1") => (
            ok,
            json!({ "decision": "allow", "always": true, "followup": "retry echo hi" }),
        ),
        ("POST", "/v1/sessions") => (ok, json!({ "id": "s2" })),
        ("GET", "/v1/sessions/s2/history") => (ok, json!([])),
        ("GET", "/v1/sessions/s1") => (ok, json!({ "id": "s1" })),
        ("GET", "/v1/sessions/s2") => (ok, json!({ "id": "s2" })),
        ("POST", p) if p.ends_with("/archive") => {
            (ok, json!({ "archived_path": "/w/archive/x.jsonl" }))
        }
        ("DELETE", p) if p.ends_with("/permanent") => (StatusCode::NO_CONTENT, json!(null)),
        _ => (
            StatusCode::NOT_FOUND,
            json!({ "error": { "code": "not_found", "message": "no route" } }),
        ),
    }
}

fn timer_record() -> serde_json::Value {
    json!({ "id": "t1", "fire_at": "2026-11-08T10:00:00Z", "message": "stretch" })
}

fn serve_stand_in() -> (Arc<Remote>, RequestLog, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("serve.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let log = RequestLog::default();
    let seen = log.clone();
    let app = Router::new().fallback(move |method: Method, uri: Uri, body: Bytes| {
        let seen = seen.clone();
        async move {
            let sent = serde_json::from_slice(&body).unwrap_or_default();
            seen.lock()
                .unwrap()
                .push((format!("{method} {}", uri.path()), sent));
            let (status, body) = route(method.as_str(), uri.path());
            reply(status, body)
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let remote = Remote::for_client(Client::new(&socket).unwrap(), "s1");
    (Arc::new(remote), log, dir)
}

fn ticket() -> Ticket {
    Ticket {
        workspace: "/work".into(),
        session: "s1".into(),
        surface: 0,
        generation: 1,
    }
}

async fn run(remote: &Arc<Remote>, request: Request) -> Result<Reply, String> {
    remote_request(remote.clone(), &ticket(), request).await
}

fn management(request: crate::app::management::Request) -> Request {
    Request::Management(request)
}

#[tokio::test]
async fn attached_tasks_list_and_detail_return_wire_records() {
    use crate::app::management::{Reply as Managed, Request as Asked};
    let (remote, _log, _dir) = serve_stand_in();
    let Reply::Management(Managed::Tasks(tasks)) =
        run(&remote, management(Asked::Tasks)).await.unwrap()
    else {
        panic!("tasks reply")
    };
    assert_eq!(tasks[0].id, "a1");
    let Reply::Management(Managed::Task(task)) = run(&remote, management(Asked::Task("a1".into())))
        .await
        .unwrap()
    else {
        panic!("task reply")
    };
    assert_eq!(task.description, "look around");
    let missing = run(&remote, management(Asked::Task("zzz".into())))
        .await
        .unwrap_err();
    assert_eq!(missing, "task not found");
}

#[tokio::test]
async fn attached_task_cancel_reports_the_id_and_the_server_conflict_text() {
    use crate::app::management::{Reply as Managed, Request as Asked};
    let (remote, log, _dir) = serve_stand_in();
    let reply = run(&remote, management(Asked::TaskCancel("a1".into())))
        .await
        .unwrap();
    assert!(matches!(reply, Reply::Management(Managed::TaskCancelled(id)) if id == "a1"));
    assert_eq!(
        log.lock().unwrap()[0].0,
        "POST /v1/sessions/s1/tasks/a1/cancel"
    );
    let error = run(&remote, management(Asked::TaskCancel("done".into())))
        .await
        .unwrap_err();
    assert!(error.contains("task_done"), "{error}");
}

#[tokio::test]
async fn attached_timers_list_and_cancel() {
    use crate::app::management::{Reply as Managed, Request as Asked};
    let (remote, _log, _dir) = serve_stand_in();
    let Reply::Management(Managed::Timers(timers)) =
        run(&remote, management(Asked::Timers)).await.unwrap()
    else {
        panic!("timers reply")
    };
    assert_eq!(timers[0].message, "stretch");
    let Reply::Management(Managed::TimerCancelled(timer)) =
        run(&remote, management(Asked::TimerCancel("t1".into())))
            .await
            .unwrap()
    else {
        panic!("timer cancel reply")
    };
    assert_eq!(timer.id, "t1");
}

#[tokio::test]
async fn attached_mcp_status_converts_wire_rows_to_server_status() {
    use crate::app::management::{Reply as Managed, Request as Asked};
    use crate::mcp::{Era, ServerState};
    let (remote, _log, _dir) = serve_stand_in();
    let Reply::Management(Managed::McpStatus(rows)) =
        run(&remote, management(Asked::McpStatus)).await.unwrap()
    else {
        panic!("mcp reply")
    };
    assert_eq!(rows[0].era, Some(Era::Modern));
    assert_eq!(rows[0].state, ServerState::Connected { tools: 3 });
    assert_eq!(rows[1].state, ServerState::Failed("spawn failed".into()));
    assert_eq!(rows[1].transport, "stdio");
}

#[tokio::test]
async fn archiving_the_current_session_opens_a_new_one_in_the_workspace() {
    let (remote, log, _dir) = serve_stand_in();
    let reply = run(&remote, Request::Command(Command::Archive("s1".into())))
        .await
        .unwrap();
    let Reply::Operation {
        message, history, ..
    } = reply
    else {
        panic!("operation reply")
    };
    assert_eq!(message, "Archived: /w/archive/x.jsonl\nSession: s2");
    assert!(history.is_some());
    assert_eq!(remote.session(), "s2");
    let log = log.lock().unwrap();
    assert_eq!(log[0].0, "POST /v1/sessions/s1/archive");
    assert_eq!(
        log[1],
        (
            "POST /v1/sessions".to_string(),
            json!({ "workspace": "/work" })
        )
    );
}

#[tokio::test]
async fn deleting_another_session_keeps_the_current_session() {
    let (remote, log, _dir) = serve_stand_in();
    let reply = run(&remote, Request::Command(Command::Delete("old".into())))
        .await
        .unwrap();
    let Reply::Operation {
        message, history, ..
    } = reply
    else {
        panic!("operation reply")
    };
    assert_eq!(message, "Session permanently deleted.");
    assert!(history.is_none());
    assert_eq!(remote.session(), "s1");
    assert_eq!(
        log.lock().unwrap()[0],
        (
            "DELETE /v1/sessions/old/permanent".to_string(),
            json!({ "confirm": "old" })
        )
    );
}

#[tokio::test]
async fn deleting_the_current_session_opens_a_new_one() {
    let (remote, _log, _dir) = serve_stand_in();
    let reply = run(&remote, Request::Command(Command::Delete("s1".into())))
        .await
        .unwrap();
    let Reply::Operation {
        message, history, ..
    } = reply
    else {
        panic!("operation reply")
    };
    assert_eq!(message, "Session permanently deleted.\nSession: s2");
    assert!(history.is_some());
}

#[tokio::test]
async fn attached_sandbox_changes_post_the_change_and_submit_the_followup() {
    let cases = [
        (
            Command::SandboxAllow("/data".into()),
            "allow_read_path",
            "/data",
        ),
        (Command::SandboxNetwork("allow".into()), "network", "allow"),
        (
            Command::SandboxExclude("git status".into()),
            "exclude_command",
            "git status",
        ),
    ];
    for (command, change, value) in cases {
        // One stand-in per command: each command refreshes the session info.
        let (remote, log, _dir) = serve_stand_in();
        let Reply::Operation {
            message, prompt, ..
        } = run(&remote, Request::Command(command)).await.unwrap()
        else {
            panic!("operation reply")
        };
        assert!(
            message.starts_with("Sandbox: Seatbelt sandbox, network allowed (network allow)"),
            "{message}"
        );
        assert_eq!(prompt.as_deref(), Some("retry the command"));
        let log = log.lock().unwrap();
        let (path, sent) = log
            .iter()
            .rev()
            .find(|(path, _)| path.ends_with("/sandbox/changes"))
            .unwrap();
        assert_eq!(path, "POST /v1/sessions/s1/sandbox/changes");
        assert_eq!(sent, &json!({ "change": change, "value": value }));
        if change == "exclude_command" {
            assert!(
                message.contains("in /c/config.toml and reloaded"),
                "{message}"
            );
        }
    }
}

#[tokio::test]
async fn attached_approve_always_submits_the_server_followup() {
    let (remote, log, _dir) = serve_stand_in();
    let Reply::Operation {
        message, prompt, ..
    } = run(
        &remote,
        Request::Command(Command::ApproveAlways("ap1".into())),
    )
    .await
    .unwrap()
    else {
        panic!("operation reply")
    };
    assert_eq!(message, "Approved ap1.");
    assert_eq!(prompt.as_deref(), Some("retry echo hi"));
    assert_eq!(
        log.lock().unwrap()[0],
        (
            "POST /v1/sessions/s1/approvals/ap1".to_string(),
            json!({ "decision": "allow", "always": true })
        )
    );
}

#[tokio::test]
async fn attached_sandbox_path_preview_resolves_on_the_client() {
    let (remote, log, _dir) = serve_stand_in();
    let target = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(target.path()).unwrap();
    let reply = run(
        &remote,
        Request::SandboxPath(target.path().to_string_lossy().into_owned()),
    )
    .await
    .unwrap();
    assert!(matches!(reply, Reply::SandboxPath(path) if path == canonical.to_string_lossy()));
    let missing = run(
        &remote,
        Request::SandboxPath("/nonexistent/otto-preview".into()),
    )
    .await;
    assert!(missing.is_err());
    assert!(
        log.lock().unwrap().is_empty(),
        "the preview must not call serve"
    );
}
