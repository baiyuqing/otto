//! HTTP transport for the ChatGPT backend Responses API.
//!
//! The wire codec lives in [`otto_core::openairesponses`]; this module owns the
//! parts that need the network: the OAuth access token, the connection
//! settings, and credential redaction.
//!
//! Ownership: a [`Client`] owns its base URL, its [`TokenSource`], its account
//! id, and its [`reqwest::Client`]. The request passed to `complete` is
//! borrowed and never retained.
//!
//! Concurrency: `complete` takes `&self`; the token source serializes its own
//! refreshes, so one client can serve a parent agent and its sub-agents.
//!
//! Operation control: the token fetch, request send, and every body read observe
//! the caller's cancellation token. Deadline and user cancellation remain
//! distinct, while a fully received and validated response wins a simultaneous
//! stop.
//!
//! Errors: transport failures use fixed messages with safe retry counts and
//! failure classes, so no endpoint text or credential reaches the caller. A
//! decoder failure carries the decoder's own message with the access token and
//! the account id redacted.
//!
//! Two deliberate decisions:
//!   - transport failures, HTTP 429/500/502/503/504, and stream-reported
//!     `server_is_overloaded` or `server_error` get up to six retries, including
//!     after partial output; other statuses, reported codes and protocol failures
//!     are returned on the first attempt.
//!   - reqwest exposes no cap on the size of a response header block, so no
//!     bound is enforced on it. The same gap exists in
//!     [`crate::provider::openaicompat`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::auth::token::TokenSource;
use futures_util::TryStreamExt;
use otto_core::agent::redactor::{Redactor, StreamRedactor};
use otto_core::model::{EffectCertainty, Message, OperationStopReason};
use otto_core::openairesponses::protocol::{build_request_with, serialized_request_size_with};
use otto_core::openairesponses::stream::{StreamAssembler, StreamError};
use otto_core::operation::OperationControl;
use otto_core::provider::{
    Provider, ProviderCapabilities, ProviderError, ProviderSettlement, RemoteCompactionSupport,
    Request, RequestSizer, Response, StreamEvent, StreamSink,
};

mod attempt;

impl Client {
    fn provider_capabilities() -> ProviderCapabilities {
        ProviderCapabilities {
            remote_compaction: RemoteCompactionSupport::ResponsesV2,
        }
    }
}

/// The ChatGPT backend that serves subscription traffic.
const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// `OpenAI-Beta` and `originator` mirror the Codex CLI.
const BETA_HEADER: &str = "responses=experimental";
const ORIGINATOR: &str = "codex_cli_rs";
/// Codex catalog compatibility version, independent of Otto's release version.
/// Keep this aligned with the upstream catalog protocol Otto supports.
const CATALOG_CLIENT_VERSION: &str = "0.163.0";

/// Longest wait for the TCP connect and TLS handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP keepalive interval.
const KEEPALIVE: Duration = Duration::from_secs(30);
/// Longest wait for a single read from the socket. reqwest has no header-only
/// timeout, so 60 seconds is applied per read instead. Any received byte
/// restarts it, so a body that keeps sending bytes without model output is
/// bounded by [`MODEL_IDLE_TIMEOUT`] instead.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest time between model-output events (text, reasoning or tool-argument
/// deltas) on a 2xx body. Keepalive bytes and SSE comments do not reset it.
const MODEL_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Authorization is unusable, and the user has to sign in again.
const AUTHORIZATION_FAILED: &str = "chatgpt authorization failed; run 'otto login'";
/// The request did not complete, with no detail that could carry a secret.
const REQUEST_FAILED: &str = "chatgpt request failed";
/// `Retry` event reason for transport failures and a clean stream close before
/// any terminal event. In either case the dispatched request may have run.
const TRANSPORT_RETRY_REASON: &str = "connection interrupted";
/// `Retry` event reason for a body that produced no model output within
/// [`MODEL_IDLE_TIMEOUT`].
const MODEL_IDLE_RETRY_REASON: &str = "model output idle timeout";

/// Classifies a Responses stream failure for the bounded transport retry.
/// Partial output is provisional; the retry event marks its replacement.
fn stream_retry(error: &StreamError) -> Option<Retry> {
    let reason = match error {
        StreamError::Reported { code, .. } => match code.as_str() {
            "server_is_overloaded" => "server_is_overloaded",
            "server_error" => "server_error",
            _ => return None,
        },
        StreamError::MissingCompletion => TRANSPORT_RETRY_REASON,
        StreamError::PolicyCapacityUnavailable => return None,
        _ => return None,
    };

    Some(Retry {
        reason,
        retry_after: None,
    })
}

async fn policy_capacity_error(
    response: reqwest::Response,
    control: &dyn OperationControl,
) -> Result<bool, ProviderError> {
    const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_ERROR_BODY_BYTES)
    {
        return Ok(false);
    }
    let mut body = Vec::new();
    let mut stream = Box::pin(response.bytes_stream());
    loop {
        match await_control(control, stream.try_next()).await {
            Ok(Ok(Some(chunk))) => {
                if body.len().saturating_add(chunk.len()) > MAX_ERROR_BODY_BYTES {
                    return Ok(false);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(Ok(None)) => break,
            Ok(Err(_)) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .is_some_and(|value| {
            value.pointer("/error/code").and_then(|code| code.as_str())
                == Some("policy_capacity_unavailable")
                || value.get("code").and_then(|code| code.as_str())
                    == Some("policy_capacity_unavailable")
        }))
}

#[derive(Clone, Copy)]
struct Retry {
    reason: &'static str,
    retry_after: Option<Duration>,
}

const RETRY_POLICY: crate::retry::Policy = crate::retry::Policy {
    max_attempts: 7,
    base: Duration::from_secs(1),
    max: Duration::from_secs(30),
    retry_after_cap: Duration::from_secs(30),
};

// Fixed exponential backoff; no new randomness dependency for six delays.
struct Backoff;
impl crate::retry::JitterSource for Backoff {
    fn full_jitter(&mut self, upper_bound: Duration) -> Duration {
        upper_bound
    }
}

/// A provider backed by a ChatGPT subscription.
///
/// See the module documentation for the ownership, concurrency, cancellation,
/// and error rules.
pub struct Client {
    base_url: String,
    tokens: TokenSource,
    account_id: String,
    /// Advertise the backend-hosted `web_search` tool with every request.
    hosted_web_search: bool,
    /// `Err` records why the HTTP client could not be built. Construction never
    /// fails; the message becomes the first `complete` failure.
    http: Result<reqwest::Client, String>,
    /// Longest time between model-output events on a response body.
    model_idle_timeout: Duration,
    catalog_cache_path: Option<PathBuf>,
    catalog_cache_lock: tokio::sync::Mutex<()>,
}

impl Client {
    /// A client for the production backend, authorized by `tokens`.
    /// `account_id` is the `chatgpt_account_id` sent with every request.
    pub fn new(tokens: TokenSource, account_id: &str) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, tokens, account_id)
    }

    /// Fetch the subscription catalog on demand, using the same native token source.
    /// Wire contract: openai/codex@8c076942cb70fba8e5c4732c23f1faabc4aed484,
    /// codex-rs/codex-api/src/endpoint/models.rs and protocol/src/openai_models.rs.
    pub async fn catalog(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<super::catalog::ModelCatalog, ProviderError> {
        if let Err(error) = &self.http {
            return Err(super::catalog::failure(error));
        }
        super::catalog::controlled(cancel, async {
            let credentials = self.tokens.token(cancel).await.map_err(|error| {
                if matches!(error, crate::auth::AuthError::Cancelled) {
                    ProviderError::Cancelled
                } else {
                    super::catalog::failure(AUTHORIZATION_FAILED)
                }
            })?;
            if credentials.access_token.trim().is_empty() || self.account_id.trim().is_empty() {
                return Err(super::catalog::failure(AUTHORIZATION_FAILED));
            }
            let redactor =
                Redactor::new(&[credentials.access_token.clone(), self.account_id.clone()]);
            if !redactor.allows_dynamic_content() {
                return Err(super::catalog::failure(REQUEST_FAILED));
            }
            let _guard = self.catalog_cache_lock.lock().await;
            let identity = super::catalog_cache::identity(
                &self.base_url,
                &self.account_id,
                CATALOG_CLIENT_VERSION,
            );
            if let Some(path) = &self.catalog_cache_path
                && let Some(mut catalog) = super::catalog_cache::load(path, &identity)
            {
                super::catalog::sanitize(
                    &mut catalog,
                    &[
                        &credentials.access_token,
                        &credentials.refresh_token,
                        &self.account_id,
                    ],
                );
                return Ok(catalog);
            }
            let http = super::catalog::http()?;
            let bytes = super::catalog::read(
                http.get(format!("{}/models", self.base_url))
                    .query(&[("client_version", CATALOG_CLIENT_VERSION)])
                    .bearer_auth(&credentials.access_token)
                    .header("chatgpt-account-id", &self.account_id)
                    .header("OpenAI-Beta", BETA_HEADER)
                    .header("originator", ORIGINATOR)
                    .header("Accept", "application/json"),
            )
            .await?;
            let mut catalog = super::catalog::parse(&bytes, true)?;
            super::catalog::sanitize(
                &mut catalog,
                &[
                    &credentials.access_token,
                    &credentials.refresh_token,
                    &self.account_id,
                ],
            );
            if let Some(path) = &self.catalog_cache_path {
                super::catalog_cache::store(path, identity, &catalog);
            }
            Ok(catalog)
        })
        .await
    }

    /// Whether requests also advertise the backend-hosted `web_search` tool.
    /// The backend runs the search itself; Otto sees no tool call for it.
    pub(crate) fn with_hosted_web_search(mut self, enabled: bool) -> Self {
        self.hosted_web_search = enabled;
        self
    }

    /// Shortens socket reads in offline fault tests without accepting an injected client.
    /// Existing endpoint authorization errors and the owned redirect policy are preserved.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_test_read_timeout(mut self, timeout: Duration) -> Self {
        if self.http.is_ok() {
            self.http = http_client_with_read_timeout(timeout).map_err(|error| error.to_string());
        }
        self
    }

    /// Overrides the model-output idle bound; tests use a short one.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_model_idle_timeout(mut self, timeout: Duration) -> Self {
        self.model_idle_timeout = timeout;
        self
    }

    /// Persist successful model catalogs under the caller's private Otto home.
    pub(crate) fn with_catalog_cache(mut self, path: PathBuf) -> Self {
        self.catalog_cache_path = Some(path);
        self
    }

    /// A client for `base_url`. One trailing `/` is trimmed so the path is
    /// never doubled.
    pub(crate) fn with_base_url(base_url: &str, tokens: TokenSource, account_id: &str) -> Self {
        Self::with_client_result(
            base_url,
            tokens,
            account_id,
            default_http_client().map_err(|error| error.to_string()),
        )
    }

    /// Uses an independently authorized endpoint with an owned no-redirect client.
    pub fn new_trusted(
        endpoint: &otto_core::config::endpoint::TrustedEndpoint,
        tokens: TokenSource,
        account_id: &str,
    ) -> Self {
        let mut client = Self::with_base_url(endpoint.base_url(), tokens, account_id);
        client.http = if endpoint.provider_type() != "chatgpt" {
            Err("endpoint capability has wrong provider type".into())
        } else {
            default_http_client().map_err(|error| error.to_string())
        };
        client
    }

    /// Fails closed: injected clients cannot establish redirect safety.
    pub fn with_http_client(
        base_url: &str,
        tokens: TokenSource,
        account_id: &str,
        http: reqwest::Client,
    ) -> Self {
        Self::with_client_result(base_url, tokens, account_id, {
            let _ = http;
            Err("injected HTTP clients cannot establish redirect safety".into())
        })
    }

    fn with_client_result(
        base_url: &str,
        tokens: TokenSource,
        account_id: &str,
        http: Result<reqwest::Client, String>,
    ) -> Self {
        let http =
            otto_core::config::endpoint::TrustedEndpoint::authorize(base_url, "chatgpt", &[], &[])
                .map_err(str::to_string)
                .and(http);
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            tokens,
            account_id: account_id.to_owned(),
            hosted_web_search: false,
            http,
            model_idle_timeout: MODEL_IDLE_TIMEOUT,
            catalog_cache_path: None,
            catalog_cache_lock: tokio::sync::Mutex::new(()),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Client {
    fn capabilities(&self) -> ProviderCapabilities {
        Self::provider_capabilities()
    }

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
            let mut attempts = 0;
            let mut auth_recovery_used = false;
            loop {
                let mut output_started = false;
                let mut retry_reason = None;
                let mut rejected_access_token = None;
                let mut settlement = self
                    .attempt(
                        request,
                        &mut |event| {
                            output_started = true;
                            emit(event);
                        },
                        control,
                        &mut retry_reason,
                        &mut rejected_access_token,
                        observer,
                    )
                    .await;
                if attempts > 0 && settlement.result.is_err() {
                    settlement.outcome.effect_certainty = EffectCertainty::Unknown;
                }
                attempts += settlement.attempts;
                settlement.attempts = attempts;
                if let Some(rejected_access_token) = rejected_access_token {
                    if auth_recovery_used {
                        return authorization_failure(attempts);
                    }
                    auth_recovery_used = true;
                    let refreshed = match await_control(
                        control,
                        self.tokens
                            .refresh_rejected(&rejected_access_token, control.cancellation_token()),
                    )
                    .await
                    {
                        Ok(Ok(credentials)) => credentials,
                        Ok(Err(_)) => {
                            if let Err(error) = check_running(control) {
                                return chatgpt_failure(
                                    error,
                                    attempts,
                                    EffectCertainty::Unknown,
                                    control,
                                );
                            }
                            return authorization_failure(attempts);
                        }
                        Err(error) => {
                            return chatgpt_failure(
                                error,
                                attempts,
                                EffectCertainty::Unknown,
                                control,
                            );
                        }
                    };
                    if refreshed.access_token.trim().is_empty()
                        || refreshed.account_id != self.account_id
                    {
                        return authorization_failure(attempts);
                    }
                    continue;
                }
                // Retry only this completion. A partial stream never authorizes
                // tools; previously completed tools remain in the borrowed request.
                // The retry event separates provisional output from its replacement.
                let Some(retry) = retry_reason else {
                    return settlement;
                };
                if let Err(error) = check_running(control) {
                    return chatgpt_failure(error, attempts, EffectCertainty::Unknown, control);
                }
                let Some(delay) = crate::retry::next_delay(
                    RETRY_POLICY,
                    attempts,
                    retry.retry_after,
                    control.remaining().unwrap_or(Duration::MAX),
                    Duration::from_secs(1),
                    &mut Backoff,
                ) else {
                    if let Err(ProviderError::Other(message)) = &mut settlement.result {
                        *message = format!(
                            "{message} ({}; {attempts} attempts; retry budget exhausted)",
                            retry.reason
                        );
                    }
                    return settlement;
                };
                emit(StreamEvent::Retry {
                    attempt: attempts + 1,
                    max_attempts: RETRY_POLICY.max_attempts,
                    delay,
                    reason: retry.reason.to_owned(),
                    interrupted_output: output_started,
                });
                if let Err(error) = await_control(control, tokio::time::sleep(delay)).await {
                    return chatgpt_failure(error, attempts, EffectCertainty::Unknown, control);
                }
            }
        })
    }
}

fn authorization_failure(attempts: u32) -> ProviderSettlement {
    ProviderSettlement::failed(
        ProviderError::Other(AUTHORIZATION_FAILED.to_owned()),
        attempts,
        if attempts == 0 {
            EffectCertainty::NotStarted
        } else {
            EffectCertainty::Unknown
        },
    )
}

fn transient_http_retry(status: u16, headers: &reqwest::header::HeaderMap) -> Option<Retry> {
    let reason = match status {
        429 => "HTTP 429",
        500 => "HTTP 500",
        502 => "HTTP 502",
        503 => "HTTP 503",
        504 => "HTTP 504",
        _ => return None,
    };
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(crate::retry::parse_retry_after_seconds);
    Some(Retry {
        reason,
        retry_after,
    })
}

impl RequestSizer for Client {
    /// The exact number of bytes the request occupies on the wire.
    fn serialized_request_size(&self, request: &Request) -> Result<usize, ProviderError> {
        serialized_request_size_with(request, self.hosted_web_search)
            .map_err(|error| ProviderError::Other(format!("encode responses request: {error}")))
    }
}

/// Returns `message` with every text-bearing field redacted.
fn redact_message(redactor: &Redactor, mut message: Message) -> Message {
    message.id = redactor.redact_string(&message.id);
    message.context_type = redactor.redact_string(&message.context_type);
    for block in &mut message.blocks {
        block.text = redactor.redact_string(&block.text);
        block.tool_call_id = redactor.redact_string(&block.tool_call_id);
        block.tool_name = redactor.redact_string(&block.tool_name);
        if let Some(arguments) = &block.arguments {
            block.arguments = Some(redactor.redact_json_strings(arguments.get()));
        }
    }
    message
}

/// One tool call's streamed arguments, and the redacted identifiers to repeat
/// when its held-back tail is flushed.
struct ToolState<'a> {
    tool_call_id: String,
    tool_name: String,
    arguments: StreamRedactor<'a>,
}

/// Redacts stream events on their way to the caller's sink.
///
/// Text and each tool call's arguments carry their own [`StreamRedactor`], so a
/// secret split across two deltas is still caught: the tail that could still
/// become a secret is held back until the next delta or until [`Self::flush`].
struct EventRedactor<'a> {
    redactor: &'a Redactor,
    reasoning: StreamRedactor<'a>,
    text: StreamRedactor<'a>,
    tools: HashMap<String, ToolState<'a>>,
    /// Insertion order of `tools`, so the flush order is deterministic.
    order: Vec<String>,
}

impl<'a> EventRedactor<'a> {
    fn new(redactor: &'a Redactor) -> Self {
        Self {
            redactor,
            reasoning: redactor.new_stream(),
            text: redactor.new_stream(),
            tools: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// Forwards `event` with every field redacted. An event whose fields are
    /// all empty afterwards is dropped.
    fn emit(&mut self, event: StreamEvent, sink: StreamSink<'_>) {
        match event {
            // Carries no provider text, so there is nothing to redact.
            retry @ StreamEvent::Retry { .. } => sink(retry),
            StreamEvent::ReasoningDelta { text } => {
                let text = self.reasoning.write(&text);
                if !text.is_empty() {
                    sink(StreamEvent::ReasoningDelta { text });
                }
            }
            StreamEvent::TextDelta { text } => {
                // Reasoning precedes text; release its held tail first.
                let held = self.reasoning.flush();
                if !held.is_empty() {
                    sink(StreamEvent::ReasoningDelta { text: held });
                }
                let text = self.text.write(&text);
                if !text.is_empty() {
                    sink(StreamEvent::TextDelta { text });
                }
            }
            StreamEvent::ToolCallDelta {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                let id = self.redactor.redact_string(&tool_call_id);
                let name = self.redactor.redact_string(&tool_name);
                let mut redacted = String::new();
                if !arguments.is_empty() {
                    let redactor = self.redactor;
                    let key = format!("{tool_call_id}\u{0}{tool_name}");
                    let state = self.tools.entry(key.clone()).or_insert_with(|| {
                        self.order.push(key);
                        ToolState {
                            tool_call_id: String::new(),
                            tool_name: String::new(),
                            arguments: redactor.new_stream(),
                        }
                    });
                    state.tool_call_id = id.clone();
                    state.tool_name = name.clone();
                    redacted = state.arguments.write(&arguments);
                }
                if id.is_empty() && name.is_empty() && redacted.is_empty() {
                    return;
                }
                sink(StreamEvent::ToolCallDelta {
                    tool_call_id: id,
                    tool_name: name,
                    arguments: redacted,
                });
            }
        }
    }

    /// Emits whatever every stream held back.
    fn flush(&mut self, sink: StreamSink<'_>) {
        let reasoning = self.reasoning.flush();
        if !reasoning.is_empty() {
            sink(StreamEvent::ReasoningDelta { text: reasoning });
        }
        let text = self.text.flush();
        if !text.is_empty() {
            sink(StreamEvent::TextDelta { text });
        }
        for key in &self.order {
            let Some(state) = self.tools.get_mut(key) else {
                continue;
            };
            let arguments = state.arguments.flush();
            if !arguments.is_empty() {
                sink(StreamEvent::ToolCallDelta {
                    tool_call_id: state.tool_call_id.clone(),
                    tool_name: state.tool_name.clone(),
                    arguments,
                });
            }
        }
    }
}

fn chatgpt_failure(
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
    future: impl std::future::Future<Output = T>,
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

/// The hardened client. Every redirect is refused rather than followed, so the
/// `Authorization` header and the request body never reach another origin;
/// reqwest returns the 3xx response itself, which becomes the status-only
/// error.
///
/// There is deliberately no overall request timeout: a streaming completion may
/// run for minutes and is bounded by the caller's cancellation token.
fn default_http_client() -> Result<reqwest::Client, reqwest::Error> {
    http_client_with_read_timeout(READ_TIMEOUT)
}

fn http_client_with_read_timeout(timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
    super::endpoint_dns::http_client_builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(KEEPALIVE)
        .read_timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

#[cfg(test)]
mod tests;
