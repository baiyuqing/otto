//! HTTP transport for the Claude Messages API.
//!
//! The wire codec lives in [`otto_core::claude`]; this module owns only
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
use std::time::Duration;

use futures_util::TryStreamExt;
use tokio_util::sync::CancellationToken;

use otto_core::claude::overflow::{MAX_ERROR_BODY, classify_overflow};
use otto_core::claude::protocol::{build_request, serialized_request_size};
use otto_core::claude::stream::{StreamAssembler, StreamError};
use otto_core::claude::{output_budget::OutputBudget, tool_input::ToolInputError};
use otto_core::model::{EffectCertainty, OperationStopReason};
use otto_core::operation::OperationControl;
use otto_core::provider::{
    Provider, ProviderError, ProviderSettlement, Request, RequestSizer, Response, StreamEvent,
    StreamSink,
};

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
/// The text an API key is replaced with when it is long enough to hold it.
const REDACTED: &str = "[REDACTED]";
/// Largest `GET /models` body read. A larger catalog is an error rather than
/// an unbounded buffer.
const MAX_MODELS_BODY: usize = 8 << 20;

/// Attempts for one completion, shared by the retryable failure classes: a
/// connection that could not be established, a dispatched request whose
/// transport failed, HTTP 429/503, and completed token-truncated tool JSON
/// when a larger known output budget is available. A `Retry-After` value
/// replaces the backoff delay and is capped at `retry_after_cap`.
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
    /// A completed message with invalid tool input; metadata determines
    /// whether a larger output budget can safely retry this generation.
    ToolInput(ToolInputError, bool),
}

/// A usable client: the base URL passed validation and the HTTP client built.
struct Ready {
    base_url: String,
    http: reqwest::Client,
}

/// A client for one Claude endpoint.
///
/// See the module documentation for the ownership, concurrency, cancellation,
/// and error rules.
pub struct Client {
    api_key: String,
    api_version: String,
    workspace_id: String,
    /// `Err` records why the client is unusable. Construction never fails; the
    /// stored message is returned from the first `complete`.
    state: Result<Ready, String>,
    /// Longest time between model-output events on a response body.
    model_idle_timeout: Duration,
}

impl Client {
    fn authentication_headers(&self) -> Result<reqwest::header::HeaderMap, ProviderError> {
        let mut headers = reqwest::header::HeaderMap::new();
        let version = reqwest::header::HeaderValue::from_str(&self.api_version)
            .map_err(|_| ProviderError::Other("invalid Claude API version header".into()))?;
        headers.insert("anthropic-version", version);
        for (name, value) in [
            ("x-api-key", self.api_key.as_str()),
            ("anthropic-workspace-id", self.workspace_id.as_str()),
        ] {
            if value.is_empty() {
                continue;
            }
            let mut value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| ProviderError::Other("invalid Claude authentication header".into()))?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(headers)
    }
    /// Builds a client for `base_url` authenticating with `api_key`.
    ///
    /// Never fails. An invalid base URL, or a TLS stack that refuses to
    /// initialize, is recorded and returned as an error from the first
    /// [`Provider::complete`] call.
    pub fn new(base_url: &str, api_key: &str, api_version: &str, workspace_id: &str) -> Self {
        let state = match default_http_client() {
            Ok(http) => otto_core::config::endpoint::TrustedEndpoint::authorize(
                base_url,
                "claude",
                &[],
                &[],
            )
            .map_err(str::to_string)
            .and_then(|endpoint| Self::ready(endpoint.base_url(), http)),
            Err(error) => Err(format!("build Claude HTTP client: {error}")),
        };
        Self {
            api_key: api_key.to_string(),
            api_version: api_version.to_string(),
            workspace_id: workspace_id.to_string(),
            state,
            model_idle_timeout: MODEL_IDLE_TIMEOUT,
        }
    }

    /// Fails closed: opaque caller HTTP clients cannot prove redirect safety.
    /// Migrate to `new_trusted` with an authorized endpoint and owned transport.
    #[cfg(test)]
    pub fn with_http_client(
        base_url: &str,
        api_key: &str,
        api_version: &str,
        workspace_id: &str,
        http: reqwest::Client,
    ) -> Self {
        Self {
            api_key: api_key.to_string(),
            api_version: api_version.to_string(),
            workspace_id: workspace_id.to_string(),
            state: {
                let _ = (base_url, http);
                Err("injected HTTP clients cannot establish redirect safety; use new or new_trusted".into())
            },
            model_idle_timeout: MODEL_IDLE_TIMEOUT,
        }
    }

    /// Uses an independently authorized endpoint and an owned no-redirect client.
    pub fn new_trusted(
        endpoint: &otto_core::config::endpoint::TrustedEndpoint,
        api_key: &str,
        api_version: &str,
        workspace_id: &str,
    ) -> Self {
        let mut client = Self::new(endpoint.base_url(), api_key, api_version, workspace_id);
        client.state = if endpoint.provider_type() != "claude" {
            Err("endpoint capability has wrong provider type".into())
        } else {
            default_http_client()
                .map_err(|error| error.to_string())
                .and_then(|http| Self::ready(endpoint.base_url(), http))
        };
        client
    }

    /// Rejects every request with `message` before anything is sent; used when
    /// the selected provider has no API key bound, so the process can still
    /// start and the user can bind a key or switch models.
    pub(crate) fn with_missing_credential(mut self, message: &str) -> Self {
        self.state = Err(message.to_string());
        self
    }

    /// Overrides the model-output idle bound; tests use a short one.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_model_idle_timeout(mut self, timeout: Duration) -> Self {
        self.model_idle_timeout = timeout;
        self
    }

    fn ready(base_url: &str, http: reqwest::Client) -> Result<Ready, String> {
        match normalize_base_url(base_url) {
            Some(base_url) => Ok(Ready { base_url, http }),
            None => Err("invalid Claude base URL".to_string()),
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
                http.get(format!("{}/v1/models", ready.base_url))
                    .headers(self.authentication_headers()?)
                    .header("Accept", "application/json"),
            )
            .await?;
            let mut catalog = super::catalog::parse(&bytes, false)?;
            super::catalog::sanitize(&mut catalog, &[&self.api_key, &self.workspace_id]);
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
            .get(format!("{}/v1/models", ready.base_url))
            .headers(self.authentication_headers()?)
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
                ErrorBody::Unreadable => {
                    ProviderError::Other(format!("Claude HTTP {status} (error body unreadable)"))
                }
                ErrorBody::Body(body) => ProviderError::Other(format!(
                    "Claude HTTP {status}: {}",
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
        ids.retain(|id| {
            ![&self.api_key, &self.workspace_id]
                .iter()
                .any(|secret| !secret.is_empty() && id.contains(secret.as_str()))
        });
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
    /// never retried, except completed max_tokens responses with incomplete
    /// tool JSON when the request budget can grow within the model ceiling.
    /// These retries share the same attempt/deadline policy and invalidate
    /// provisional output; malformed complete arguments are never repaired.
    ///
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
            let mut wire = match build_request(request) {
                Ok(wire) => wire,
                Err(error) => {
                    return ProviderSettlement::failed(
                        self.safe_error(error),
                        0,
                        EffectCertainty::NotStarted,
                    );
                }
            };
            let mut payload = match serde_json::to_vec(&wire) {
                Ok(payload) => payload,
                Err(error) => {
                    return ProviderSettlement::failed(
                        self.safe_error(ProviderError::Other(format!(
                            "encode Claude messages request: {error}"
                        ))),
                        0,
                        EffectCertainty::NotStarted,
                    );
                }
            };
            if let Err(error) = check_running(control) {
                return provider_failure(error, 0, EffectCertainty::NotStarted, control);
            }
            let output_budget = OutputBudget::for_model(&request.model);
            // HTTP requests sent.
            let mut attempts = 0;
            // Failed attempts counted against `RETRY_POLICY`.
            let mut failures = 0;
            // Set once any attempt was a dispatched transport interruption.
            let mut interrupted_seen = false;
            let result = loop {
                attempts += 1;
                // Per attempt: only an interruption after output sets it.
                let mut interrupted_output = false;
                let mut generation_completed = false;
                // `dispatched` records whether a retry could repeat provider work,
                // which the settlement must keep as unknown effect certainty.
                let (error, reason, dispatched, retry_after, completed_response) = match self
                    .attempt(ready, &payload, &mut *emit, control, observer)
                    .await
                {
                    Ok(response) => break Ok(response),
                    Err(AttemptError::Sent(error)) => break Err(error),
                    Err(AttemptError::ToolInput(input_error, output_emitted)) => {
                        let error = ProviderError::Other(format!(
                            "{input_error} (max_tokens={})",
                            wire.max_tokens
                        ));
                        let next_budget = input_error
                            .is_token_limit_truncation()
                            .then(|| output_budget.grow(wire.max_tokens))
                            .flatten();
                        let Some(next_budget) = next_budget else {
                            break Err(error);
                        };
                        wire.max_tokens = next_budget;
                        payload = match serde_json::to_vec(&wire) {
                            Ok(payload) => payload,
                            Err(_) => {
                                break Err(ProviderError::Other(
                                    "encode Claude recovery request".into(),
                                ));
                            }
                        };
                        interrupted_output = output_emitted;
                        generation_completed = true;
                        (
                            error,
                            "Claude tool input reached output token limit",
                            interrupted_seen,
                            None,
                            true,
                        )
                    }
                    Err(AttemptError::NotSent(error)) => {
                        (error, CONNECT_RETRY_REASON, false, None, false)
                    }
                    Err(AttemptError::Interrupted(error, reason, output_emitted)) => {
                        interrupted_seen = true;
                        interrupted_output = output_emitted;
                        (error, reason, true, None, false)
                    }
                    // A completed response proves the endpoint answered; only an earlier
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
                    } else if completed_response {
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
                    } else if generation_completed {
                        ProviderSettlement::stopped(
                            error,
                            attempts,
                            EffectCertainty::Completed,
                            control
                                .stop_reason()
                                .unwrap_or(OperationStopReason::UserCancellation),
                        )
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
            ProviderError::Other(format!("encode Claude messages request: {error}"))
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

/// Owned transport: bounded connect/read time and no redirects, including
/// same-origin redirects. Streaming remains bounded by operation control.
fn default_http_client() -> Result<reqwest::Client, reqwest::Error> {
    super::endpoint_dns::http_client_builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(KEEPALIVE)
        .read_timeout(READ_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            attempt.error("provider redirects are disabled")
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
    otto_core::config::endpoint::normalize_base_url(base_url).ok()
}

#[cfg(test)]
mod tests;
