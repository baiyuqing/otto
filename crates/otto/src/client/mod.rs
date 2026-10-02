//! An HTTP client of `otto serve` over its Unix socket.
//!
//! Used by `otto acp --attach`; the calls are the ones that relay needs.
//! Contract:
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
//! - No call retries and no call has a timeout: a turn stream stays open for
//!   as long as the turn runs.

use std::collections::VecDeque;
use std::fmt;
use std::path::Path;

use otto_core::model::Message;
use otto_core::wire::events::WireEvent;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::mcp::sse::SseParser;

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
    /// the stored session of that id. Returns the session id.
    pub async fn open_session(
        &self,
        workspace: &str,
        resume: Option<&str>,
    ) -> Result<String, Error> {
        let mut body = json!({ "workspace": workspace });
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

    /// `GET /v1/sessions/{id}/history`.
    pub async fn history(&self, session_id: &str) -> Result<Vec<Message>, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/history")),
            )
            .await?;
        decode(response).await
    }

    /// `POST /v1/sessions/{id}/turns` with `queue: true`.
    pub async fn start_turn(&self, session_id: &str, text: &str) -> Result<TurnStream, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/turns"),
                &json!({ "text": text, "queue": true }),
            ))
            .await?;
        let turn_id = response
            .headers()
            .get(TURN_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| unreachable_error("response has no Otto-Turn-Id header"))?;
        Ok(TurnStream {
            turn_id,
            response,
            parser: Some(SseParser::new(MAX_FRAME_BYTES)),
            ready: VecDeque::new(),
        })
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

/// The SSE body of one turn.
pub struct TurnStream {
    pub turn_id: String,
    response: reqwest::Response,
    /// `None` after the body ended.
    parser: Option<SseParser>,
    ready: VecDeque<WireEvent>,
}

impl TurnStream {
    /// The next frame, or `Ok(None)` when serve closed the stream. Cancel
    /// safe: dropping the future between chunks loses no frame.
    pub async fn next(&mut self) -> Result<Option<WireEvent>, Error> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(Some(event));
            }
            let Some(parser) = self.parser.as_mut() else {
                return Ok(None);
            };
            match self.response.chunk().await.map_err(unreachable_error)? {
                Some(chunk) => {
                    for frame in parser.feed(&chunk).map_err(unreachable_error)? {
                        let event = serde_json::from_str(&frame.data)
                            .map_err(|error| unreachable_error(format!("decode frame: {error}")))?;
                        self.ready.push_back(event);
                    }
                }
                None => {
                    let trailing = self
                        .parser
                        .take()
                        .and_then(|parser| parser.finish().ok().flatten());
                    if let Some(frame) = trailing {
                        let event = serde_json::from_str(&frame.data)
                            .map_err(|error| unreachable_error(format!("decode frame: {error}")))?;
                        self.ready.push_back(event);
                    }
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
}
