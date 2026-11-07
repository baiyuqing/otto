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
//!   shape). The former carries the failure and its causes, so a missing
//!   socket reads as `... (http://otto/healthz): client error (Connect): No
//!   such file or directory (os error 2)` instead of a bare `reqwest`
//!   request error.
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

use crate::app::{ActiveInputAdmission, Channel};
use crate::mcp::sse::SseParser;
use crate::runtime::info::{SandboxMode, SandboxNetwork};
use crate::schedule::Origin;
use crate::subagent::record::{ListQuery, ListResult, TaskRow};

mod approvals;
mod session_operations;
mod sessions;
mod tasks;

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

/// The server-side detail for one recorded sub-agent task.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TaskDetail {
    pub task: TaskRow,
    #[serde(default)]
    pub history: Vec<Message>,
    #[serde(default)]
    pub transcript_missing: bool,
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

/// [`Error::Unreachable`] for a failure. `reqwest`'s `Display` names only the
/// request, so an absent socket or a refused connection is named only by the
/// chain, and `otto acp --attach` has to print it to explain the failure.
fn unreachable_error(error: impl std::error::Error + 'static) -> Error {
    Error::Unreachable(error_chain(&error))
}

/// [`unreachable_error`] for a literal or already-formatted message.
fn unreachable_text(error: impl fmt::Display) -> Error {
    Error::Unreachable(error.to_string())
}

/// Joins an error with its causes, so a transport failure names the reason
/// instead of hiding it behind `reqwest`'s generic "error sending request".
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
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

#[derive(Debug, Deserialize)]
pub struct WakeReply {
    pub id: u64,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct WakeReplies {
    pub notices: Vec<WakeReply>,
    pub last: u64,
}

pub struct Client {
    http: reqwest::Client,
    router: Option<axum::Router>,
}

impl Client {
    #[cfg(unix)]
    pub fn new(socket: &Path) -> Result<Self, Error> {
        let http = crate::tls_provider::http_client_builder()
            .unix_socket(socket)
            .no_proxy()
            .build()
            .map_err(unreachable_error)?;
        Ok(Self { http, router: None })
    }

    /// Dispatch against the same serve router without a socket or extra process.
    pub fn in_process(router: axum::Router) -> Self {
        Self {
            http: crate::tls_provider::http_client(),
            router: Some(router),
        }
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
        let response = if let Some(router) = &self.router {
            use tower::ServiceExt;
            let request = request.build().map_err(unreachable_error)?;
            let mut local = axum::http::Request::builder()
                .method(request.method().clone())
                .uri(request.url().as_str());
            *local.headers_mut().expect("request headers") = request.headers().clone();
            let body = match request.body() {
                None => Vec::new(),
                Some(body) => body
                    .as_bytes()
                    .ok_or_else(|| {
                        Error::Unreachable("in-process request body must be buffered".into())
                    })?
                    .to_vec(),
            };
            let response = router
                .clone()
                .oneshot(
                    local
                        .body(axum::body::Body::from(body))
                        .map_err(unreachable_error)?,
                )
                .await
                .map_err(unreachable_error)?;
            let (parts, body) = response.into_parts();
            reqwest::Response::from(axum::http::Response::from_parts(
                parts,
                reqwest::Body::wrap_stream(body.into_data_stream()),
            ))
        } else {
            request.send().await.map_err(unreachable_error)?
        };
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

/// Escapes every byte of one URL path segment except unreserved characters.
fn percent_encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

async fn decode<T: for<'de> Deserialize<'de>>(response: reqwest::Response) -> Result<T, Error> {
    let body = response.bytes().await.map_err(unreachable_error)?;
    serde_json::from_slice(&body)
        .map_err(|error| unreachable_text(format!("decode response: {error}")))
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
    provider_type: String,
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
    /// True when the session decides requests automatically, in AUTO or YOLO.
    #[serde(default)]
    auto_mode: bool,
    /// The exact policy; a server older than the field sends only
    /// `auto_mode`, which maps to AUTO or MANUAL.
    #[serde(default)]
    approval_mode: Option<otto_core::approval::ApprovalMode>,
    #[serde(default)]
    approval_settings: Option<otto_core::approval::ApprovalSettings>,
    #[serde(default)]
    forked_from: Option<crate::app::ForkedFrom>,
    #[serde(default)]
    plan: Vec<otto_core::session::PlanStep>,
    #[serde(default)]
    local_commands: Vec<otto_core::session::local_command::LocalCommandReceipt>,
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
            approval_settings: self.approval_settings.unwrap_or_else(|| {
                otto_core::approval::ApprovalSettings::from_mode(self.approval_mode.unwrap_or(
                    if self.auto_mode {
                        otto_core::approval::ApprovalMode::Auto
                    } else {
                        otto_core::approval::ApprovalMode::Manual
                    },
                ))
            }),
            approval_mode: self.approval_mode.unwrap_or(if self.auto_mode {
                otto_core::approval::ApprovalMode::Auto
            } else {
                otto_core::approval::ApprovalMode::Manual
            }),
            forked_from: self.forked_from,
            plan: self.plan,
            local_commands: self.local_commands,
            session_id: self.id,
            session_name: self.name,
            workspace: self.workspace,
            model_selector: format!("{}/{}", self.provider, self.model),
            provider: self.provider,
            provider_type: self.provider_type,
            model: self.model,
            thinking: self.thinking,
            usage_present: self.usage != Usage::default(),
            usage: self.usage,
            context_window: self.context_window,
            context_input_tokens_present: self.context_input_tokens > 0,
            context_input_tokens: self.context_input_tokens,
            sandbox: crate::runtime::info::SandboxInfo {
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
                    for frame in parser.feed(&chunk).map_err(unreachable_text)? {
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
    serde_json::from_str(data).map_err(|error| unreachable_text(format!("decode frame: {error}")))
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
    /// True when the session decides requests automatically, in AUTO or YOLO.
    #[serde(default)]
    pub auto_mode: bool,
    /// The exact policy; absent from a server older than the field.
    #[serde(default)]
    pub approval_mode: Option<otto_core::approval::ApprovalMode>,
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
mod tests;
