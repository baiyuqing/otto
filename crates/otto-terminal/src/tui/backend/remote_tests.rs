//! Attached `/model` and `/thinking` dispatch against a stand-in serve.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Method, StatusCode, Uri};
use axum::response::Response;

use super::*;
use otto_client::Client;

type RequestLog = Arc<Mutex<Vec<String>>>;

fn json_reply(status: StatusCode, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A serve stand-in that keeps the session's model and thinking level and
/// records `METHOD /path` for every request.
fn serve_stand_in() -> (Arc<Remote>, RequestLog, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("serve.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let log = RequestLog::default();
    let session_state = Arc::new(Mutex::new(("a/x".to_string(), String::new())));
    let seen = log.clone();
    let app = Router::new().fallback(move |method: Method, uri: Uri, body: Bytes| {
        let seen = seen.clone();
        let session_state = session_state.clone();
        async move {
            let path = uri.path().to_string();
            seen.lock().unwrap().push(format!("{method} {path}"));
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let mut state = session_state.lock().unwrap();
            if path == "/v1/config/providers" {
                return json_reply(
                    StatusCode::OK,
                    r#"{"providers":[{"name":"b","type":"claude","models":[{"selector":"b/y","model_id":"y","thinking":"low"}]},
                    {"name":"a","type":"openai-compatible","models":[{"selector":"a/x","model_id":"x"}]}]}"#,
                );
            }
            if path.ends_with("/model") {
                state.0 = body["model"].as_str().unwrap().to_string();
            } else if path.ends_with("/thinking") {
                let level = body["thinking"].as_str().unwrap();
                if level == "bogus" {
                    return json_reply(
                        StatusCode::BAD_REQUEST,
                        r#"{"error":{"code":"invalid_thinking","message":"thinking must be one of unset, low, medium, high, xhigh, max"}}"#,
                    );
                }
                state.1 = if level == "unset" { String::new() } else { level.to_string() };
            }
            let (provider, model) = state.0.split_once('/').unwrap();
            json_reply(
                StatusCode::OK,
                &serde_json::json!({
                    "id": "s1", "provider": provider, "provider_type": "claude",
                    "model": model, "thinking": state.1,
                })
                .to_string(),
            )
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

#[tokio::test]
async fn attached_models_request_lists_configured_models_sorted_by_selector() {
    let (remote, log, _dir) = serve_stand_in();
    let Reply::Models(rows) = run(&remote, Request::Models).await.unwrap() else {
        panic!("models reply")
    };
    let selectors: Vec<_> = rows.iter().map(|row| row.selector.as_str()).collect();
    assert_eq!(selectors, ["a/x", "b/y"]);
    assert_eq!(rows[1].provider, "b");
    assert_eq!(rows[1].provider_type, "claude");
    assert_eq!(rows[1].model_id, "y");
    assert_eq!(rows[1].thinking, "low");
    assert_eq!(log.lock().unwrap().as_slice(), ["GET /v1/config/providers"]);
}

#[tokio::test]
async fn attached_model_switch_changes_the_session_and_does_not_claim_a_saved_default() {
    let (remote, log, _dir) = serve_stand_in();
    let reply = run(
        &remote,
        Request::Command(Command::SwitchModel("b/y".into())),
    )
    .await
    .unwrap();
    let Reply::Operation {
        message,
        info,
        history,
        ..
    } = reply
    else {
        panic!("operation reply")
    };
    assert_eq!(message, "Switched to model b/y (type claude).");
    assert_eq!(info.model_selector, "b/y");
    assert!(history.is_some());
    assert_eq!(log.lock().unwrap()[0], "POST /v1/sessions/s1/model");
}

#[tokio::test]
async fn attached_thinking_with_save_changes_the_session_and_reports_nothing_saved() {
    let (remote, log, _dir) = serve_stand_in();
    let reply = run(
        &remote,
        Request::Command(Command::SetThinking {
            thinking: "high".into(),
            save: true,
        }),
    )
    .await
    .unwrap();
    let Reply::Operation { message, info, .. } = reply else {
        panic!("operation reply")
    };
    assert_eq!(
        message,
        "Thinking: high. Not saved: attached sessions change the session only."
    );
    assert_eq!(info.thinking, "high");
    assert_eq!(log.lock().unwrap()[0], "POST /v1/sessions/s1/thinking");
}

#[tokio::test]
async fn attached_model_switch_with_a_rejected_thinking_level_keeps_the_switch_receipt() {
    let (remote, _log, _dir) = serve_stand_in();
    let reply = run(
        &remote,
        Request::Command(Command::SwitchModelThinking {
            model: "b/y".into(),
            thinking: "bogus".into(),
            save: false,
        }),
    )
    .await
    .unwrap();
    let Reply::Operation { message, info, .. } = reply else {
        panic!("operation reply")
    };
    assert!(
        message.starts_with("Switched to model b/y (type claude)."),
        "{message}"
    );
    assert!(message.contains("/thinking: "), "{message}");
    assert!(
        message.ends_with("The model switch was applied."),
        "{message}"
    );
    assert_eq!(info.model_selector, "b/y");
}
