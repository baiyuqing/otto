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
        let client = Client::builder()
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
        })
    }

    fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The `Mcp-Protocol-Version`/`Mcp-Method`/`Mcp-Name`/`Mcp-Session-Id`
    /// headers for one outbound message. A probe (`era: None`) is sent with
    /// modern headers.
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
                    let refreshed = source.refresh(cancel).await?;
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
            StatusCode::BAD_REQUEST => handle_bad_request(response, is_probe).await,
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
            let text = response
                .text()
                .await
                .map_err(|_| CallError::Transport("response body error".to_string()))?;
            match jsonrpc::parse_incoming(&text) {
                Ok(Incoming::Response { result, .. }) => Ok(Ok(result)),
                Ok(Incoming::Error { error, .. }) => Ok(Err(error)),
                _ => Err(CallError::Transport("unexpected message type".to_string())),
            }
        } else if content_type.starts_with("text/event-stream") {
            read_sse(response, expect_id, cancel).await
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

async fn read_sse(
    response: reqwest::Response,
    expect_id: Value,
    cancel: &CancellationToken,
) -> Result<Result<Value, RpcError>, CallError> {
    let mut stream = response.bytes_stream();
    let mut parser = SseParser::new(MAX_SSE_EVENT_BYTES);
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CallError::Cancelled),
            next = stream.next() => next,
        };
        match next {
            Some(Ok(bytes)) => {
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
) -> Result<Result<Value, RpcError>, CallError> {
    let text = response.text().await.unwrap_or_default();
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
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::auth::testserver;

    struct FakeBearer {
        bearer_result: Mutex<Result<String, CallError>>,
        refresh_result: Mutex<Result<String, CallError>>,
        refresh_calls: Mutex<u32>,
    }

    impl FakeBearer {
        fn ok(token: &str) -> Self {
            Self {
                bearer_result: Mutex::new(Ok(token.to_string())),
                refresh_result: Mutex::new(Ok(token.to_string())),
                refresh_calls: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl BearerSource for FakeBearer {
        async fn bearer(&self, _cancel: &CancellationToken) -> Result<String, CallError> {
            self.bearer_result.lock().unwrap().clone()
        }

        async fn refresh(&self, _cancel: &CancellationToken) -> Result<String, CallError> {
            *self.refresh_calls.lock().unwrap() += 1;
            self.refresh_result.lock().unwrap().clone()
        }

        fn secrets(&self) -> Vec<String> {
            vec![]
        }
    }

    fn modern_outbound(method: &str, params: Value) -> Outbound {
        Outbound {
            method: method.to_string(),
            params,
            era: Some(Era::Modern),
        }
    }

    #[tokio::test]
    async fn modern_headers_and_static_headers_sent() {
        let server = testserver::spawn(|req| {
            assert_eq!(req.header("mcp-protocol-version"), MODERN_VERSION);
            assert_eq!(req.header("mcp-method"), "tools/call");
            assert_eq!(req.header("mcp-name"), "my_tool");
            assert_eq!(req.header("x-custom"), "value");
            testserver::json_response(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
        })
        .await;
        let transport = HttpTransport::new(
            server.url.clone(),
            vec![("X-Custom".to_string(), "value".to_string())],
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        let outbound = modern_outbound("tools/call", json!({"name": "my_tool", "arguments": {}}));
        let cancel = CancellationToken::new();
        let result = transport.request(outbound, &cancel).await.unwrap();
        assert_eq!(result.unwrap(), json!({}));
    }

    #[tokio::test]
    async fn tool_name_with_control_char_uses_base64_header() {
        let expected = base64::engine::general_purpose::STANDARD.encode("tool\nname");
        let server = testserver::spawn(move |req| {
            assert_eq!(req.header("mcp-name"), format!("=?base64?{expected}?="));
            testserver::json_response(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
        })
        .await;
        let transport =
            HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
        let outbound =
            modern_outbound("tools/call", json!({"name": "tool\nname", "arguments": {}}));
        let cancel = CancellationToken::new();
        transport.request(outbound, &cancel).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn sse_response_skips_keepalive_and_unrelated_events() {
        let server = testserver::spawn(|_req| {
            testserver::sse_response(
                ": keep-alive\n\nevent: other\ndata: {\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{}}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
            )
        })
        .await;
        let transport =
            HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
        let outbound = modern_outbound("tools/list", json!({}));
        let cancel = CancellationToken::new();
        let result = transport.request(outbound, &cancel).await.unwrap();
        assert_eq!(result.unwrap(), json!({"ok": true}));
    }

    #[tokio::test]
    async fn legacy_session_id_captured_and_replayed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let server = testserver::spawn(move |req| {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                assert_eq!(req.header("mcp-session-id"), "");
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: sess-123\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                assert_eq!(req.header("mcp-session-id"), "sess-123");
                testserver::json_response(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#)
            }
        })
        .await;
        let transport =
            HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
        let cancel = CancellationToken::new();
        let outbound = Outbound {
            method: "tools/list".to_string(),
            params: json!({}),
            era: Some(Era::Legacy("2025-11-25".to_string())),
        };
        transport
            .request(outbound.clone(), &cancel)
            .await
            .unwrap()
            .unwrap();
        transport.request(outbound, &cancel).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn probe_400_with_unsupported_version_falls_back() {
        let server = testserver::spawn(|_req| {
            testserver::status_response(
                400,
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32020,"message":"unsupported","data":{"supported":["2025-11-25"]}}}"#,
            )
        })
        .await;
        let transport =
            HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
        let outbound = Outbound {
            method: "tools/list".to_string(),
            params: json!({}),
            era: None,
        };
        let cancel = CancellationToken::new();
        let result = transport.request(outbound, &cancel).await.unwrap();
        let err = result.unwrap_err();
        assert_eq!(err.code, jsonrpc::UNSUPPORTED_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn probe_400_without_body_falls_back_to_sentinel() {
        let server = testserver::spawn(|_req| testserver::status_response(400, "")).await;
        let transport =
            HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
        let outbound = Outbound {
            method: "tools/list".to_string(),
            params: json!({}),
            era: None,
        };
        let cancel = CancellationToken::new();
        let result = transport.request(outbound, &cancel).await.unwrap();
        let err = result.unwrap_err();
        assert_eq!(err.code, jsonrpc::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn bearer_token_sent_when_configured() {
        let server = testserver::spawn(|req| {
            assert_eq!(req.header("authorization"), "Bearer tok-abc");
            testserver::json_response(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
        })
        .await;
        let bearer = Arc::new(FakeBearer::ok("tok-abc"));
        let transport = HttpTransport::new(
            server.url.clone(),
            vec![],
            Some(bearer),
            Duration::from_secs(5),
        )
        .unwrap();
        let outbound = modern_outbound("tools/list", json!({}));
        let cancel = CancellationToken::new();
        transport.request(outbound, &cancel).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unauthorized_then_refresh_then_retry_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let server = testserver::spawn(move |req| {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                testserver::status_response(401, "")
            } else {
                assert_eq!(req.header("authorization"), "Bearer new-token");
                testserver::json_response(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#)
            }
        })
        .await;
        let bearer = Arc::new(FakeBearer {
            bearer_result: Mutex::new(Ok("old-token".to_string())),
            refresh_result: Mutex::new(Ok("new-token".to_string())),
            refresh_calls: Mutex::new(0),
        });
        let transport = HttpTransport::new(
            server.url.clone(),
            vec![],
            Some(Arc::clone(&bearer) as Arc<dyn BearerSource>),
            Duration::from_secs(5),
        )
        .unwrap();
        let outbound = modern_outbound("tools/list", json!({}));
        let cancel = CancellationToken::new();
        let result = transport.request(outbound, &cancel).await.unwrap();
        assert_eq!(result.unwrap(), json!({"ok": true}));
        assert_eq!(*bearer.refresh_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn unauthorized_then_refresh_fails_needs_login() {
        let server = testserver::spawn(|_req| testserver::status_response(401, "")).await;
        let bearer = Arc::new(FakeBearer {
            bearer_result: Mutex::new(Ok("old".to_string())),
            refresh_result: Mutex::new(Err(CallError::NeedsLogin)),
            refresh_calls: Mutex::new(0),
        });
        let transport = HttpTransport::new(
            server.url.clone(),
            vec![],
            Some(bearer),
            Duration::from_secs(5),
        )
        .unwrap();
        let outbound = modern_outbound("tools/list", json!({}));
        let cancel = CancellationToken::new();
        let err = transport.request(outbound, &cancel).await.unwrap_err();
        assert_eq!(err, CallError::NeedsLogin);
    }

    #[tokio::test]
    async fn unauthorized_without_bearer_is_transport_error() {
        let server = testserver::spawn(|_req| testserver::status_response(401, "")).await;
        let transport =
            HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
        let outbound = modern_outbound("tools/list", json!({}));
        let cancel = CancellationToken::new();
        let err = transport.request(outbound, &cancel).await.unwrap_err();
        match err {
            CallError::Transport(msg) => assert!(msg.contains("401")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forbidden_insufficient_scope_reports_scope() {
        let server = testserver::spawn(|_req| {
            "HTTP/1.1 403 X\r\nWWW-Authenticate: Bearer error=\"insufficient_scope\", scope=\"mcp:tools\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string()
        })
        .await;
        let transport =
            HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
        let outbound = modern_outbound("tools/list", json!({}));
        let cancel = CancellationToken::new();
        let err = transport.request(outbound, &cancel).await.unwrap_err();
        match err {
            CallError::Transport(msg) => assert!(msg.contains("mcp:tools")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn notify_accepts_202() {
        let server = testserver::spawn(|_req| testserver::status_response(202, "")).await;
        let transport =
            HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
        let outbound = modern_outbound("notifications/initialized", Value::Null);
        transport.notify(outbound).await.unwrap();
    }

    #[tokio::test]
    async fn server_error_is_transport() {
        let server = testserver::spawn(|_req| testserver::status_response(500, "")).await;
        let transport =
            HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
        let outbound = modern_outbound("tools/list", json!({}));
        let cancel = CancellationToken::new();
        let err = transport.request(outbound, &cancel).await.unwrap_err();
        match err {
            CallError::Transport(msg) => assert!(msg.contains("500")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_during_send_returns_cancelled() {
        let url = testserver::spawn_hanging().await;
        let transport = HttpTransport::new(url, vec![], None, Duration::from_secs(30)).unwrap();
        let outbound = modern_outbound("tools/list", json!({}));
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });
        let err = transport.request(outbound, &cancel).await.unwrap_err();
        assert_eq!(err, CallError::Cancelled);
    }

    #[tokio::test]
    async fn redirect_not_followed_is_transport_error() {
        let server = testserver::spawn(|_req| {
            testserver::redirect_response(302, "http://127.0.0.1:1/somewhere")
        })
        .await;
        let transport =
            HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
        let outbound = modern_outbound("tools/list", json!({}));
        let cancel = CancellationToken::new();
        let err = transport.request(outbound, &cancel).await.unwrap_err();
        match err {
            CallError::Transport(msg) => assert!(msg.contains("302")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }
}
