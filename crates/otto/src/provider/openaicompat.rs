//! HTTP transport for the OpenAI-compatible Chat Completions API.
//!
//! The wire codec lives in [`otto_core::openaicompat`]; this module owns only
//! the parts that need the network: base-URL validation, connection settings,
//! the retry policy, the bounded error-body reader, and API-key redaction.
//!
//! Ownership: a [`Client`] owns its base URL, its API key, and its
//! [`reqwest::Client`]. The request passed to `complete` is borrowed and never
//! retained; the returned response belongs to the caller.
//!
//! Concurrency: `complete` takes `&self` and holds no mutable state, so one
//! client can serve a parent agent and its sub-agents at the same time.
//! `reqwest::Client` shares its connection pool across those calls.
//!
//! Cancellation: every await, the request send, each body read, and the retry
//! backoff, races the caller's [`CancellationToken`]. A cancelled call returns
//! [`ProviderError::Cancelled`] and never a transport error, so the agent can
//! tell "the user stopped this" apart from "the provider failed".
//!
//! Errors: every failure is a [`ProviderError`]. Text of an
//! [`ProviderError::Other`] is passed through API-key redaction before it
//! leaves this module. [`ProviderError::Overflow`] carries only a status, an
//! allowlisted code, and two token counts, never provider text.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{NaiveDateTime, Utc};
use futures_util::TryStreamExt;
use tokio_util::sync::CancellationToken;

use otto_core::openaicompat::overflow::{MAX_ERROR_BODY, classify_overflow};
use otto_core::openaicompat::protocol::{build_request, serialized_request_size};
use otto_core::openaicompat::stream::StreamAssembler;
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
/// Total attempts, the first plus at most two retries.
const MAX_ATTEMPTS: u32 = 3;
/// Backoff before the first retry; it doubles for each further retry.
const BASE_BACKOFF: Duration = Duration::from_millis(250);
/// The text an API key is replaced with when it is long enough to hold it.
const REDACTED: &str = "[REDACTED]";

/// Waits out a retry backoff. Injected so tests observe the delays without
/// waiting for them; production uses [`tokio::time::sleep`].
type Sleeper = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

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
    sleep: Sleeper,
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
            sleep: Arc::new(|delay| Box::pin(tokio::time::sleep(delay))),
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
            sleep: Arc::new(|delay| Box::pin(tokio::time::sleep(delay))),
        }
    }

    fn ready(base_url: &str, http: reqwest::Client) -> Result<Ready, String> {
        match normalize_base_url(base_url) {
            Some(base_url) => Ok(Ready { base_url, http }),
            None => Err("invalid OpenAI-compatible base URL".to_string()),
        }
    }

    /// Replaces the backoff sleeper so a test can record delays instead of
    /// waiting. Cancellation is handled by the caller of the sleeper, so a
    /// test sleeper only has to resolve.
    #[cfg(test)]
    fn with_sleeper(mut self, sleep: Sleeper) -> Self {
        self.sleep = sleep;
        self
    }

    /// One request/response round trip, including reading the whole stream.
    async fn attempt(
        &self,
        ready: &Ready,
        payload: &[u8],
        emit: StreamSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Response, Failure> {
        let send = ready
            .http
            .post(format!("{}/chat/completions", ready.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .body(payload.to_vec())
            .send();
        let response = match with_cancel(cancel, send).await {
            None => return Err(Failure::cancelled()),
            Some(Ok(response)) => response,
            Some(Err(error)) => {
                // A redirect-policy rejection is a decision, not a transient
                // fault, so it is the one send failure that is not retried.
                let retryable = !error.is_redirect();
                return Err(Failure {
                    error: ProviderError::Other(format!(
                        "send chat completion request: {}",
                        error_chain(&error)
                    )),
                    emitted: false,
                    retryable,
                    retry_after: None,
                });
            }
        };

        let status = response.status();
        if !status.is_success() {
            return Err(self.error_response(status.as_u16(), response, cancel).await);
        }

        let mut assembler = StreamAssembler::new();
        let mut body = Box::pin(response.bytes_stream());
        while !assembler.is_done() {
            let chunk = match with_cancel(cancel, body.try_next()).await {
                None => return Err(Failure::cancelled()),
                Some(Ok(Some(chunk))) => chunk,
                Some(Ok(None)) => break,
                // The only retryable stream failure: the body was cut short
                // before the response finished.
                Some(Err(error)) => {
                    return Err(Failure {
                        error: ProviderError::Other(format!(
                            "read chat completion stream: {}",
                            error_chain(&error)
                        )),
                        emitted: assembler.emitted(),
                        retryable: true,
                        retry_after: None,
                    });
                }
            };
            if let Err(error) = assembler.push(&chunk, &mut *emit) {
                return Err(Failure::fatal(error.to_string(), assembler.emitted()));
            }
        }
        let emitted = assembler.emitted();
        assembler
            .finish(&mut *emit)
            .map_err(|error| Failure::fatal(error.to_string(), emitted))
    }

    /// Turns a non-2xx response into a failure, reading a bounded prefix of
    /// the body so a hostile endpoint cannot stream an unbounded error.
    async fn error_response(
        &self,
        status: u16,
        response: reqwest::Response,
        cancel: &CancellationToken,
    ) -> Failure {
        // The header is captured on every non-2xx status, retryable or not, and
        // consulted only when a retry is decided on.
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after);
        let body = match read_error_body(response, cancel).await {
            ErrorBody::Cancelled => return Failure::cancelled(),
            ErrorBody::Unreadable => {
                return Failure {
                    error: ProviderError::Other(format!(
                        "OpenAI-compatible HTTP {status} (error body unreadable)"
                    )),
                    emitted: false,
                    retryable: is_retryable_status(status),
                    retry_after,
                };
            }
            ErrorBody::Body(body) => body,
        };
        // Redaction runs before classification so that an API key which itself
        // reads like an overflow message cannot forge a typed overflow error.
        let safe = self.redact(&body);
        if let Some(overflow) = classify_overflow(status, &safe) {
            return Failure {
                error: ProviderError::Overflow(overflow),
                emitted: false,
                retryable: false,
                retry_after: None,
            };
        }
        Failure {
            error: ProviderError::Other(format!(
                "OpenAI-compatible HTTP {status}: {}",
                String::from_utf8_lossy(&safe).trim()
            )),
            emitted: false,
            retryable: is_retryable_status(status),
            retry_after,
        }
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
    /// Sends one chat completion and assembles the streamed response.
    ///
    /// Retries at most twice, and only while nothing has been emitted: once
    /// the frontend has seen part of a response, a retry would duplicate it.
    /// Retryable failures are HTTP 429 and 5xx, a send failure that is not a
    /// redirect-policy rejection, and a body cut short mid-stream.
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Response, ProviderError> {
        let ready = match &self.state {
            Ok(ready) => ready,
            Err(error) => return Err(ProviderError::Other(error.clone())),
        };
        let payload = serde_json::to_vec(&build_request(request)).map_err(|error| {
            self.safe_error(ProviderError::Other(format!(
                "encode chat completion request: {error}"
            )))
        })?;

        for attempt in 0..MAX_ATTEMPTS {
            let failure = match self.attempt(ready, &payload, &mut *emit, cancel).await {
                Ok(response) => return Ok(response),
                Err(failure) => failure,
            };
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            if !failure.retryable || failure.emitted || attempt == MAX_ATTEMPTS - 1 {
                return Err(self.safe_error(failure.error));
            }
            let delay = failure
                .retry_after
                .unwrap_or(BASE_BACKOFF * (1u32 << attempt));
            if with_cancel(cancel, (self.sleep)(delay)).await.is_none() {
                return Err(ProviderError::Cancelled);
            }
        }
        unreachable!("the loop returns on the last attempt")
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

/// One failed attempt, and what the retry loop needs to decide about it.
struct Failure {
    error: ProviderError,
    /// A stream event already reached the caller, so a retry would duplicate
    /// visible output.
    emitted: bool,
    retryable: bool,
    /// The delay a `Retry-After` header asked for, used only when retrying.
    retry_after: Option<Duration>,
}

impl Failure {
    fn cancelled() -> Self {
        Self {
            error: ProviderError::Cancelled,
            emitted: false,
            retryable: false,
            retry_after: None,
        }
    }

    /// A decoding failure: the response arrived intact and was rejected, so
    /// repeating the request would only produce the same rejection.
    fn fatal(message: String, emitted: bool) -> Self {
        Self {
            error: ProviderError::Other(message),
            emitted,
            retryable: false,
            retry_after: None,
        }
    }
}

/// The outcome of reading a bounded prefix of an error body.
enum ErrorBody {
    Cancelled,
    Unreadable,
    Body(Vec<u8>),
}

/// Reads at most [`MAX_ERROR_BODY`] bytes of an error response.
///
/// Reading stops as soon as the bound is passed, so the rest of the body is
/// never buffered. The result is truncated to exactly the bound.
async fn read_error_body(response: reqwest::Response, cancel: &CancellationToken) -> ErrorBody {
    let mut stream = Box::pin(response.bytes_stream());
    let mut body: Vec<u8> = Vec::new();
    while body.len() <= MAX_ERROR_BODY {
        match with_cancel(cancel, stream.try_next()).await {
            None => return ErrorBody::Cancelled,
            Some(Ok(Some(chunk))) => body.extend_from_slice(&chunk),
            Some(Ok(None)) => break,
            Some(Err(_)) => return ErrorBody::Unreadable,
        }
    }
    body.truncate(MAX_ERROR_BODY);
    ErrorBody::Body(body)
}

/// Races `future` against the cancellation token. `None` means the token was
/// cancelled, in which case `future` was dropped without completing.
async fn with_cancel<T>(cancel: &CancellationToken, future: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        value = future => Some(value),
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

/// Reports whether a status is worth another attempt: rate limiting, or any
/// server-side error.
fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

/// The three date layouts Go's `http.ParseTime` accepts, as chrono formats.
/// None of them carries a usable offset, so all three are read as UTC, which
/// is what the two GMT-anchored layouts mean and the closest reading of the
/// third.
const HTTP_DATE_FORMATS: [&str; 3] = [
    "%a, %d %b %Y %H:%M:%S GMT",
    "%A, %d-%b-%y %H:%M:%S %Z",
    "%a %b %e %H:%M:%S %Y",
];

/// Parses a `Retry-After` header value into a delay.
///
/// A non-negative integer count of seconds wins. Otherwise an HTTP date is
/// tried and its distance from now is used, clamped at zero for a date in the
/// past. Anything else yields `None`, and the caller falls back to the
/// exponential backoff.
fn parse_retry_after(value: &str) -> Option<Duration> {
    if value.is_empty() {
        return None;
    }
    if let Ok(seconds) = value.trim().parse::<i64>() {
        return u64::try_from(seconds).ok().map(Duration::from_secs);
    }
    let now = Utc::now();
    for format in HTTP_DATE_FORMATS {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(value, format) {
            return Some((parsed.and_utc() - now).to_std().unwrap_or(Duration::ZERO));
        }
    }
    None
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
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use otto_core::model::{Block, Message, Role};
    use otto_core::provider::StreamEvent;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    /// A complete, minimal SSE body that finishes without emitting anything.
    const DONE_STREAM: &str =
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    /// A loopback HTTP/1.1 origin server.
    ///
    /// The accept loop is aborted when the guard is dropped, so a test never
    /// leaks a listener. Handlers return the exact bytes to write, which lets
    /// the same helper serve well-formed responses and the truncated or absent
    /// ones the retry tests need.
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

    /// A sleeper that records every delay and returns immediately.
    fn recording_sleeper() -> (Sleeper, Arc<Mutex<Vec<Duration>>>) {
        let delays = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&delays);
        let sleeper: Sleeper = Arc::new(move |delay| {
            recorded
                .lock()
                .expect("delay log is not poisoned")
                .push(delay);
            Box::pin(std::future::ready(()))
        });
        (sleeper, delays)
    }

    /// A sleeper that fails the test if the retry loop ever waits.
    fn forbidden_sleeper() -> Sleeper {
        Arc::new(|_| panic!("a non-retryable response slept"))
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

    // --- retry -----------------------------------------------------------

    #[tokio::test]
    async fn retries_rate_limits_and_server_errors_at_most_twice() {
        for status in [429u16, 503] {
            let attempts = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&attempts);
            let server = spawn_server(move |_head, _body| {
                if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                    return http_response(status, &[], "try again");
                }
                http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
            })
            .await;

            let (sleeper, delays) = recording_sleeper();
            let client = Client::new(&server.base_url, "key").with_sleeper(sleeper);
            complete(&client, &model_request())
                .await
                .expect("the third attempt succeeds");

            assert_eq!(attempts.load(Ordering::SeqCst), 3, "status {status}");
            assert_eq!(
                *delays.lock().expect("delay log is not poisoned"),
                vec![Duration::from_millis(250), Duration::from_millis(500)],
                "status {status}"
            );
        }
    }

    #[tokio::test]
    async fn honors_retry_after_seconds() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                return http_response(429, &[("Retry-After", "0")], "retry");
            }
            http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
        })
        .await;

        let (sleeper, delays) = recording_sleeper();
        let client = Client::new(&server.base_url, "key").with_sleeper(sleeper);
        complete(&client, &model_request())
            .await
            .expect("the second attempt succeeds");

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            *delays.lock().expect("delay log is not poisoned"),
            vec![Duration::ZERO]
        );
    }

    #[tokio::test]
    async fn honors_retry_after_http_date() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                let at = (Utc::now() + chrono::TimeDelta::seconds(10))
                    .format("%a, %d %b %Y %H:%M:%S GMT")
                    .to_string();
                return http_response(429, &[("Retry-After", &at)], "retry");
            }
            http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
        })
        .await;

        let (sleeper, delays) = recording_sleeper();
        let client = Client::new(&server.base_url, "key").with_sleeper(sleeper);
        complete(&client, &model_request())
            .await
            .expect("the second attempt succeeds");

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        let delays = delays.lock().expect("delay log is not poisoned");
        assert_eq!(delays.len(), 1);
        assert!(
            delays[0] > Duration::from_secs(8) && delays[0] <= Duration::from_secs(10),
            "delay = {:?}",
            delays[0]
        );
    }

    #[tokio::test]
    async fn does_not_retry_redirect_policy_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            counter.fetch_add(1, Ordering::SeqCst);
            http_response(302, &[("Location", "/elsewhere")], "")
        })
        .await;

        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                attempt.error("redirect blocked")
            }))
            .build()
            .expect("build a test HTTP client");
        let client = Client::with_http_client(&server.base_url, "key", http)
            .with_sleeper(forbidden_sleeper());

        complete(&client, &model_request())
            .await
            .expect_err("a blocked redirect is an error");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_connection_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                return Vec::new();
            }
            http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
        })
        .await;

        let (sleeper, _) = recording_sleeper();
        let client = Client::new(&server.base_url, "key").with_sleeper(sleeper);
        complete(&client, &model_request())
            .await
            .expect("the third attempt succeeds");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retries_stream_read_error_before_delta() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                return truncated_stream("");
            }
            http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
        })
        .await;

        let (sleeper, _) = recording_sleeper();
        let client = Client::new(&server.base_url, "key").with_sleeper(sleeper);
        complete(&client, &model_request())
            .await
            .expect("the third attempt succeeds");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_after_a_visible_delta() {
        let cases = [
            r#"{"choices":[{"delta":{"content":"visible"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"read","arguments":"{"}}]}}]}"#,
        ];
        for data in cases {
            let attempts = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&attempts);
            let server = spawn_server(move |_head, _body| {
                counter.fetch_add(1, Ordering::SeqCst);
                truncated_stream(&format!("data: {data}\n\n"))
            })
            .await;

            let client = Client::new(&server.base_url, "key").with_sleeper(forbidden_sleeper());
            let seen = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&seen);
            let mut emit = move |_: StreamEvent| {
                counted.fetch_add(1, Ordering::SeqCst);
            };
            let error = client
                .complete(&model_request(), &mut emit, &CancellationToken::new())
                .await
                .expect_err("a truncated body is an error");

            assert!(seen.load(Ordering::SeqCst) > 0, "no delta reached the sink");
            assert!(
                error.to_string().contains("read chat completion stream"),
                "error = {error}"
            );
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn stops_after_three_retryable_attempts() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            counter.fetch_add(1, Ordering::SeqCst);
            http_response(503, &[], "still unavailable")
        })
        .await;

        let (sleeper, delays) = recording_sleeper();
        let client = Client::new(&server.base_url, "key").with_sleeper(sleeper);
        complete(&client, &model_request())
            .await
            .expect_err("every attempt failed");

        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(delays.lock().expect("delay log is not poisoned").len(), 2);
    }

    #[tokio::test]
    async fn cancellation_during_retry_backoff_returns_cancelled() {
        let server = spawn_server(|_head, _body| http_response(503, &[], "retry later")).await;

        let (started, sleeping) = tokio::sync::oneshot::channel::<()>();
        let started = Arc::new(Mutex::new(Some(started)));
        let sleeper: Sleeper = Arc::new(move |_| {
            if let Some(sender) = started.lock().expect("sender is not poisoned").take() {
                let _ = sender.send(());
            }
            Box::pin(std::future::pending::<()>())
        });
        let client = Client::new(&server.base_url, "key").with_sleeper(sleeper);

        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        let mut emit = |_: StreamEvent| {};
        let request = model_request();
        let (result, ()) =
            tokio::join!(client.complete(&request, &mut emit, &cancel), async move {
                let _ = sleeping.await;
                canceller.cancel();
            });
        assert!(
            matches!(result, Err(ProviderError::Cancelled)),
            "result = {:?}",
            result.err().map(|error| error.to_string())
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

        let client = Client::new(&source.base_url, "secret").with_sleeper(forbidden_sleeper());
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

        let client = Client::new(&server.base_url, KEY).with_sleeper(forbidden_sleeper());
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

        let client = Client::new(&server.base_url, KEY).with_sleeper(forbidden_sleeper());
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

        let client = Client::new(&server.base_url, "secret").with_sleeper(forbidden_sleeper());
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

        let client = Client::new(&server.base_url, KEY).with_sleeper(forbidden_sleeper());
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

        let client = Client::new(&server.base_url, "key").with_sleeper(forbidden_sleeper());
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
