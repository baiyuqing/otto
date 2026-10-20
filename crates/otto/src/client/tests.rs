
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Method, StatusCode, Uri};
use axum::response::Response;

use super::*;

/// One request as serve saw it: `METHOD /path?query` and the body text.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// A serve stand-in on a Unix socket in a temp dir. `reply` gets the
/// method and the path with query and returns the response; every
/// request is recorded in the returned log.
fn stub<F>(reply: F) -> (Client, Seen, tempfile::TempDir)
where
    F: Fn(&str, &str) -> Response + Clone + Send + Sync + 'static,
{
    let dir = tempfile::tempdir().expect("dir");
    let socket = dir.path().join("serve.sock");
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    let seen: Seen = Seen::default();
    let log = Arc::clone(&seen);
    let app = Router::new().fallback(move |method: Method, uri: Uri, body: Bytes| {
        let reply = reply.clone();
        let log = Arc::clone(&log);
        async move {
            let target = uri.path_and_query().map(ToString::to_string).unwrap();
            log.lock().unwrap().push((
                format!("{method} {target}"),
                String::from_utf8_lossy(&body).into_owned(),
            ));
            reply(method.as_str(), &target)
        }
    });
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (Client::new(&socket).expect("client"), seen, dir)
}

fn json_body(status: StatusCode, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn sse(turn_id: &str, frames: &str) -> Response {
    Response::builder()
        .header("content-type", "text/event-stream")
        .header(TURN_ID_HEADER, turn_id)
        .body(Body::from(frames.to_string()))
        .unwrap()
}

fn request(seen: &Seen, index: usize) -> (String, Value) {
    let (line, body) = seen.lock().unwrap()[index].clone();
    let body = serde_json::from_str(&body).unwrap_or(Value::Null);
    (line, body)
}

/// An error that names only itself, as `reqwest::Error` does.
#[derive(Debug)]
struct Bare(&'static str);

impl fmt::Display for Bare {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Bare {}

/// [`Bare`] with the OS reason as its only cause.
#[derive(Debug)]
struct Wrapped(Bare, Bare);

impl fmt::Display for Wrapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Wrapped {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.1)
    }
}

#[test]
fn unreachable_errors_name_the_cause_a_display_leaves_out() {
    let error = unreachable_error(Wrapped(
        Bare("error sending request for url (http://otto/healthz)"),
        Bare("No such file or directory (os error 2)"),
    ));
    assert_eq!(
        error.to_string(),
        "error sending request for url (http://otto/healthz): No such file or directory (os error 2)"
    );
}

/// Context that is already formatted has no chain to add and keeps its
/// shape.
#[test]
fn unreachable_text_keeps_a_message_verbatim() {
    let error = unreachable_text(format!("decode frame: {}", Bare("truncated")));
    assert_eq!(error.to_string(), "decode frame: truncated");
}

#[tokio::test]
async fn a_missing_socket_is_unreachable_not_an_http_error() {
    let dir = tempfile::tempdir().expect("dir");
    let client = Client::new(&dir.path().join("absent.sock")).expect("client");
    let error = client.healthz().await.expect_err("no server");
    assert!(matches!(error, Error::Unreachable(_)), "{error}");
    // `otto acp --attach` and the attached TUI print this text to explain
    // an absent `otto serve`, so the OS reason has to reach it.
    let text = error.to_string();
    assert!(text.contains("No such file or directory"), "{text}");
}

#[test]
fn http_errors_display_status_code_and_message() {
    let error = Error::Http {
        status: 409,
        code: "approval_decided".into(),
        message: "already decided".into(),
    };
    let text = error.to_string();
    assert!(
        text.contains("409") && text.contains("approval_decided"),
        "{text}"
    );
}

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

#[tokio::test]
async fn compact_decodes_a_compaction_and_maps_an_empty_body_to_none() {
    let (client, seen, _dir) = stub(|_, path| {
        if path.contains("full") {
            json_body(
                StatusCode::OK,
                r#"{"reason":"manual","tokens_before":900,"estimated_tokens_after":100}"#,
            )
        } else {
            Response::new(Body::empty())
        }
    });
    let done = client.compact("full", "api").await.unwrap().expect("some");
    assert_eq!(
        (done.tokens_before, done.estimated_tokens_after),
        (Some(900), Some(100))
    );
    assert_eq!(
        request(&seen, 0),
        (
            "POST /v1/sessions/full/compact".into(),
            json!({ "focus": "api" })
        )
    );
    assert_eq!(client.compact("cut", "").await.unwrap(), None);
}

#[tokio::test]
async fn skill_manager_client_uses_session_routes_and_encodes_qualified_names() {
    let (client, seen, _dir) = stub(|_, path| {
        if path.ends_with("/skill-manager") {
            json_body(StatusCode::OK, r#"{"global_enabled":false,"skills":[]}"#)
        } else if path.contains("/skill-manager/") {
            json_body(
                StatusCode::OK,
                r#"{"skill":{"name":"plugin:lint","description":"lint","contract":null,"directory":"/skills/lint","path":"/skills/lint/SKILL.md"},"body":"instructions","contract_check":null}"#,
            )
        } else {
            json_body(
                StatusCode::OK,
                r#"{"name":"plugin:lint","enabled":true,"restart_required":true}"#,
            )
        }
    });
    let inventory = client.skill_manager("s1").await.unwrap();
    assert!(!inventory.global_enabled);
    assert!(inventory.skills.is_empty());
    assert_eq!(
        client.skill_info("s1", "plugin:lint").await.unwrap().body,
        "instructions"
    );
    assert!(
        client
            .set_skill_enabled("s1", "plugin:lint", true)
            .await
            .unwrap()
            .contains("restart Otto")
    );
    assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1/skill-manager");
    assert_eq!(
        request(&seen, 1).0,
        "GET /v1/sessions/s1/skill-manager/plugin:lint"
    );
    assert_eq!(
        request(&seen, 2).0,
        "POST /v1/sessions/s1/skills/plugin:lint/enable"
    );
    client.skill_info("s1", "bad/name").await.unwrap();
    assert_eq!(
        request(&seen, 3).0,
        "GET /v1/sessions/s1/skill-manager/bad%2Fname"
    );
}

#[tokio::test]
async fn reflection_client_calls_use_the_existing_routes_and_empty_cancel_reply() {
    let (client, seen, _dir) = stub(|_, path| match path.split('?').next().unwrap_or(path) {
        "/v1/sessions/s1/reflect" => json_body(
            StatusCode::OK,
            r#"{"line":"reflection: 1 candidate queued"}"#,
        ),
        "/v1/sessions/s1/reflect/cancel" => json_body(StatusCode::ACCEPTED, ""),
        "/v1/sessions/s1/reflection/skills" => json_body(
            StatusCode::OK,
            r#"{"enabled":true,"skills":[{"name":"lint","run_id":"r1","session_id":"s1","reason":"reason","created_at":"now","updated_at":"now","owned":true}]}"#,
        ),
        "/v1/sessions/s1/notices" => json_body(
            StatusCode::OK,
            r#"{"notices":[{"id":3,"text":"reflection: done","presentation_origin":"chat:telegram:42"}],"last":3}"#,
        ),

        _ => json_body(StatusCode::OK, r#"{"name":"lint","result":"removed"}"#),
    });
    assert_eq!(
        client
            .reflect("s1", "focus", "acp-1")
            .await
            .unwrap()
            .unwrap()
            .line,
        "reflection: 1 candidate queued"
    );
    client.cancel_reflection("s1", "acp-1").await.unwrap();
    let skills = client.generated_skills("s1").await.unwrap();
    assert_eq!(skills.skills[0].name, "lint");
    assert_eq!(
        client.revert_generated_skill("s1", "lint").await.unwrap(),
        "removed"
    );
    let notices = client.reflection_notices("s1", 2).await.unwrap();
    assert_eq!(notices.notices[0].id, 3);
    assert_eq!(notices.notices[0].presentation_origin, "chat:telegram:42");
    assert_eq!(notices.last, 3);
    assert_eq!(
        request(&seen, 0).1,
        json!({"focus":"focus", "request_id":"acp-1"})
    );
    assert_eq!(request(&seen, 1).1, json!({"request_id":"acp-1"}));
    assert_eq!(request(&seen, 4).0, "GET /v1/sessions/s1/notices?after=2");
}

#[tokio::test]
async fn reflection_run_logs_use_their_routes_and_a_missing_run_is_none() {
    let (client, seen, _dir) = stub(|_, path| match path {
        "/v1/sessions/s1/reflection/runs" => json_body(
            StatusCode::OK,
            r#"{"enabled":true,"runs":[{"id":"abcd1234","path":"/h/.otto/reflection/runs/x.md","started":"2026-10-05T02:03:00Z","trigger":"requested","status":"ok","headline":"Read the docs first."}]}"#,
        ),
        "/v1/sessions/s1/reflection/runs/abcd1234" => json_body(
            StatusCode::OK,
            r##"{"id":"abcd1234","path":"/h/.otto/reflection/runs/x.md","content":"# Reflection run"}"##,
        ),
        _ => json_body(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":"not_found","message":"reflection run not found"}}"#,
        ),
    });
    let list = client.reflection_runs("s1").await.unwrap();
    assert!(list.enabled);
    assert_eq!(list.runs[0].id, "abcd1234");
    assert_eq!(list.runs[0].headline, "Read the docs first.");
    let log = client
        .reflection_run("s1", "abcd1234")
        .await
        .unwrap()
        .expect("log");
    assert_eq!(log.content, "# Reflection run");
    assert!(client.reflection_run("s1", "a/b").await.unwrap().is_none());
    assert_eq!(
        request(&seen, 2).0,
        "GET /v1/sessions/s1/reflection/runs/a%2Fb"
    );
}

#[tokio::test]
async fn trace_span_escapes_the_span_id_and_maps_404_to_none() {
    let (client, seen, _dir) = stub(|_, path| match path {
        "/v1/sessions/s1/trace/t1%3Acall%2F1" => {
            json_body(StatusCode::OK, r#"{"spanId":"t1:call/1","sections":[]}"#)
        }
        _ => json_body(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":"not_found","message":"span not found"}}"#,
        ),
    });

    let content = client.trace_span("s1", "t1:call/1").await.unwrap();
    assert_eq!(content.expect("content").span_id, "t1:call/1");
    assert!(client.trace_span("s1", "other").await.unwrap().is_none());
    assert_eq!(
        request(&seen, 0).0,
        "GET /v1/sessions/s1/trace/t1%3Acall%2F1"
    );
}

#[tokio::test]
async fn task_control_client_uses_session_routes_and_transient_channel() {
    let (client, seen, _dir) = stub(|_, path| match path {
        "/v1/sessions/s1/tasks/steer" => json_body(
            StatusCode::OK,
            r#"{"id":7,"text":"inspect the result","status":"queued"}"#,
        ),
        "/v1/sessions/s1/tasks/control" => json_body(
            StatusCode::OK,
            r#"{"text":"I can guide the task","queued":false}"#,
        ),
        _ => json_body(StatusCode::NOT_FOUND, ""),
    });

    let steer = client.steer("s1", "inspect the result").await.unwrap();
    assert_eq!(steer.id, 7);
    assert_eq!(steer.text, "inspect the result");
    assert_eq!(steer.status, otto_core::agent::steering::Status::Queued);
    let reply = client
        .control_message_with_channel("s1", "what can you do?", Some(Channel::Telegram))
        .await
        .unwrap()
        .expect("routed reply");
    assert_eq!(reply.text, "I can guide the task");
    assert!(!reply.queued);
    assert_eq!(
        request(&seen, 0),
        (
            "POST /v1/sessions/s1/tasks/steer".into(),
            json!({"text":"inspect the result"})
        )
    );
    assert_eq!(
        request(&seen, 1),
        (
            "POST /v1/sessions/s1/tasks/control".into(),
            json!({"text":"what can you do?","channel":"telegram"})
        )
    );
}

#[tokio::test]
async fn reflection_treats_an_empty_cancelled_response_as_none() {
    let (client, _, _dir) = stub(|_, _| json_body(StatusCode::OK, "  "));
    assert!(client.reflect("s1", "", "acp-2").await.unwrap().is_none());
}

#[tokio::test]
async fn context_decodes_the_report() {
    let report = ContextReport {
        model: "m".into(),
        context_window: 1000,
        compaction_threshold: 800,
        estimated_total: 120,
        estimate_complete: true,
        reported_input_tokens: Some(110),
        sections: vec![otto_core::agent::context_report::ContextSection {
            kind: otto_core::agent::context_report::SectionKind::Messages,
            tokens: 120,
            items: vec![],
        }],
    };
    let body = serde_json::to_string(&report).unwrap();
    let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
    assert_eq!(client.context("s1").await.unwrap(), report);
    assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1/context");
}

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
