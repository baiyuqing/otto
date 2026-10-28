//! HTTP transport for the OpenAI-compatible Chat Completions API.
//!
//! The wire codec lives in [`otto_core::openaicompat`]; this module owns only
//! the parts that need the network: base-URL validation, connection settings,
//! the bounded retry policy (connection failures, transport interruptions and
//! HTTP 429/503), bounded error-body reader, and API-key redaction.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::TryStreamExt;
use tokio_util::sync::CancellationToken;

use otto_core::model::{EffectCertainty, OperationStopReason};
use otto_core::openaicompat::overflow::{MAX_ERROR_BODY, classify_overflow};
use otto_core::openaicompat::protocol::{build_request, serialized_request_size};
use otto_core::openaicompat::stream::StreamAssembler;
use otto_core::operation::OperationControl;
use otto_core::provider::{
    Provider, ProviderError, ProviderSettlement, Request, RequestSizer, Response, StreamEvent,
    StreamSink,
};

use crate::gourl::{self, Encoding};

mod attempt;

/// Longest wait for the TCP connect and TLS handshake of one attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP keepalive interval.
const KEEPALIVE: Duration = Duration::from_secs(30);
/// Longest wait for a single read from the socket.
///
/// reqwest has no header-only timeout, so the bound is applied per read
/// instead. Any received byte restarts it, so a body that keeps sending bytes
/// without model output is bounded by [`MODEL_IDLE_TIMEOUT`] instead. There is
/// deliberately no overall request timeout, because a long completion may
/// legitimately take minutes.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest time between model-output events (text, reasoning or tool-argument
/// deltas) on a 2xx body. Keepalive bytes and SSE comments do not reset it.
const MODEL_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Largest number of redirect hops followed before the policy stops.
const MAX_REDIRECTS: usize = 3;
/// The text an API key is replaced with when it is long enough to hold it.
const REDACTED: &str = "[REDACTED]";
/// Largest `GET /models` body read. A larger catalog is an error rather than
/// an unbounded buffer.
const MAX_MODELS_BODY: usize = 8 << 20;

/// Attempts for one completion, shared by the retryable failure classes: a
/// connection that could not be established, a dispatched request whose
/// transport failed before any stream event reached the caller, and an
/// HTTP 429 or 503 status. A `Retry-After` value replaces the backoff delay
/// and is capped at `retry_after_cap`.
const RETRY_POLICY: crate::retry::Policy = crate::retry::Policy {
    max_attempts: 4,
    base: Duration::from_secs(1),
    max: Duration::from_secs(4),
    retry_after_cap: Duration::from_secs(30),
};
/// `Retry` event reason for a connection that could not be established: the
/// request never left Otto.
const CONNECT_RETRY_REASON: &str = "connection failed";
/// `Retry` event reason for a dispatched request whose transport failed
/// before any stream event reached the caller.
const TRANSPORT_RETRY_REASON: &str = "connection interrupted";
/// `Retry` event reason for a body that produced no model output within
/// [`MODEL_IDLE_TIMEOUT`] before any delta reached the caller.
const MODEL_IDLE_RETRY_REASON: &str = "model output idle timeout";

// Fixed exponential backoff (1, 2, 4 s); no randomness dependency.
struct Backoff;
impl crate::retry::JitterSource for Backoff {
    fn full_jitter(&mut self, upper_bound: Duration) -> Duration {
        upper_bound
    }
}

/// How one attempt failed.
enum AttemptError {
    /// The connection could not be established, so the request never left
    /// Otto and trying again cannot duplicate provider work.
    NotSent(ProviderError),
    /// The request was dispatched and the transport failed or the model went
    /// idle, possibly after stream events reached the caller. The endpoint
    /// may already have started the completion. The reason is the `Retry`
    /// event reason; the flag is true when this attempt emitted a reasoning,
    /// text or tool-call delta, which the caller must discard on retry.
    Interrupted(ProviderError, &'static str, bool),
    /// The endpoint answered HTTP 429 or 503 before any stream event, which
    /// means it did not process the request, so trying again cannot duplicate
    /// provider work. Carries the reason (the `Retry` event reason) and the
    /// delta-seconds `Retry-After` value, if the response had one.
    RetryableStatus(ProviderError, &'static str, Option<Duration>),
    /// No retry: the failure is not a transient transport failure, a model
    /// idle timeout or a 429/503 status.
    Sent(ProviderError),
    /// HTTP 400 or 422 whose body names `prompt_cache_key`. The endpoint
    /// rejects the field; the error is reported when the request did not
    /// carry it.
    CacheKeyRejected(ProviderError),
}

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
    /// Longest time between model-output events on a response body.
    model_idle_timeout: Duration,
    /// Set when the endpoint rejected `prompt_cache_key`; later requests omit it.
    prompt_cache_key_rejected: AtomicBool,
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
            model_idle_timeout: MODEL_IDLE_TIMEOUT,
            prompt_cache_key_rejected: AtomicBool::new(false),
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
            model_idle_timeout: MODEL_IDLE_TIMEOUT,
            prompt_cache_key_rejected: AtomicBool::new(false),
        }
    }

    /// Overrides the model-output idle bound; tests use a short one.
    pub fn with_model_idle_timeout(mut self, timeout: Duration) -> Self {
        self.model_idle_timeout = timeout;
        self
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

    /// On-demand bounded catalog; unlike the legacy ID API, never exposes error bodies.
    pub async fn catalog(
        &self,
        cancel: &CancellationToken,
    ) -> Result<super::catalog::ModelCatalog, ProviderError> {
        super::catalog::controlled(cancel, async {
            let ready = self
                .state
                .as_ref()
                .map_err(|_| super::catalog::failure("model catalog client unavailable"))?;
            let http = super::catalog::http()?;
            let bytes = super::catalog::read(
                http.get(format!("{}/models", ready.base_url))
                    .bearer_auth(&self.api_key)
                    .header("Accept", "application/json"),
            )
            .await?;
            let mut catalog = super::catalog::parse(&bytes, false)?;
            super::catalog::sanitize(&mut catalog, &[&self.api_key]);
            Ok(catalog)
        })
        .await
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
}

#[async_trait::async_trait]
impl Provider for Client {
    /// Sends one chat completion and assembles its streamed response.
    ///
    /// An attempt is repeated after a connection that could not be
    /// established, a transient transport failure or model idle timeout on a
    /// dispatched request (before or after stream output), and an HTTP 429 or
    /// 503 status (the request was not processed). All share one policy (at
    /// most 4 attempts, 1/2/4 s apart, within the operation deadline) and
    /// report every planned retry as a `Retry` event; `interrupted_output` is
    /// true when the failed attempt had emitted a reasoning, text or
    /// tool-call delta, and each attempt assembles its own response. A
    /// `Retry-After` in seconds replaces the delay, capped at 30 s. Any other
    /// HTTP status (including 500, 502 and 504), a stream-codec error, a
    /// response validation failure and a non-transient transport error are
    /// never retried.
    ///
    /// A request with a cache key sends `prompt_cache_key`. If the endpoint
    /// answers HTTP 400 or 422 naming that field, the request is sent again
    /// at once without it, with no `Retry` event and without using the retry
    /// attempts; the client then omits the field from every later request.
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.complete_observed(request, emit, control, &otto_core::diagnostics::Noop)
            .await
    }

    fn complete_observed<'a>(
        &'a self,
        request: &'a Request,
        emit: StreamSink<'a>,
        control: &'a dyn OperationControl,
        observer: &'a dyn otto_core::diagnostics::Observer,
    ) -> otto_core::provider::DiagnosticFuture<'a> {
        Box::pin(async move {
            if let Err(error) = check_running(control) {
                return provider_failure(error, 0, EffectCertainty::NotStarted, control);
            }
            let ready = match &self.state {
                Ok(ready) => ready,
                Err(error) => {
                    return ProviderSettlement::failed(
                        ProviderError::Other(error.clone()),
                        0,
                        EffectCertainty::NotStarted,
                    );
                }
            };
            let mut wire = build_request(request);
            if self.prompt_cache_key_rejected.load(Ordering::Relaxed) {
                wire.prompt_cache_key.clear();
            }
            let mut payload = match serde_json::to_vec(&wire) {
                Ok(payload) => payload,
                Err(error) => {
                    return ProviderSettlement::failed(
                        self.safe_error(ProviderError::Other(format!(
                            "encode chat completion request: {error}"
                        ))),
                        0,
                        EffectCertainty::NotStarted,
                    );
                }
            };
            if let Err(error) = check_running(control) {
                return provider_failure(error, 0, EffectCertainty::NotStarted, control);
            }
            // HTTP requests sent, the cache-key resend included.
            let mut attempts = 0;
            // Failed attempts counted against `RETRY_POLICY`.
            let mut failures = 0;
            // Set once any attempt was a dispatched transport interruption.
            let mut interrupted_seen = false;
            let result = loop {
                attempts += 1;
                // Per attempt: only an interruption after output sets it.
                let mut interrupted_output = false;
                // `dispatched` records whether a retry could repeat provider work,
                // which the settlement must keep as unknown effect certainty.
                let (error, reason, dispatched, retry_after, status_retry) = match self
                    .attempt(ready, &payload, &mut *emit, control, observer)
                    .await
                {
                    Ok(response) => break Ok(response),
                    Err(AttemptError::Sent(error)) => break Err(error),
                    Err(AttemptError::CacheKeyRejected(error)) => {
                        if wire.prompt_cache_key.is_empty() {
                            break Err(error);
                        }
                        self.prompt_cache_key_rejected
                            .store(true, Ordering::Relaxed);
                        wire.prompt_cache_key.clear();
                        payload = match serde_json::to_vec(&wire) {
                            Ok(payload) => payload,
                            Err(_) => break Err(error),
                        };
                        continue;
                    }
                    Err(AttemptError::NotSent(error)) => {
                        (error, CONNECT_RETRY_REASON, false, None, false)
                    }
                    Err(AttemptError::Interrupted(error, reason, output_emitted)) => {
                        interrupted_seen = true;
                        interrupted_output = output_emitted;
                        (error, reason, true, None, false)
                    }
                    // A status proves the endpoint answered; only an earlier
                    // interruption leaves the effect unknown.
                    Err(AttemptError::RetryableStatus(error, reason, retry_after)) => {
                        (error, reason, interrupted_seen, retry_after, true)
                    }
                };
                failures += 1;
                let Some(delay) = crate::retry::next_delay(
                    RETRY_POLICY,
                    failures,
                    retry_after,
                    control.remaining().unwrap_or(Duration::MAX),
                    Duration::from_secs(1),
                    &mut Backoff,
                ) else {
                    let error = self.safe_error(error);
                    return if dispatched {
                        provider_failure(error, attempts, EffectCertainty::Unknown, control)
                    } else if status_retry {
                        ProviderSettlement::failed(error, attempts, EffectCertainty::Completed)
                    } else {
                        ProviderSettlement::failed(error, attempts, EffectCertainty::NotStarted)
                    };
                };
                emit(StreamEvent::Retry {
                    attempt: failures + 1,
                    max_attempts: RETRY_POLICY.max_attempts,
                    delay,
                    reason: reason.to_owned(),
                    interrupted_output,
                });
                if let Err(error) = await_control(control, tokio::time::sleep(delay)).await {
                    return if dispatched {
                        provider_failure(error, attempts, EffectCertainty::Unknown, control)
                    } else {
                        not_started_stop(error, attempts, control)
                    };
                }
            };
            match result {
                Ok(response) => ProviderSettlement::succeeded(response, attempts),
                Err(error) => {
                    let certainty = match &error {
                        ProviderError::DeadlineExceeded | ProviderError::Cancelled => {
                            EffectCertainty::Unknown
                        }
                        ProviderError::Other(message)
                            if message.starts_with("send chat completion request:")
                                || message.starts_with("read chat completion stream:") =>
                        {
                            EffectCertainty::Unknown
                        }
                        ProviderError::Overflow(_)
                        | ProviderError::PolicyCapacityUnavailable
                        | ProviderError::Other(_) => EffectCertainty::Completed,
                    };
                    let error = self.safe_error(error);
                    if certainty == EffectCertainty::Completed {
                        ProviderSettlement::failed(error, attempts, certainty)
                    } else {
                        provider_failure(error, attempts, certainty, control)
                    }
                }
            }
        })
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

/// A stop while waiting to retry a connection: no attempt reached the
/// endpoint, so the operation is `NotStarted`.
fn not_started_stop(
    error: ProviderError,
    attempts: u32,
    control: &dyn OperationControl,
) -> ProviderSettlement {
    match control.stop_reason() {
        Some(reason) => {
            ProviderSettlement::stopped(error, attempts, EffectCertainty::NotStarted, reason)
        }
        None => ProviderSettlement::failed(error, attempts, EffectCertainty::NotStarted),
    }
}

fn provider_failure(
    error: ProviderError,
    attempts: u32,
    certainty: EffectCertainty,
    control: &dyn OperationControl,
) -> ProviderSettlement {
    match control.stop_reason() {
        Some(reason) => ProviderSettlement::stopped(
            error,
            attempts,
            if attempts == 0 {
                EffectCertainty::NotStarted
            } else {
                EffectCertainty::Unknown
            },
            reason,
        ),
        None if attempts > 0 && certainty == EffectCertainty::Unknown => {
            ProviderSettlement::transport_lost(error, attempts)
        }
        None => ProviderSettlement::failed(error, attempts, certainty),
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
    match control.admission_stop_reason().map(|reason| match reason {
        OperationStopReason::Deadline => ProviderError::DeadlineExceeded,
        OperationStopReason::UserCancellation
        | OperationStopReason::Shutdown
        | OperationStopReason::Migration
        | OperationStopReason::TransportLost
        | OperationStopReason::ProcessLost => ProviderError::Cancelled,
    }) {
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

/// Whether a failed send or body read is a transport failure the retry policy
/// may repeat. A local builder or redirect-policy error is a decision Otto
/// made rather than a network failure, so it stays terminal; this mirrors the
/// ChatGPT transport's classification.
fn is_transient_transport(error: &reqwest::Error) -> bool {
    !error.is_builder() && !error.is_redirect()
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
mod tests;
