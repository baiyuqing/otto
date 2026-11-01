//! Streamable HTTP transport for MCP servers.
//!
//! Owned by the HTTP transport step; see `docs/specs/2026-09-19-mcp-design.md`
//! ("Transports > Streamable HTTP"). One `HttpTransport` serializes every
//! `request`/`notify` call as a single POST to the configured URL, adds the
//! protocol headers for the negotiated [`Era`], and reads either a JSON body
//! or a `text/event-stream` body for the matching response. Concurrency:
//! `request` and `notify` take `&self` and may run concurrently; each call
//! allocates its own JSON-RPC id and opens its own HTTP request, so no
//! internal lock serializes them except the single-slot legacy session id.
//! Redirects are never followed (`reqwest::redirect::Policy::none()`).

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use futures_util::StreamExt;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue, WWW_AUTHENTICATE,
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::mcp::jsonrpc::{self, Incoming, Notification, Request, RpcError};
use crate::mcp::sse::SseParser;
use crate::mcp::{BearerSource, CallError, Era, MODERN_VERSION, Outbound, Transport};

/// SSE event size cap: matches the stdio transport's frame cap.
const MAX_SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// Cap on a whole HTTP response body (JSON reply, 400 error body, or total
/// bytes across an SSE stream): matches the stdio transport's line cap.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// The exact message used when a response body exceeds its cap, so callers
/// can distinguish it from other body-read failures without parsing prose.
const BODY_TOO_LARGE: &str = "response body exceeds 16 MiB";

/// One HTTP-based MCP server connection.
pub struct HttpTransport {
    client: Client,
    url: String,
    headers: Vec<(String, String)>,
    bearer: Option<Arc<dyn BearerSource>>,
    /// Captured from a legacy server's `Mcp-Session-Id` response header and
    /// replayed on every later request to the same server.
    session_id: Mutex<Option<String>>,
    next_id: AtomicI64,
    /// Cap on a whole response body; [`MAX_BODY_BYTES`] outside tests, or a
    /// smaller value a test installs to exercise the cap without sending a
    /// real 16 MiB body.
    max_body_bytes: usize,
}

impl HttpTransport {
    /// `headers` are the configured static headers (already expanded).
    /// `bearer` is present for `auth = "oauth"` servers.
    pub fn new(
        url: String,
        headers: Vec<(String, String)>,
        bearer: Option<Arc<dyn BearerSource>>,
        request_timeout: Duration,
    ) -> Result<Self, CallError> {
        let client = crate::tls_provider::http_client_builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(request_timeout)
            .build()
            .map_err(|_| CallError::Transport("build http client".to_string()))?;
        Ok(Self {
            client,
            url,
            headers,
            bearer,
            session_id: Mutex::new(None),
            next_id: AtomicI64::new(1),
            max_body_bytes: MAX_BODY_BYTES,
        })
    }

    /// Installs a smaller body cap for a test, so it can exercise
    /// [`BODY_TOO_LARGE`] without sending a real [`MAX_BODY_BYTES`] body.
    #[cfg(test)]
    fn with_max_body_bytes(mut self, max: usize) -> Self {
        self.max_body_bytes = max;
        self
    }

    fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The `Mcp-Protocol-Version`/`Mcp-Method`/`Mcp-Name`/`Mcp-Session-Id`
    /// headers for one outbound message. A probe (`era: None`) is sent with
    /// modern headers, except the legacy handshake's `initialize`: it offers
    /// its version in the body, and a modern `Mcp-Protocol-Version` header
    /// contradicts that body, which a server rejects with `-32020`.
    fn protocol_headers(
        &self,
        era: &Option<Era>,
        method: &str,
        params: &Value,
    ) -> Vec<(HeaderName, HeaderValue)> {
        let mut headers = Vec::new();
        match era {
            Some(Era::Legacy(version)) => {
                if let Ok(value) = HeaderValue::from_str(version) {
                    headers.push((HeaderName::from_static("mcp-protocol-version"), value));
                }
                let session_id = self.session_id.lock().expect("mcp session id lock").clone();
                if let Some(id) = session_id
                    && let Ok(value) = HeaderValue::from_str(&id)
                {
                    headers.push((HeaderName::from_static("mcp-session-id"), value));
                }
            }
            Some(Era::Modern) | None => {
                if method == "initialize" {
                    return headers;
                }
                headers.push((
                    HeaderName::from_static("mcp-protocol-version"),
                    HeaderValue::from_static(MODERN_VERSION),
                ));
                if let Ok(value) = HeaderValue::from_str(method) {
                    headers.push((HeaderName::from_static("mcp-method"), value));
                }
                if method == "tools/call"
                    && let Some(name) = params.get("name").and_then(Value::as_str)
                {
                    headers.push((HeaderName::from_static("mcp-name"), tool_name_header(name)));
                }
            }
        }
        headers
    }

    /// Sends one POST. `id: None` builds a notification. Captures a legacy
    /// session id from the response, whatever the status.
    async fn send_once(
        &self,
        id: Option<i64>,
        outbound: &Outbound,
        token: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, CallError> {
        let body = match id {
            Some(id) => {
                serde_json::to_vec(&Request::new(id, &outbound.method, outbound.params.clone()))
            }
            None => serde_json::to_vec(&Notification::new(
                &outbound.method,
                outbound.params.clone(),
            )),
        }
        .map_err(|e| CallError::Transport(format!("serialize request: {e}")))?;

        let mut builder = self
            .client
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream");
        for (name, value) in &self.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                builder = builder.header(name, value);
            }
        }
        for (name, value) in
            self.protocol_headers(&outbound.era, &outbound.method, &outbound.params)
        {
            builder = builder.header(name, value);
        }
        if let Some(token) = token {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }

        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CallError::Cancelled),
            result = builder.body(body).send() => result.map_err(classify_reqwest_error)?,
        };

        if let Some(session_id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().expect("mcp session id lock") = Some(session_id.to_string());
        }

        Ok(response)
    }

    /// Fetches the bearer token (if any), sends the request, and retries once
    /// on a 401 after refreshing.
    async fn send_with_auth(
        &self,
        id: Option<i64>,
        outbound: &Outbound,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, CallError> {
        let token = match &self.bearer {
            Some(source) => Some(source.bearer(cancel).await?),
            None => None,
        };
        let mut response = self
            .send_once(id, outbound, token.as_deref(), cancel)
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            match &self.bearer {
                Some(source) => {
                    let rejected = token
                        .as_deref()
                        .expect("token is set whenever self.bearer is Some");
                    let refreshed = source.refresh(rejected, cancel).await?;
                    response = self
                        .send_once(id, outbound, Some(&refreshed), cancel)
                        .await?;
                    if response.status() == StatusCode::UNAUTHORIZED {
                        return Err(CallError::NeedsLogin);
                    }
                }
                None => return Err(CallError::Transport("401 unauthorized".to_string())),
            }
        }
        Ok(response)
    }

    async fn handle_response(
        &self,
        response: reqwest::Response,
        expect_id: Value,
        is_probe: bool,
        cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        match response.status() {
            StatusCode::OK => self.handle_ok_body(response, expect_id, cancel).await,
            StatusCode::ACCEPTED => Err(CallError::Transport("202 accepted".to_string())),
            StatusCode::BAD_REQUEST => {
                handle_bad_request(response, is_probe, self.max_body_bytes).await
            }
            StatusCode::FORBIDDEN => Err(handle_forbidden(&response)),
            StatusCode::NOT_FOUND => Err(CallError::Transport("404 session expired".to_string())),
            status if status.is_redirection() => Err(CallError::Transport(format!(
                "{} redirect not followed",
                status.as_u16()
            ))),
            status if status.is_server_error() => Err(CallError::Transport(format!(
                "{} server error",
                status.as_u16()
            ))),
            status => Err(CallError::Transport(format!(
                "unexpected status {}",
                status.as_u16()
            ))),
        }
    }

    async fn handle_ok_body(
        &self,
        response: reqwest::Response,
        expect_id: Value,
        cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if content_type.starts_with("application/json") {
            let body = read_body_capped(response, self.max_body_bytes).await?;
            let text = String::from_utf8_lossy(&body);
            match jsonrpc::parse_incoming(&text) {
                Ok(Incoming::Response { result, .. }) => Ok(Ok(result)),
                Ok(Incoming::Error { error, .. }) => Ok(Err(error)),
                _ => Err(CallError::Transport("unexpected message type".to_string())),
            }
        } else if content_type.starts_with("text/event-stream") {
            read_sse(response, expect_id, cancel, self.max_body_bytes).await
        } else {
            Err(CallError::Transport("unexpected content type".to_string()))
        }
    }
}

/// Builds the `Mcp-Name` header value: the tool name verbatim when it is a
/// valid header value, otherwise the base64 sentinel form.
fn tool_name_header(name: &str) -> HeaderValue {
    HeaderValue::from_str(name).unwrap_or_else(|_| {
        let encoded = base64::engine::general_purpose::STANDARD.encode(name);
        HeaderValue::from_str(&format!("=?base64?{encoded}?="))
            .expect("base64 alphabet is a valid header value")
    })
}

fn classify_reqwest_error(error: reqwest::Error) -> CallError {
    if error.is_timeout() {
        return CallError::Timeout;
    }
    let kind = if error.is_connect() {
        "connection failed"
    } else if error.is_body() || error.is_decode() {
        "response body error"
    } else if error.is_request() {
        "request error"
    } else {
        "http error"
    };
    CallError::Transport(kind.to_string())
}

/// Reads a whole response body, erroring once the running total exceeds
/// `max_body_bytes` instead of buffering an unbounded amount of memory.
async fn read_body_capped(
    response: reqwest::Response,
    max_body_bytes: usize,
) -> Result<Vec<u8>, CallError> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| CallError::Transport("response body error".to_string()))?;
        body.extend_from_slice(&chunk);
        if body.len() > max_body_bytes {
            return Err(CallError::Transport(BODY_TOO_LARGE.to_string()));
        }
    }
    Ok(body)
}

async fn read_sse(
    response: reqwest::Response,
    expect_id: Value,
    cancel: &CancellationToken,
    max_body_bytes: usize,
) -> Result<Result<Value, RpcError>, CallError> {
    let mut stream = response.bytes_stream();
    let mut parser = SseParser::new(MAX_SSE_EVENT_BYTES);
    let mut total_bytes = 0usize;
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CallError::Cancelled),
            next = stream.next() => next,
        };
        match next {
            Some(Ok(bytes)) => {
                total_bytes += bytes.len();
                if total_bytes > max_body_bytes {
                    return Err(CallError::Transport(BODY_TOO_LARGE.to_string()));
                }
                let events = parser
                    .feed(&bytes)
                    .map_err(|_| CallError::Transport("malformed sse stream".to_string()))?;
                for event in events {
                    if let Ok(incoming) = jsonrpc::parse_incoming(&event.data) {
                        match incoming {
                            Incoming::Response { id, result } if id == expect_id => {
                                return Ok(Ok(result));
                            }
                            Incoming::Error { id, error } if id == expect_id => {
                                return Ok(Err(error));
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some(Err(_)) => return Err(CallError::Transport("response body error".to_string())),
            None => {
                return Err(CallError::Transport(
                    "stream ended without a response".to_string(),
                ));
            }
        }
    }
}

/// A 400 response: during the probe, or when the body carries
/// `UNSUPPORTED_PROTOCOL_VERSION`/`INVALID_PARAMS`, this surfaces as an
/// `Ok(Err(..))` RPC error so the client's legacy fallback runs.
async fn handle_bad_request(
    response: reqwest::Response,
    is_probe: bool,
    max_body_bytes: usize,
) -> Result<Result<Value, RpcError>, CallError> {
    let body = match read_body_capped(response, max_body_bytes).await {
        Ok(body) => body,
        Err(CallError::Transport(message)) if message == BODY_TOO_LARGE => {
            return Err(CallError::Transport(message));
        }
        // A non-cap read failure (e.g. a dropped connection) here just
        // means the 400's own body is unavailable; the status code alone
        // still carries the case handled below.
        Err(_) => Vec::new(),
    };
    let text = String::from_utf8_lossy(&body);
    let parsed_error = match jsonrpc::parse_incoming(&text) {
        Ok(Incoming::Error { error, .. }) => Some(error),
        _ => None,
    };
    match parsed_error {
        Some(error)
            if is_probe
                || error.code == jsonrpc::UNSUPPORTED_PROTOCOL_VERSION
                || error.code == jsonrpc::INVALID_PARAMS =>
        {
            Ok(Err(error))
        }
        None if is_probe => Ok(Err(RpcError {
            code: jsonrpc::INVALID_PARAMS,
            message: "400 bad request".to_string(),
            data: None,
        })),
        _ => Err(CallError::Transport("400 bad request".to_string())),
    }
}

fn handle_forbidden(response: &reqwest::Response) -> CallError {
    let header = response
        .headers()
        .get(WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if header.contains("insufficient_scope") {
        match header_param(header, "scope") {
            Some(scope) => CallError::Transport(format!("403 insufficient scope: {scope}")),
            None => CallError::Transport("403 insufficient scope".to_string()),
        }
    } else {
        CallError::Transport("403 forbidden".to_string())
    }
}

/// Extracts `key="value"` from a header value such as
/// `Bearer error="insufficient_scope", scope="mcp:tools"`.
fn header_param(header: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = header.find(&needle)? + needle.len();
    let end = header[start..].find('"')?;
    Some(header[start..start + end].to_string())
}

#[async_trait]
impl Transport for HttpTransport {
    async fn request(
        &self,
        outbound: Outbound,
        cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        let id = self.next_id();
        let expect_id = json!(id);
        let is_probe = outbound.era.is_none();
        let response = self.send_with_auth(Some(id), &outbound, cancel).await?;
        self.handle_response(response, expect_id, is_probe, cancel)
            .await
    }

    async fn notify(&self, outbound: Outbound) -> Result<(), CallError> {
        let cancel = CancellationToken::new();
        let response = self.send_with_auth(None, &outbound, &cancel).await?;
        match response.status() {
            StatusCode::ACCEPTED | StatusCode::OK | StatusCode::NO_CONTENT => Ok(()),
            StatusCode::FORBIDDEN => Err(handle_forbidden(&response)),
            status if status.is_redirection() => Err(CallError::Transport(format!(
                "{} redirect not followed",
                status.as_u16()
            ))),
            status if status.is_server_error() => Err(CallError::Transport(format!(
                "{} server error",
                status.as_u16()
            ))),
            status => Err(CallError::Transport(format!(
                "unexpected status {}",
                status.as_u16()
            ))),
        }
    }

    async fn close(&self) {}
}

#[cfg(test)]
mod tests;
