//! HTTP transport for the OpenAI-compatible Chat Completions API.
//!
//! The wire codec lives in [`otto_core::openaicompat`]; this module owns only
//! the parts that need the network: base-URL validation, connection settings,
//! the single-attempt policy, bounded error-body reader, and API-key redaction.
//!
//! Ownership: a [`Client`] owns its base URL, its API key, and its
//! [`reqwest::Client`]. The request passed to `complete` is borrowed and never
//! retained; the returned response belongs to the caller.
//!
//! Concurrency: `complete` takes `&self` and holds no mutable state, so one
//! client can serve a parent agent and its sub-agents at the same time.
//! `reqwest::Client` shares its connection pool across those calls.
//!
//! Operation control: every network await observes the caller's cancellation
//! token. Stop reasons are checked before and after partial work; deadline and
//! user cancellation remain distinct. A fully received and validated response
//! wins a simultaneous stop.
//!
//! Errors: every failure is a [`ProviderError`]. Text of an
//! [`ProviderError::Other`] is passed through API-key redaction before it
//! leaves this module. [`ProviderError::Overflow`] carries only a status, an
//! allowlisted code, and two token counts, never provider text.

use std::future::Future;
use std::time::Duration;

use futures_util::TryStreamExt;
use tokio_util::sync::CancellationToken;

use otto_core::model::OperationStopReason;
use otto_core::openaicompat::overflow::{MAX_ERROR_BODY, classify_overflow};
use otto_core::openaicompat::protocol::{build_request, serialized_request_size};
use otto_core::openaicompat::stream::StreamAssembler;
use otto_core::operation::OperationControl;
use otto_core::provider::{Provider, ProviderError, Request, RequestSizer, Response, StreamSink};

use crate::gourl::{self, Encoding};

/// Longest wait for the TCP connect and TLS handshake of one attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP keepalive interval.
const KEEPALIVE: Duration = Duration::from_secs(30);
/// Longest wait for a single read from the socket.
///
/// reqwest has no header-only timeout, so the bound is applied per read
/// instead. It therefore also bounds a stream that stalls mid-body. A streaming
/// chat completion sends something well inside 60 seconds, and there is
/// deliberately no overall request timeout, because a long completion may
/// legitimately take minutes.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest number of redirect hops followed before the policy stops.
const MAX_REDIRECTS: usize = 3;
/// The text an API key is replaced with when it is long enough to hold it.
const REDACTED: &str = "[REDACTED]";
/// Largest `GET /models` body read. A larger catalog is an error rather than
/// an unbounded buffer.
const MAX_MODELS_BODY: usize = 8 << 20;

/// A usable client: the base URL passed validation and the HTTP client built.
struct Ready {
    base_url: String,
    http: reqwest::Client,
}

/// A client for one OpenAI-compatible endpoint.
///
/// See the module documentation for the ownership, concurrency, cancellation,
/// and error rules.
pub struct Client {
    api_key: String,
    /// `Err` records why the client is unusable. Construction never fails; the
    /// stored message is returned from the first `complete`.
    state: Result<Ready, String>,
}

impl Client {
    /// Builds a client for `base_url` authenticating with `api_key`.
    ///
    /// Never fails. An invalid base URL, or a TLS stack that refuses to
    /// initialize, is recorded and returned as an error from the first
    /// [`Provider::complete`] call.
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let state = match default_http_client() {
            Ok(http) => Self::ready(base_url, http),
            Err(error) => Err(format!("build OpenAI-compatible HTTP client: {error}")),
        };
        Self {
            api_key: api_key.to_string(),
            state,
        }
    }

    /// Builds a client on a caller-supplied [`reqwest::Client`].
    ///
    /// The caller owns the timeouts and the redirect policy of `http`; nothing
    /// in [`default_http_client`] is reapplied. Base-URL validation is
    /// unchanged.
    pub fn with_http_client(base_url: &str, api_key: &str, http: reqwest::Client) -> Self {
        Self {
            api_key: api_key.to_string(),
            state: Self::ready(base_url, http),
        }
    }

    fn ready(base_url: &str, http: reqwest::Client) -> Result<Ready, String> {
        match normalize_base_url(base_url) {
            Some(base_url) => Ok(Ready { base_url, http }),
            None => Err("invalid OpenAI-compatible base URL".to_string()),
        }
    }

    /// Lists the model ids the endpoint reports at `GET {base}/models`, sorted
    /// and without duplicates.
    ///
    /// One attempt with no retry: the caller is a tool the model can call
    /// again. The body is read up to [`MAX_MODELS_BODY`] bytes and must be the
    /// OpenAI shape `{"data":[{"id":…}]}`. Error text is redacted the same way
    /// as `complete`'s.
    pub async fn list_models(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<String>, ProviderError> {
        let ready = match &self.state {
            Ok(ready) => ready,
            Err(error) => return Err(ProviderError::Other(error.clone())),
        };
        self.fetch_models(ready, cancel)
            .await
            .map_err(|error| self.safe_error(error))
    }

    async fn fetch_models(
        &self,
        ready: &Ready,
        cancel: &CancellationToken,
    ) -> Result<Vec<String>, ProviderError> {
        let send = ready
            .http
            .get(format!("{}/models", ready.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Accept", "application/json")
            .send();
        let response = match with_cancel(cancel, send).await {
            None => return Err(ProviderError::Cancelled),
            Some(Ok(response)) => response,
            Some(Err(error)) => {
                return Err(ProviderError::Other(format!(
                    "send model list request: {}",
                    error_chain(&error)
                )));
            }
        };
        let status = response.status();
        if !status.is_success() {
            let status = status.as_u16();
            return Err(match read_error_body(response, cancel).await {
                ErrorBody::Stopped(error) => error,
                ErrorBody::Unreadable => ProviderError::Other(format!(
                    "OpenAI-compatible HTTP {status} (error body unreadable)"
                )),
                ErrorBody::Body(body) => ProviderError::Other(format!(
                    "OpenAI-compatible HTTP {status}: {}",
                    String::from_utf8_lossy(&body).trim()
                )),
            });
        }

        let mut body = Box::pin(response.bytes_stream());
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            match with_cancel(cancel, body.try_next()).await {
                None => return Err(ProviderError::Cancelled),
                Some(Ok(Some(chunk))) => bytes.extend_from_slice(&chunk),
                Some(Ok(None)) => break,
                Some(Err(error)) => {
                    return Err(ProviderError::Other(format!(
                        "read model list: {}",
                        error_chain(&error)
                    )));
                }
            }
            if bytes.len() > MAX_MODELS_BODY {
                return Err(ProviderError::Other(format!(
                    "model list is larger than {MAX_MODELS_BODY} bytes"
                )));
            }
        }

        #[derive(serde::Deserialize)]
        struct ModelList {
            data: Vec<Model>,
        }
        #[derive(serde::Deserialize)]
        struct Model {
            id: String,
        }
        let list: ModelList = serde_json::from_slice(&bytes)
            .map_err(|error| ProviderError::Other(format!("decode model list: {error}")))?;
        let mut ids: Vec<String> = list.data.into_iter().map(|model| model.id).collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// One request/response round trip, including reading the whole stream.
    async fn attempt(
        &self,
        ready: &Ready,
        payload: &[u8],
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<Response, ProviderError> {
        check_running(control)?;
        let send = ready
            .http
            .post(format!("{}/chat/completions", ready.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .body(payload.to_vec())
            .send();
        let response = match await_control(control, send).await? {
            Ok(response) => response,
            Err(error) => {
                check_running(control)?;
                return Err(ProviderError::Other(format!(
                    "send chat completion request: {}",
                    error_chain(&error)
                )));
            }
        };

        let status = response.status();
        if !status.is_success() {
            return Err(self
                .error_response(status.as_u16(), response, control)
                .await);
        }

        let mut assembler = StreamAssembler::new();
        let mut body = Box::pin(response.bytes_stream());
        while !assembler.is_done() {
            let chunk = match await_control(control, body.try_next()).await? {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => {
                    check_running(control)?;
                    return Err(ProviderError::Other(format!(
                        "read chat completion stream: {}",
                        error_chain(&error)
                    )));
                }
            };
            {
                if let Err(error) = assembler.push(&chunk, &mut *emit) {
                    check_running(control)?;
                    return Err(ProviderError::Other(error.to_string()));
                }
                // Once a complete response has been received and validated, it
                // wins a simultaneous stop. Before then, the stop reason wins.
                if assembler.is_done() {
                    return match assembler.finish(&mut *emit) {
                        Ok(response) => Ok(response),
                        Err(error) => {
                            check_running(control)?;
                            Err(ProviderError::Other(error.to_string()))
                        }
                    };
                }
                check_running(control)?;
            }
        }
        let result = assembler
            .finish(&mut *emit)
            .map_err(|error| ProviderError::Other(error.to_string()));
        match result {
            Ok(response) => Ok(response),
            Err(error) => {
                check_running(control)?;
                Err(error)
            }
        }
    }

    /// Turns a non-2xx response into a failure, reading a bounded prefix of
    /// the body so a hostile endpoint cannot stream an unbounded error.
    async fn error_response(
        &self,
        status: u16,
        response: reqwest::Response,
        control: &dyn OperationControl,
    ) -> ProviderError {
        let body = match read_error_body(response, control).await {
            ErrorBody::Stopped(error) => return error,
            ErrorBody::Unreadable => {
                return ProviderError::Other(format!(
                    "OpenAI-compatible HTTP {status} (error body unreadable)"
                ));
            }
            ErrorBody::Body(body) => body,
        };
        let safe = self.redact(&body);
        if let Err(error) = check_running(control) {
            return error;
        }
        if let Some(overflow) = classify_overflow(status, &safe) {
            return ProviderError::Overflow(overflow);
        }
        ProviderError::Other(format!(
            "OpenAI-compatible HTTP {status}: {}",
            String::from_utf8_lossy(&safe).trim()
        ))
    }

    /// Redacts the API key from text that is about to leave the module.
    /// [`ProviderError::Overflow`] is returned untouched because it carries no
    /// provider text to redact.
    fn safe_error(&self, error: ProviderError) -> ProviderError {
        match error {
            ProviderError::Other(message) if !self.api_key.is_empty() => ProviderError::Other(
                String::from_utf8_lossy(&self.redact(message.as_bytes())).into_owned(),
            ),
            other => other,
        }
    }

    /// Replaces every occurrence of the API key. A key shorter than
    /// `[REDACTED]` is replaced by as many `*` as it has bytes, so the
    /// replacement never reveals the key's length by being longer than it.
    /// An empty key redacts nothing.
    fn redact(&self, body: &[u8]) -> Vec<u8> {
        if self.api_key.is_empty() {
            return body.to_vec();
        }
        let key = self.api_key.as_bytes();
        let stars = vec![b'*'; key.len()];
        let replacement: &[u8] = if REDACTED.len() > key.len() {
            &stars
        } else {
            REDACTED.as_bytes()
        };
        let mut out = Vec::with_capacity(body.len());
        let mut index = 0;
        while index < body.len() {
            if body[index..].starts_with(key) {
                out.extend_from_slice(replacement);
                index += key.len();
            } else {
                out.push(body[index]);
                index += 1;
            }
        }
        out
    }
}

#[async_trait::async_trait]
impl Provider for Client {
    /// Sends exactly one chat completion and assembles its streamed response.
    /// Transport errors, 429/5xx statuses, and interrupted streams are never
    /// retried because a transport cannot prove that the request had no effect.
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<Response, ProviderError> {
        check_running(control)?;
        let ready = match &self.state {
            Ok(ready) => ready,
            Err(error) => return Err(ProviderError::Other(error.clone())),
        };
        let payload = serde_json::to_vec(&build_request(request)).map_err(|error| {
            self.safe_error(ProviderError::Other(format!(
                "encode chat completion request: {error}"
            )))
        })?;
        check_running(control)?;
        self.attempt(ready, &payload, emit, control)
            .await
            .map_err(|error| self.safe_error(error))
    }
}

impl RequestSizer for Client {
    /// Returns the exact number of bytes the request occupies on the wire.
    fn serialized_request_size(&self, request: &Request) -> Result<usize, ProviderError> {
        serialized_request_size(request).map_err(|error| {
            ProviderError::Other(format!("encode chat completion request: {error}"))
        })
    }
}

/// The outcome of reading a bounded prefix of an error body.
enum ErrorBody {
    Stopped(ProviderError),
    Unreadable,
    Body(Vec<u8>),
}

/// Reads at most [`MAX_ERROR_BODY`] bytes of an error response.
///
/// Reading stops as soon as the bound is passed, so the rest of the body is
/// never buffered. The result is truncated to exactly the bound.
async fn read_error_body(response: reqwest::Response, control: &dyn OperationControl) -> ErrorBody {
    let mut stream = Box::pin(response.bytes_stream());
    let mut body: Vec<u8> = Vec::new();
    while body.len() <= MAX_ERROR_BODY {
        match await_control(control, stream.try_next()).await {
            Err(error) => return ErrorBody::Stopped(error),
            Ok(Ok(Some(chunk))) => body.extend_from_slice(&chunk),
            Ok(Ok(None)) => break,
            Ok(Err(_)) => {
                if let Err(error) = check_running(control) {
                    return ErrorBody::Stopped(error);
                }
                return ErrorBody::Unreadable;
            }
        }
    }
    body.truncate(MAX_ERROR_BODY);
    ErrorBody::Body(body)
}

/// Races a model-list await against its legacy per-call cancellation token.
async fn with_cancel<T>(cancel: &CancellationToken, future: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        value = future => Some(value),
        () = cancel.cancelled() => None,
    }
}

fn stop_error(control: &dyn OperationControl) -> Option<ProviderError> {
    control.stop_reason().map(|reason| match reason {
        OperationStopReason::Deadline => ProviderError::DeadlineExceeded,
        OperationStopReason::UserCancellation
        | OperationStopReason::Shutdown
        | OperationStopReason::Migration
        | OperationStopReason::TransportLost
        | OperationStopReason::ProcessLost => ProviderError::Cancelled,
    })
}

fn check_running(control: &dyn OperationControl) -> Result<(), ProviderError> {
    match stop_error(control) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Runs one await against the operation token. A ready future wins the token
/// race; callers still inspect the stop reason before accepting partial work.
async fn await_control<T>(
    control: &dyn OperationControl,
    future: impl Future<Output = T>,
) -> Result<T, ProviderError> {
    check_running(control)?;
    tokio::select! {
        biased;
        value = future => Ok(value),
        () = control.cancellation_token().cancelled() => {
            Err(stop_error(control).unwrap_or(ProviderError::Cancelled))
        }
    }
}

/// Joins an error with its causes, so a wrapped transport error names the
/// reason instead of hiding it behind a generic summary.
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

/// The hardened client used when the caller supplies none.
///
/// A default [`reqwest::Client`] has no connect timeout and follows up to ten
/// redirects anywhere, so a stalled or hostile endpoint could hold a request
/// open or bounce the `Authorization` header to another origin. There is
/// deliberately no overall request timeout: a streaming completion may run for
/// minutes and is bounded by the caller's cancellation token instead.
///
/// reqwest exposes no cap on the size of a response header block, so none is
/// enforced here. reqwest exposes no cap on the size of a response header
/// block.
fn default_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(KEEPALIVE)
        .read_timeout(READ_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let previous = attempt.previous();
            if previous.len() >= MAX_REDIRECTS {
                return attempt.error(format!("stopped after {MAX_REDIRECTS} redirects"));
            }
            if let Some(last) = previous.last() {
                let next = attempt.url();
                let changed_origin = next.scheme() != last.scheme()
                    || next.host_str() != last.host_str()
                    || next.port_or_known_default() != last.port_or_known_default();
                if changed_origin {
                    return attempt.error("redirect to a different origin is blocked");
                }
            }
            attempt.follow()
        }))
        .build()
}

/// Normalizes and validates a provider base URL.
///
/// Returns `None` for an unparsable URL, a scheme other than `http` or `https`,
/// no host, embedded userinfo, any query (including a bare `?`), or a fragment.
/// One trailing `/` is trimmed from the path so that `{base}/chat/completions`
/// never doubles the separator.
///
/// This is a pure function over borrowed input with no shared state.
fn normalize_base_url(base_url: &str) -> Option<String> {
    let raw = base_url.as_bytes();
    // The fragment is split off first, then the query, so a bare `?` with
    // nothing after it is exactly "a `?` before the `#`".
    let before_fragment = match raw.iter().position(|&byte| byte == b'#') {
        Some(hash) => &raw[..hash],
        None => raw,
    };
    let parsed = gourl::parse(raw).ok()?;
    if (parsed.scheme != b"http" && parsed.scheme != b"https")
        || parsed.host.is_empty()
        || parsed.user.is_some()
        || !parsed.raw_query.is_empty()
        || before_fragment.contains(&b'?')
        || !parsed.fragment.is_empty()
    {
        return None;
    }
    let mut path = gourl::escape(&parsed.path, Encoding::Path);
    if path.last() == Some(&b'/') {
        path.pop();
    }
    let host = gourl::escape(&parsed.host, Encoding::Host);
    let mut url = String::from_utf8(parsed.scheme).ok()?;
    url.push_str("://");
    url.push_str(&String::from_utf8(host).ok()?);
    url.push_str(&String::from_utf8(path).ok()?);
    Some(url)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use otto_core::model::{Block, Message, Role};
    use otto_core::provider::StreamEvent;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    /// A complete, minimal SSE body that finishes without emitting anything.
    const DONE_STREAM: &str =
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    struct StoppedControl {
        token: CancellationToken,
        reason: OperationStopReason,
    }

    impl StoppedControl {
        fn deadline() -> Self {
            let token = CancellationToken::new();
            token.cancel();
            Self {
                token,
                reason: OperationStopReason::Deadline,
            }
        }
    }

    impl OperationControl for StoppedControl {
        fn cancellation_token(&self) -> &CancellationToken {
            &self.token
        }

        fn remaining(&self) -> Option<Duration> {
            Some(Duration::ZERO)
        }

        fn stop_reason(&self) -> Option<OperationStopReason> {
            Some(self.reason)
        }
    }

    /// A loopback HTTP/1.1 origin server.
    ///
    /// The accept loop is aborted when the guard is dropped, so a test never
    /// leaks a listener. Handlers return the exact bytes to write, which lets
    /// the same helper serve well-formed responses and the truncated or absent
    /// ones the single-attempt tests need.
    struct TestServer {
        base_url: String,
        accept: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.accept.abort();
        }
    }

    /// Starts a server on an ephemeral loopback port.
    ///
    /// `handler` receives the request head as text and the request body, and
    /// returns the raw response bytes. An empty return closes the connection
    /// without a response, which is how the tests produce a transport error.
    async fn spawn_server<H>(handler: H) -> TestServer
    where
        H: Fn(&str, &[u8]) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let base_url = format!(
            "http://{}",
            listener.local_addr().expect("listener has an address")
        );
        let handler = Arc::new(handler);
        let accept = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let Some((head, body)) = read_request(&mut stream).await else {
                        return;
                    };
                    let response = handler(&head, &body);
                    if !response.is_empty() {
                        let _ = stream.write_all(&response).await;
                    }
                    let _ = stream.shutdown().await;
                });
            }
        });
        TestServer { base_url, accept }
    }

    /// Reads one request. Returns `None` if the peer closed early.
    async fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
        let mut raw: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 4096];
        let head_end = loop {
            if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
            let read = stream.read(&mut buffer).await.ok()?;
            if read == 0 {
                return None;
            }
            raw.extend_from_slice(&buffer[..read]);
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
        let length = head
            .to_ascii_lowercase()
            .lines()
            .find_map(|line| line.strip_prefix("content-length:").map(str::to_string))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = raw[head_end..].to_vec();
        while body.len() < length {
            let read = stream.read(&mut buffer).await.ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buffer[..read]);
        }
        Some((head, body))
    }

    /// Serializes a complete response with a `Content-Length` body.
    fn http_response(status: u16, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status} Status\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("\r\n");
        out.push_str(body);
        out.into_bytes()
    }

    /// A 200 response whose chunked body stops mid-message, so the client sees
    /// a transport read error rather than a clean end of body.
    fn truncated_stream(prefix: &str) -> Vec<u8> {
        let mut out = String::from(
            "HTTP/1.1 200 Status\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        );
        if !prefix.is_empty() {
            out.push_str(&format!("{:x}\r\n{prefix}\r\n", prefix.len()));
        }
        out.into_bytes()
    }

    fn model_request() -> Request {
        Request {
            model: "model".to_string(),
            ..Request::default()
        }
    }

    /// Runs one completion with a no-op sink and no cancellation.
    async fn complete(client: &Client, request: &Request) -> Result<Response, ProviderError> {
        let mut emit = |_: StreamEvent| {};
        client
            .complete(request, &mut emit, &CancellationToken::new())
            .await
    }

    #[tokio::test]
    async fn a_deadline_stopped_call_reports_deadline_exceeded() {
        let server = spawn_server(|_head, _body| {
            http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
        })
        .await;
        let client = Client::new(&server.base_url, "key");
        let mut emit = |_: StreamEvent| panic!("no event is emitted after deadline");
        let error = client
            .complete(&model_request(), &mut emit, &StoppedControl::deadline())
            .await
            .unwrap_err();
        assert!(
            matches!(error, ProviderError::DeadlineExceeded),
            "{error:?}"
        );
    }

    // --- single attempt --------------------------------------------------

    #[tokio::test]
    async fn rate_limits_and_server_errors_are_not_retried() {
        for status in [429u16, 503] {
            let attempts = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&attempts);
            let server = spawn_server(move |_head, _body| {
                counter.fetch_add(1, Ordering::SeqCst);
                http_response(status, &[], "try again")
            })
            .await;

            let client = Client::new(&server.base_url, "key");
            let mut events = Vec::new();
            let mut emit = |event| events.push(event);
            let error = client
                .complete(&model_request(), &mut emit, &CancellationToken::new())
                .await
                .expect_err("the first status is returned");
            assert!(error.to_string().contains(&status.to_string()));
            assert_eq!(attempts.load(Ordering::SeqCst), 1, "status {status}");
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, StreamEvent::Retry { .. })),
                "status {status} emitted a retry"
            );
        }
    }

    #[tokio::test]
    async fn send_errors_are_not_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            counter.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        })
        .await;

        complete(&Client::new(&server.base_url, "key"), &model_request())
            .await
            .expect_err("the first send failure is returned");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_cut_before_any_delta_is_not_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            counter.fetch_add(1, Ordering::SeqCst);
            truncated_stream("")
        })
        .await;

        complete(&Client::new(&server.base_url, "key"), &model_request())
            .await
            .expect_err("the first interrupted stream is returned");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_cut_after_a_delta_is_not_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            counter.fetch_add(1, Ordering::SeqCst);
            truncated_stream("data: {\"choices\":[{\"delta\":{\"content\":\"visible\"}}]}\n\n")
        })
        .await;

        let mut events = Vec::new();
        let mut emit = |event| events.push(event);
        Client::new(&server.base_url, "key")
            .complete(&model_request(), &mut emit, &CancellationToken::new())
            .await
            .expect_err("the first interrupted stream is returned");
        assert!(!events.is_empty());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Retry { .. }))
        );
    }

    // --- redirect --------------------------------------------------------

    #[tokio::test]
    async fn blocks_cross_origin_redirect() {
        let target_calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&target_calls);
        let target = spawn_server(move |_head, _body| {
            counted.fetch_add(1, Ordering::SeqCst);
            http_response(500, &[], "must not be reached")
        })
        .await;

        let source_calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&source_calls);
        let location = format!("{}/chat/completions", target.base_url);
        let source = spawn_server(move |_head, _body| {
            counted.fetch_add(1, Ordering::SeqCst);
            http_response(302, &[("Location", &location)], "")
        })
        .await;

        let client = Client::new(&source.base_url, "secret");
        let error = complete(&client, &model_request())
            .await
            .expect_err("a cross-origin redirect is rejected");

        assert!(error.to_string().contains("redirect"), "error = {error}");
        assert_eq!(source_calls.load(Ordering::SeqCst), 1);
        assert_eq!(target_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn follows_same_origin_redirect_preserving_authorization() {
        let authorization = Arc::new(Mutex::new(String::new()));
        let seen = Arc::clone(&authorization);
        let server = spawn_server(move |head, _body| {
            if head.starts_with("POST /redirect/chat/completions ") {
                return http_response(302, &[("Location", "/v1/chat/completions")], "");
            }
            if !head.starts_with("GET /v1/chat/completions ") {
                return http_response(404, &[], "unexpected path");
            }
            *seen.lock().expect("header log is not poisoned") = header(head, "authorization");
            http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
        })
        .await;

        let client = Client::new(&format!("{}/redirect", server.base_url), "secret");
        complete(&client, &model_request())
            .await
            .expect("the redirect is followed");
        assert_eq!(
            *authorization.lock().expect("header log is not poisoned"),
            "Bearer secret"
        );
    }

    // --- redaction -------------------------------------------------------

    #[tokio::test]
    async fn does_not_retry_unauthorized_and_redacts_bounded_error() {
        const KEY: &str = "very-secret-key";
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |head, _body| {
            counter.fetch_add(1, Ordering::SeqCst);
            let body = format!(
                "{} / {KEY} / {}TAIL-MARKER",
                header(head, "authorization"),
                "x".repeat(40 << 10)
            );
            http_response(401, &[], &body)
        })
        .await;

        let client = Client::new(&server.base_url, KEY);
        let message = complete(&client, &model_request())
            .await
            .expect_err("401 is an error")
            .to_string();

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(!message.contains(KEY), "error leaked the API key");
        assert!(
            !message.contains(&format!("Bearer {KEY}")),
            "error leaked the Authorization header"
        );
        assert!(!message.contains("TAIL-MARKER"), "error was not truncated");
        assert!(
            message.len() <= MAX_ERROR_BODY + 256,
            "error is {} bytes",
            message.len()
        );
    }

    #[tokio::test]
    async fn redacts_api_key_before_context_overflow_classification() {
        const KEY: &str = "maximum context length";
        let server = spawn_server(|_head, _body| {
            http_response(
                400,
                &[],
                r#"{"error":{"message":"Bearer maximum context length"}}"#,
            )
        })
        .await;

        let client = Client::new(&server.base_url, KEY);
        let error = complete(&client, &model_request())
            .await
            .expect_err("400 is an error");

        assert!(
            !matches!(error, ProviderError::Overflow(_)),
            "a redacted key forged an overflow: {error}"
        );
        assert!(!error.to_string().contains(KEY), "error leaked the API key");
    }

    // --- overflow --------------------------------------------------------

    #[tokio::test]
    async fn returns_typed_context_overflow_without_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let server = spawn_server(move |_head, _body| {
            counter.fetch_add(1, Ordering::SeqCst);
            http_response(
                400,
                &[],
                r#"{"error":{"code":"context_length_exceeded","message":"maximum context length is 128000 tokens; requested 130000 tokens"}}"#,
            )
        })
        .await;

        let client = Client::new(&server.base_url, "secret");
        let error = complete(&client, &model_request())
            .await
            .expect_err("400 is an error");

        let ProviderError::Overflow(overflow) = error else {
            panic!("error = {error}, want a typed overflow");
        };
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(overflow.status, 400);
        assert_eq!(overflow.code, "context_length_exceeded");
        assert_eq!(overflow.maximum_tokens, 128_000);
        assert_eq!(overflow.current_tokens, 130_000);
    }

    #[tokio::test]
    async fn typed_context_overflow_redacts_api_key_and_body_secrets() {
        const KEY: &str = "very-secret-api-key";
        const BODY_SECRET: &str = "body-secret-value";
        let server = spawn_server(|_head, _body| {
            http_response(
                413,
                &[],
                &format!(
                    r#"{{"error":{{"message":"maximum context length; credentials {KEY} and {BODY_SECRET}"}}}}"#
                ),
            )
        })
        .await;

        let client = Client::new(&server.base_url, KEY);
        let error = complete(&client, &model_request())
            .await
            .expect_err("413 is an error");

        assert!(
            matches!(error, ProviderError::Overflow(_)),
            "error = {error}, want a typed overflow"
        );
        let message = error.to_string();
        assert!(!message.contains(KEY), "typed error leaked the API key");
        assert!(
            !message.contains(BODY_SECRET),
            "typed error leaked a body secret"
        );
        assert!(
            !message.contains("credentials"),
            "typed error leaked provider text"
        );
    }

    #[tokio::test]
    async fn does_not_classify_context_overflow_past_the_error_body_limit() {
        let server = spawn_server(|_head, _body| {
            let body = format!(
                "{}{}",
                " ".repeat(MAX_ERROR_BODY),
                r#"{"error":{"code":"context_length_exceeded","message":"TAIL-SECRET"}}"#
            );
            http_response(400, &[], &body)
        })
        .await;

        let client = Client::new(&server.base_url, "key");
        let error = complete(&client, &model_request())
            .await
            .expect_err("400 is an error");

        assert!(
            !matches!(error, ProviderError::Overflow(_)),
            "content past the limit was classified: {error}"
        );
        let message = error.to_string();
        assert!(!message.contains("TAIL-SECRET"), "error leaked body tail");
        assert!(
            message.len() <= MAX_ERROR_BODY + 256,
            "error is {} bytes",
            message.len()
        );
    }

    // --- request shape ---------------------------------------------------

    #[test]
    fn rejects_invalid_base_urls() {
        for base_url in [
            "",
            "ftp://example.test/v1",
            "http:///v1",
            "https://example.test/v1?tenant=x",
            "https://example.test/v1?",
            "https://example.test/v1#fragment",
            "https://username@example.test/v1",
            "https://username:password@example.test/v1",
        ] {
            assert_eq!(normalize_base_url(base_url), None, "base URL {base_url:?}");
        }
    }

    #[tokio::test]
    async fn an_invalid_base_url_fails_the_first_complete() {
        let error = complete(
            &Client::new("ftp://example.test/v1", "key"),
            &model_request(),
        )
        .await
        .expect_err("an invalid base URL is an error");
        assert_eq!(error.to_string(), "invalid OpenAI-compatible base URL");
    }

    #[test]
    fn normalizes_accepted_base_urls() {
        for (base_url, want) in [
            ("https://example.test/v1", "https://example.test/v1"),
            (
                "https://example.test/gateway/v1/",
                "https://example.test/gateway/v1",
            ),
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080"),
            ("http://127.0.0.1:8080/", "http://127.0.0.1:8080"),
        ] {
            assert_eq!(normalize_base_url(base_url).as_deref(), Some(want));
        }
    }

    #[test]
    fn serialized_request_size_matches_the_exact_wire_payload() {
        let request = Request {
            model: "model\\\"界".to_string(),
            system_prompt: "system\n\t\\\"界".to_string(),
            thinking: "high".to_string(),
            messages: vec![Message {
                role: Role::User,
                blocks: vec![Block::text("escape-heavy: \\ \" \n \t 界 <>&")],
                ..Message::default()
            }],
            tools: Vec::new(),
        };
        let payload = serde_json::to_vec(&build_request(&request)).expect("the request encodes");
        let client = Client::new("https://example.test/v1", "key");
        assert_eq!(
            client
                .serialized_request_size(&request)
                .expect("the request encodes"),
            payload.len()
        );
    }

    #[tokio::test]
    async fn sends_the_expected_request_line_headers_and_body() {
        let observed = Arc::new(Mutex::new((String::new(), Vec::new())));
        let seen = Arc::clone(&observed);
        let server = spawn_server(move |head, body| {
            *seen.lock().expect("request log is not poisoned") = (head.to_string(), body.to_vec());
            http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
        })
        .await;

        let client = Client::new(&format!("{}/gateway/v1/", server.base_url), "secret");
        complete(&client, &model_request())
            .await
            .expect("the request succeeds");

        let (head, body) = observed
            .lock()
            .expect("request log is not poisoned")
            .clone();
        assert!(
            head.starts_with("POST /gateway/v1/chat/completions HTTP/1.1\r\n"),
            "request line = {:?}",
            head.lines().next()
        );
        assert_eq!(header(&head, "authorization"), "Bearer secret");
        assert_eq!(header(&head, "content-type"), "application/json");
        assert_eq!(header(&head, "accept"), "text/event-stream");
        let decoded: serde_json::Value = serde_json::from_slice(&body).expect("the body is JSON");
        assert_eq!(decoded["model"], "model");
        assert_eq!(decoded["stream"], true);
    }

    #[tokio::test]
    async fn list_models_gets_the_models_path_and_returns_sorted_unique_ids() {
        let observed = Arc::new(Mutex::new(String::new()));
        let seen = Arc::clone(&observed);
        let server = spawn_server(move |head, _body| {
            *seen.lock().expect("request log is not poisoned") = head.to_string();
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"object":"list","data":[{"id":"gpt-5.6","object":"model"},{"id":"deepseek-chat"},{"id":"gpt-5.6"}]}"#,
            )
        })
        .await;

        let client = Client::new(&format!("{}/gateway/v1/", server.base_url), "secret");
        let ids = client
            .list_models(&CancellationToken::new())
            .await
            .expect("the list is returned");

        assert_eq!(ids, vec!["deepseek-chat", "gpt-5.6"]);
        let head = observed
            .lock()
            .expect("request log is not poisoned")
            .clone();
        assert!(
            head.starts_with("GET /gateway/v1/models HTTP/1.1\r\n"),
            "request line = {:?}",
            head.lines().next()
        );
        assert_eq!(header(&head, "authorization"), "Bearer secret");
    }

    #[tokio::test]
    async fn list_models_reports_a_redacted_http_error_without_retrying() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            counter.fetch_add(1, Ordering::SeqCst);
            http_response(503, &[], "bad key sk-secret-value-123")
        })
        .await;

        let client = Client::new(&server.base_url, "sk-secret-value-123");
        let error = client
            .list_models(&CancellationToken::new())
            .await
            .expect_err("a 503 is an error");

        let ProviderError::Other(message) = error else {
            panic!("unexpected error {error:?}");
        };
        assert_eq!(message, "OpenAI-compatible HTTP 503: bad key [REDACTED]");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn list_models_rejects_a_body_without_a_data_list() {
        let server = spawn_server(|_head, _body| http_response(200, &[], r#"{"models":[]}"#)).await;

        let client = Client::new(&server.base_url, "key");
        let error = client
            .list_models(&CancellationToken::new())
            .await
            .expect_err("the body has no data list");

        let ProviderError::Other(message) = error else {
            panic!("unexpected error {error:?}");
        };
        assert!(message.starts_with("decode model list: "), "{message}");
    }

    /// Returns the value of one request header, matched case-insensitively.
    fn header(head: &str, name: &str) -> String {
        head.lines()
            .find(|line| {
                line.to_ascii_lowercase()
                    .starts_with(&format!("{name}: ").to_ascii_lowercase())
            })
            .map(|line| line[name.len() + 2..].trim().to_string())
            .unwrap_or_default()
    }
}
