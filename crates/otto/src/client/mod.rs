//! An HTTP client of `otto serve` over its Unix socket.
//!
//! Used by `otto acp --attach` and by the TUI's `--attach` mode; the calls
//! are the ones those two frontends need. Contract:
//!
//! - One [`Client`] holds one connection pool to one socket path. The URL
//!   host is ignored by the transport.
//! - Every call returns [`Error::Unreachable`] when the socket cannot be
//!   reached, the connection breaks or a body does not decode, and
//!   [`Error::Http`] when serve answered with a non-2xx status. The latter
//!   keeps the status and the `code` and `message` of serve's
//!   `{"error":{"code","message"}}` body (empty when the body has another
//!   shape).
//! - [`Client::start_turn`] returns once the response headers arrive, which
//!   serve sends before a queued turn starts. [`TurnStream::next`] then
//!   yields the turn's frames in order and `Ok(None)` when serve closes the
//!   stream. A turn's last frame is `turn_end`; a stream that closes without
//!   it was cut off, and the caller must treat that as a lost connection.
//! - [`Client::turn_events`] replays and follows one turn's stream and
//!   [`Client::status`] opens `GET /v1/status`. [`StatusStream::next`] yields
//!   one full snapshot per `status` frame (the first on connect) and
//!   `Ok(None)` when serve closes the stream. Both streams share the SSE
//!   reader of [`TurnStream`] and are cancel safe in the same way.
//! - No call retries and no call has a timeout: a turn stream stays open for
//!   as long as the turn runs.

use std::collections::VecDeque;
use std::fmt;
use std::path::Path;

use otto_core::agent::context_report::ContextReport;
use otto_core::model::{Block, Message, Usage};
use otto_core::wire::events::{WireCompaction, WireEvent};
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cli::info::{SandboxMode, SandboxNetwork};
use crate::mcp::sse::SseParser;
use crate::subagent::record::{ListQuery, ListResult, TaskRow};

const BASE: &str = "http://otto";
/// The largest SSE frame accepted from serve: a tool result frame carries at
/// most the tool output limit, so 64 MiB is above anything serve writes.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const TURN_ID_HEADER: &str = "otto-turn-id";

#[derive(Debug)]
pub enum Error {
    /// Serve could not be reached, the connection broke, or a response did
    /// not decode.
    Unreachable(String),
    /// Serve answered with an error status.
    Http {
        status: u16,
        code: String,
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(message) => f.write_str(message),
            Self::Http {
                status,
                code,
                message,
            } => write!(f, "HTTP {status} {code}: {message}"),
        }
    }
}

impl std::error::Error for Error {}

fn unreachable_error(error: impl fmt::Display) -> Error {
    Error::Unreachable(error.to_string())
}

/// A row of `GET /v1/sessions`; absent fields are empty.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionRow {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub last_user_text: String,
    /// RFC 3339; empty for a session that has no file yet.
    #[serde(default)]
    pub modified: String,
}

pub struct Client {
    http: reqwest::Client,
}

impl Client {
    #[cfg(unix)]
    pub fn new(socket: &Path) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .unix_socket(socket)
            .no_proxy()
            .build()
            .map_err(unreachable_error)?;
        Ok(Self { http })
    }

    /// `GET /healthz`.
    pub async fn healthz(&self) -> Result<(), Error> {
        self.send(self.http.get(format!("{BASE}/healthz")))
            .await
            .map(drop)
    }

    /// `POST /v1/sessions`: a new session in `workspace`, or with `resume`
    /// the stored session of that id. Without `workspace`, serve uses its
    /// startup workspace for a new session and searches every loaded
    /// workspace for a resumed one. Returns the session id.
    pub async fn open_session(
        &self,
        workspace: Option<&str>,
        resume: Option<&str>,
    ) -> Result<String, Error> {
        let mut body = json!({});
        if let Some(workspace) = workspace {
            body["workspace"] = json!(workspace);
        }
        if let Some(id) = resume {
            body["resume"] = json!(id);
        }
        let response = self
            .send(self.post(&format!("{BASE}/v1/sessions"), &body))
            .await?;
        #[derive(Deserialize)]
        struct Opened {
            id: String,
        }
        Ok(decode::<Opened>(response).await?.id)
    }

    /// `GET /v1/sessions?workspace=...`.
    pub async fn list_sessions(&self, workspace: &str) -> Result<Vec<SessionRow>, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions"))
                    .query(&[("workspace", workspace)]),
            )
            .await?;
        #[derive(Deserialize)]
        struct Listed {
            sessions: Vec<SessionRow>,
        }
        Ok(decode::<Listed>(response).await?.sessions)
    }

    /// `GET /v1/sessions/{id}/history`; with `before_turn`, only the
    /// messages that existed when that turn started.
    pub async fn history(
        &self,
        session_id: &str,
        before_turn: Option<&str>,
    ) -> Result<Vec<Message>, Error> {
        let mut request = self
            .http
            .get(format!("{BASE}/v1/sessions/{session_id}/history"));
        if let Some(turn_id) = before_turn {
            request = request.query(&[("before_turn", turn_id)]);
        }
        decode(self.send(request).await?).await
    }

    /// `POST /v1/sessions/{id}/turns` with `queue: true`. `image` is an
    /// image block; its data and MIME type go in the `image` field.
    pub async fn start_turn(
        &self,
        session_id: &str,
        text: &str,
        image: Option<&Block>,
    ) -> Result<TurnStream, Error> {
        let mut body = json!({ "text": text, "queue": true });
        if let Some(image) = image {
            body["image"] = json!({ "data": image.data, "mime_type": image.mime_type });
        }
        let response = self
            .send(self.post(&format!("{BASE}/v1/sessions/{session_id}/turns"), &body))
            .await?;
        let turn_id = response
            .headers()
            .get(TURN_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| unreachable_error("response has no Otto-Turn-Id header"))?;
        Ok(TurnStream::new(turn_id, response))
    }

    /// `GET /v1/sessions/{id}/turns/{turn_id}/events`: the events with an
    /// index above `after`, or from the first event when `None`, then the
    /// live ones.
    pub async fn turn_events(
        &self,
        session_id: &str,
        turn_id: &str,
        after: Option<usize>,
    ) -> Result<TurnStream, Error> {
        let mut request = self.http.get(format!(
            "{BASE}/v1/sessions/{session_id}/turns/{turn_id}/events"
        ));
        if let Some(after) = after {
            request = request.query(&[("after", after)]);
        }
        let response = self.send(request).await?;
        Ok(TurnStream::new(turn_id.to_string(), response))
    }

    /// `GET /v1/status`: snapshots of every open session in serve.
    pub async fn status(&self) -> Result<StatusStream, Error> {
        let response = self
            .send(self.http.get(format!("{BASE}/v1/status")))
            .await?;
        Ok(StatusStream {
            body: SseBody::new(response),
        })
    }

    /// `GET /v1/sessions/{id}`. `session_path` is empty, `reason` is the
    /// default and `pending` is false: the wire carries none of them.
    pub async fn session(&self, session_id: &str) -> Result<crate::app::Info, Error> {
        let response = self
            .send(self.http.get(format!("{BASE}/v1/sessions/{session_id}")))
            .await?;
        let wire: SessionWire = decode(response).await?;
        Ok(wire.into_info())
    }

    /// `PATCH /v1/sessions/{id}`.
    pub async fn rename_session(&self, session_id: &str, name: &str) -> Result<(), Error> {
        self.send(
            self.http
                .patch(format!("{BASE}/v1/sessions/{session_id}"))
                .header(CONTENT_TYPE, "application/json")
                .body(json!({ "name": name }).to_string()),
        )
        .await
        .map(drop)
    }

    /// `POST /v1/sessions/{id}/compact`. `None` when serve answered 200 with
    /// an empty body, which it does when shutdown cancelled the compaction.
    pub async fn compact(
        &self,
        session_id: &str,
        focus: &str,
    ) -> Result<Option<WireCompaction>, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/compact"),
                &json!({ "focus": focus }),
            ))
            .await?;
        let body = response.bytes().await.map_err(unreachable_error)?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| unreachable_error(format!("decode response: {error}")))
    }

    /// `GET /v1/sessions/{id}/context`.
    pub async fn context(&self, session_id: &str) -> Result<ContextReport, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/context")),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/tasks`: the open session's sub-agent tasks.
    /// `prompt` and `context` are empty; the wire omits them.
    pub async fn session_tasks(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::subagent::tasks::Task>, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/tasks")),
            )
            .await?;
        #[derive(Deserialize)]
        struct Listed {
            tasks: Vec<crate::app::tasks::Task>,
        }
        Ok(decode::<Listed>(response)
            .await?
            .tasks
            .into_iter()
            .map(crate::app::tasks::from_wire)
            .collect())
    }

    /// `GET /v1/tasks`, from `tasks.db`; only the `Some` fields of `query`
    /// are sent.
    pub async fn tasks(&self, query: &ListQuery) -> Result<ListResult, Error> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(status) = &query.status {
            params.push(("status", status.clone()));
        }
        if let Some(workspace) = &query.workspace {
            params.push(("workspace", workspace.clone()));
        }
        if let Some(limit) = query.limit {
            params.push(("limit", limit.to_string()));
        }
        if let Some(before) = &query.before {
            params.push(("before", before.clone()));
        }
        let response = self
            .send(self.http.get(format!("{BASE}/v1/tasks")).query(&params))
            .await?;
        #[derive(Deserialize)]
        struct Listed {
            tasks: Vec<TaskRow>,
            #[serde(default)]
            next_before: String,
        }
        let listed: Listed = decode(response).await?;
        Ok(ListResult {
            tasks: listed.tasks,
            next_before: listed.next_before,
        })
    }

    /// `GET /v1/tasks/{parent_session}/{task_id}`; `None` on 404.
    pub async fn task(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Option<TaskRow>, Error> {
        let result = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/tasks/{parent_session}/{task_id}")),
            )
            .await;
        let response = match result {
            Ok(response) => response,
            Err(Error::Http { status: 404, .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        #[derive(Deserialize)]
        struct Detail {
            task: TaskRow,
        }
        Ok(Some(decode::<Detail>(response).await?.task))
    }

    /// `POST /v1/sandbox/reload`; returns the sandbox summary line.
    pub async fn reload_sandbox(&self) -> Result<String, Error> {
        let response = self
            .send(self.http.post(format!("{BASE}/v1/sandbox/reload")))
            .await?;
        #[derive(Deserialize)]
        struct Reloaded {
            #[serde(default)]
            summary: String,
        }
        Ok(decode::<Reloaded>(response).await?.summary)
    }

    /// `POST /v1/sessions/{id}/turns/{turn_id}/cancel`, for a queued or a
    /// running turn.
    pub async fn cancel_turn(&self, session_id: &str, turn_id: &str) -> Result<(), Error> {
        self.send(self.http.post(format!(
            "{BASE}/v1/sessions/{session_id}/turns/{turn_id}/cancel"
        )))
        .await
        .map(drop)
    }

    /// `POST /v1/sessions/{id}/approvals/{approval_id}`; `allow` is the
    /// decision `allow`, otherwise `deny`.
    pub async fn decide_approval(
        &self,
        session_id: &str,
        approval_id: &str,
        allow: bool,
    ) -> Result<(), Error> {
        let decision = if allow { "allow" } else { "deny" };
        self.send(self.post(
            &format!("{BASE}/v1/sessions/{session_id}/approvals/{approval_id}"),
            &json!({ "decision": decision }),
        ))
        .await
        .map(drop)
    }

    fn post(&self, url: &str, body: &Value) -> reqwest::RequestBuilder {
        self.http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string())
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, Error> {
        let response = request.send().await.map_err(unreachable_error)?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.bytes().await.unwrap_or_default();
        let error = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|value| value.get("error").cloned())
            .unwrap_or(Value::Null);
        let field = |name: &str| error.get(name).and_then(Value::as_str).unwrap_or_default();
        Err(Error::Http {
            status: status.as_u16(),
            code: field("code").to_string(),
            message: field("message").to_string(),
        })
    }
}

async fn decode<T: for<'de> Deserialize<'de>>(response: reqwest::Response) -> Result<T, Error> {
    let body = response.bytes().await.map_err(unreachable_error)?;
    serde_json::from_slice(&body)
        .map_err(|error| unreachable_error(format!("decode response: {error}")))
}

/// The `GET /v1/sessions/{id}` body, the fields [`crate::app::Info`] reads.
#[derive(Deserialize)]
struct SessionWire {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    context_window: i64,
    #[serde(default)]
    usage: Usage,
    #[serde(default)]
    context_input_tokens: i64,
    #[serde(default)]
    sandbox: SandboxWireIn,
}

#[derive(Default, Deserialize)]
struct SandboxWireIn {
    #[serde(default)]
    mode: String,
    #[serde(default)]
    network: String,
    #[serde(default)]
    bash_available: bool,
}

impl SessionWire {
    fn into_info(self) -> crate::app::Info {
        // An unknown string maps to the enum's default, the most restrictive
        // reading ("unavailable", "unconfined").
        let mode = match self.sandbox.mode.as_str() {
            "seatbelt" => SandboxMode::Seatbelt,
            "off" => SandboxMode::Off,
            _ => SandboxMode::default(),
        };
        let network = match self.sandbox.network.as_str() {
            "allowed" => SandboxNetwork::Allowed,
            "denied" => SandboxNetwork::Denied,
            _ => SandboxNetwork::default(),
        };
        crate::app::Info {
            session_id: self.id,
            session_name: self.name,
            workspace: self.workspace,
            provider: self.provider,
            profile: self.profile,
            model: self.model,
            thinking: self.thinking,
            usage_present: self.usage != Usage::default(),
            usage: self.usage,
            context_window: self.context_window,
            context_input_tokens_present: self.context_input_tokens > 0,
            context_input_tokens: self.context_input_tokens,
            sandbox: crate::cli::info::SandboxInfo {
                mode,
                network,
                bash_available: self.sandbox.bash_available,
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

/// The SSE reader both streams share: the response body, the frame parser
/// and the frames parsed from chunks but not yet returned.
struct SseBody {
    response: reqwest::Response,
    /// `None` after the body ended.
    parser: Option<SseParser>,
    ready: VecDeque<String>,
}

impl SseBody {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            parser: Some(SseParser::new(MAX_FRAME_BYTES)),
            ready: VecDeque::new(),
        }
    }

    /// The `data` of the next frame, or `Ok(None)` when serve closed the
    /// stream. Cancel safe: dropping the future between chunks loses no
    /// frame.
    async fn next_data(&mut self) -> Result<Option<String>, Error> {
        loop {
            if let Some(data) = self.ready.pop_front() {
                return Ok(Some(data));
            }
            let Some(parser) = self.parser.as_mut() else {
                return Ok(None);
            };
            match self.response.chunk().await.map_err(unreachable_error)? {
                Some(chunk) => {
                    for frame in parser.feed(&chunk).map_err(unreachable_error)? {
                        self.ready.push_back(frame.data);
                    }
                }
                None => {
                    let trailing = self
                        .parser
                        .take()
                        .and_then(|parser| parser.finish().ok().flatten());
                    if let Some(frame) = trailing {
                        self.ready.push_back(frame.data);
                    }
                }
            }
        }
    }
}

fn decode_frame<T: for<'de> Deserialize<'de>>(data: &str) -> Result<T, Error> {
    serde_json::from_str(data).map_err(|error| unreachable_error(format!("decode frame: {error}")))
}

/// The SSE body of one turn.
pub struct TurnStream {
    pub turn_id: String,
    body: SseBody,
}

impl TurnStream {
    fn new(turn_id: String, response: reqwest::Response) -> Self {
        Self {
            turn_id,
            body: SseBody::new(response),
        }
    }

    /// The next frame, or `Ok(None)` when serve closed the stream. Cancel
    /// safe: dropping the future between chunks loses no frame.
    pub async fn next(&mut self) -> Result<Option<WireEvent>, Error> {
        match self.body.next_data().await? {
            Some(data) => decode_frame(&data).map(Some),
            None => Ok(None),
        }
    }
}

/// One row of a `GET /v1/status` snapshot (server: `StatusSessionWire`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct StatusRow {
    pub id: String,
    #[serde(default)]
    pub workspace: String,
    /// queued, running, ok, error or canceled; `None` before the first turn.
    #[serde(default)]
    pub turn: Option<String>,
    /// The running turn, else the newest one.
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub queued: usize,
    #[serde(default)]
    pub approvals: usize,
    #[serde(default)]
    pub tasks: usize,
}

/// The SSE body of `GET /v1/status`.
pub struct StatusStream {
    body: SseBody,
}

impl StatusStream {
    /// The next snapshot's rows, or `Ok(None)` when serve closed the stream.
    /// Cancel safe like [`TurnStream::next`].
    pub async fn next(&mut self) -> Result<Option<Vec<StatusRow>>, Error> {
        #[derive(Deserialize)]
        struct Snapshot {
            sessions: Vec<StatusRow>,
        }
        match self.body.next_data().await? {
            Some(data) => decode_frame::<Snapshot>(&data).map(|s| Some(s.sessions)),
            None => Ok(None),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
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

    #[tokio::test]
    async fn a_missing_socket_is_unreachable_not_an_http_error() {
        let dir = tempfile::tempdir().expect("dir");
        let client = Client::new(&dir.path().join("absent.sock")).expect("client");
        let error = client.healthz().await.expect_err("no server");
        assert!(matches!(error, Error::Unreachable(_)), "{error}");
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
        assert_eq!(
            request(&seen, 0),
            (
                "POST /v1/sessions/s1/turns".into(),
                json!({
                    "text": "look",
                    "queue": true,
                    "image": { "data": "QUJD", "mime_type": "image/png" }
                })
            )
        );
        assert_eq!(
            request(&seen, 1).1,
            json!({ "text": "plain", "queue": true })
        );
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
            (900, 100)
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
    async fn context_decodes_the_report() {
        let report = ContextReport {
            model: "m".into(),
            context_window: 1000,
            compaction_threshold: 800,
            estimated_total: 120,
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
}
