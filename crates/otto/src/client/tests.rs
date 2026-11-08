use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Method, StatusCode, Uri};
use axum::response::Response;

use super::*;

mod sessions;
mod tasks;
mod timers_and_mcp;

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

mod session_operations;
