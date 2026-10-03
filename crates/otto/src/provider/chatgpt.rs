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
//! Errors: every failure outside the stream decoder is one of three fixed
//! strings, so no endpoint text and no credential can reach the caller. A
//! decoder failure carries the decoder's own message with the access token and
//! the account id redacted.
//!
//! Two deliberate decisions:
//!   - transport failures before any streamed output get up to three retries;
//!     HTTP status and protocol failures are returned on the first attempt.
//!   - reqwest exposes no cap on the size of a response header block, so no
//!     bound is enforced on it. The same gap exists in
//!     [`crate::provider::openaicompat`].

use std::collections::HashMap;
use std::time::Duration;

use crate::auth::token::TokenSource;
use futures_util::TryStreamExt;
use otto_core::agent::redactor::{Redactor, StreamRedactor};
use otto_core::model::{EffectCertainty, Message, OperationStopReason};
use otto_core::openairesponses::protocol::{build_request, serialized_request_size};
use otto_core::openairesponses::stream::StreamAssembler;
use otto_core::operation::OperationControl;
use otto_core::provider::{
    Provider, ProviderError, ProviderSettlement, Request, RequestSizer, Response, StreamEvent,
    StreamSink,
};

/// The ChatGPT backend that serves subscription traffic.
const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// `OpenAI-Beta` and `originator` mirror the Codex CLI.
const BETA_HEADER: &str = "responses=experimental";
const ORIGINATOR: &str = "codex_cli_rs";

/// Longest wait for the TCP connect and TLS handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP keepalive interval.
const KEEPALIVE: Duration = Duration::from_secs(30);
/// Longest wait for a single read from the socket. reqwest has no header-only
/// timeout, so 60 seconds is applied per read instead, which also bounds a
/// stream that stalls mid-body.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Authorization is unusable, and the user has to sign in again.
const AUTHORIZATION_FAILED: &str = "chatgpt authorization failed; run 'otto login'";
/// The request did not complete, with no detail that could carry a secret.
const REQUEST_FAILED: &str = "chatgpt request failed";
const RETRY_POLICY: crate::retry::Policy = crate::retry::Policy {
    max_attempts: 4,
    base: Duration::from_secs(1),
    max: Duration::from_secs(4),
    retry_after_cap: Duration::from_secs(4),
};

// Fixed exponential backoff; no new randomness dependency for three delays.
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
    /// `Err` records why the HTTP client could not be built. Construction never
    /// fails; the message becomes the first `complete` failure.
    http: Result<reqwest::Client, String>,
}

impl Client {
    /// A client for the production backend, authorized by `tokens`.
    /// `account_id` is the `chatgpt_account_id` sent with every request.
    pub fn new(tokens: TokenSource, account_id: &str) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, tokens, account_id)
    }

    /// A client for `base_url`. One trailing `/` is trimmed so the path is
    /// never doubled.
    pub fn with_base_url(base_url: &str, tokens: TokenSource, account_id: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            tokens,
            account_id: account_id.to_owned(),
            http: default_http_client().map_err(|error| error.to_string()),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Client {
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        let mut attempts = 0;
        loop {
            let mut output_started = false;
            let mut retryable = false;
            let mut settlement = self
                .attempt(
                    request,
                    &mut |event| {
                        output_started = true;
                        emit(event);
                    },
                    control,
                    &mut retryable,
                )
                .await;
            if attempts > 0 && settlement.result.is_err() {
                settlement.outcome.effect_certainty = EffectCertainty::Unknown;
            }
            attempts += settlement.attempts;
            settlement.attempts = attempts;
            // Retry this completion only. Previously executed tools stay in
            // the unchanged request; streamed output cannot be replayed.
            if !retryable || output_started {
                return settlement;
            }
            if let Err(error) = check_running(control) {
                return chatgpt_failure(error, attempts, EffectCertainty::Unknown, control);
            }
            let Some(delay) = crate::retry::next_delay(
                RETRY_POLICY,
                attempts,
                None,
                control.remaining().unwrap_or(Duration::MAX),
                Duration::from_secs(1),
                &mut Backoff,
            ) else {
                return settlement;
            };
            emit(StreamEvent::Retry {
                attempt: attempts + 1,
                max_attempts: RETRY_POLICY.max_attempts,
                delay,
                reason: "connection interrupted".to_owned(),
            });
            if let Err(error) = await_control(control, tokio::time::sleep(delay)).await {
                return chatgpt_failure(error, attempts, EffectCertainty::Unknown, control);
            }
        }
    }
}

impl Client {
    /// One request attempt. Only transport failures are eligible for retry.
    async fn attempt(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
        retryable: &mut bool,
    ) -> ProviderSettlement {
        if let Err(error) = check_running(control) {
            return chatgpt_failure(error, 0, EffectCertainty::NotStarted, control);
        }
        let http = match &self.http {
            Ok(http) => http,
            Err(error) => {
                return ProviderSettlement::failed(
                    ProviderError::Other(error.clone()),
                    0,
                    EffectCertainty::NotStarted,
                );
            }
        };
        let credentials =
            match await_control(control, self.tokens.token(control.cancellation_token())).await {
                Ok(Ok(credentials)) => credentials,
                Ok(Err(_)) => {
                    if let Err(error) = check_running(control) {
                        return chatgpt_failure(error, 0, EffectCertainty::NotStarted, control);
                    }
                    return ProviderSettlement::failed(
                        ProviderError::Other(AUTHORIZATION_FAILED.to_owned()),
                        0,
                        EffectCertainty::NotStarted,
                    );
                }
                Err(error) => {
                    return chatgpt_failure(error, 0, EffectCertainty::NotStarted, control);
                }
            };
        let access_token = credentials.access_token;
        if access_token.trim().is_empty() || self.account_id.trim().is_empty() {
            return ProviderSettlement::failed(
                ProviderError::Other(AUTHORIZATION_FAILED.to_owned()),
                0,
                EffectCertainty::NotStarted,
            );
        }

        let redactor = Redactor::new(&[access_token.clone(), self.account_id.clone()]);
        if !redactor.allows_dynamic_content() {
            return ProviderSettlement::failed(
                ProviderError::Other(REQUEST_FAILED.to_owned()),
                0,
                EffectCertainty::NotStarted,
            );
        }

        let payload = match serde_json::to_vec(&build_request(request)) {
            Ok(payload) => payload,
            Err(error) => {
                return ProviderSettlement::failed(
                    ProviderError::Other(format!("encode responses request: {error}")),
                    0,
                    EffectCertainty::NotStarted,
                );
            }
        };
        if let Err(error) = check_running(control) {
            return chatgpt_failure(error, 0, EffectCertainty::NotStarted, control);
        }

        let send = http
            .post(format!("{}/responses", self.base_url))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("chatgpt-account-id", &self.account_id)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("OpenAI-Beta", BETA_HEADER)
            .header("originator", ORIGINATOR)
            .body(payload)
            .send();
        let response = match await_control(control, send).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                *retryable = !error.is_builder() && !error.is_redirect();
                if let Err(error) = check_running(control) {
                    return chatgpt_failure(error, 1, EffectCertainty::Unknown, control);
                }
                return ProviderSettlement::failed(
                    ProviderError::Other(REQUEST_FAILED.to_owned()),
                    1,
                    EffectCertainty::Unknown,
                );
            }
            Err(error) => return chatgpt_failure(error, 1, EffectCertainty::Unknown, control),
        };

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return ProviderSettlement::failed(
                ProviderError::Other(format!("chatgpt responses HTTP {status}")),
                1,
                EffectCertainty::Completed,
            );
        }

        let mut events = EventRedactor::new(&redactor);
        let mut assembler = StreamAssembler::new();
        let mut body = Box::pin(response.bytes_stream());
        loop {
            let chunk = match await_control(control, body.try_next()).await {
                Ok(Ok(Some(chunk))) => chunk,
                Ok(Ok(None)) => break,
                Ok(Err(error)) => {
                    *retryable = !error.is_builder() && !error.is_redirect();
                    if let Err(error) = check_running(control) {
                        return chatgpt_failure(error, 1, EffectCertainty::Unknown, control);
                    }
                    return ProviderSettlement::failed(
                        ProviderError::Other(REQUEST_FAILED.to_owned()),
                        1,
                        EffectCertainty::Unknown,
                    );
                }
                Err(error) => {
                    return chatgpt_failure(error, 1, EffectCertainty::Unknown, control);
                }
            };
            if let Err(error) = assembler.push(&chunk, &mut |event| events.emit(event, &mut *emit))
            {
                if let Err(stop) = check_running(control) {
                    return chatgpt_failure(stop, 1, EffectCertainty::Unknown, control);
                }
                return ProviderSettlement::failed(
                    ProviderError::Other(redactor.redact_string(&error.to_string())),
                    1,
                    EffectCertainty::Completed,
                );
            }
            if assembler.is_done() {
                let response = match assembler.finish(&mut |event| events.emit(event, &mut *emit)) {
                    Ok(response) => response,
                    Err(error) => {
                        if let Err(stop) = check_running(control) {
                            return chatgpt_failure(stop, 1, EffectCertainty::Unknown, control);
                        }
                        return ProviderSettlement::failed(
                            ProviderError::Other(redactor.redact_string(&error.to_string())),
                            1,
                            EffectCertainty::Completed,
                        );
                    }
                };
                events.flush(&mut *emit);
                return ProviderSettlement::succeeded(
                    Response {
                        message: redact_message(&redactor, response.message),
                    },
                    1,
                );
            }
            if let Err(error) = check_running(control) {
                return chatgpt_failure(error, 1, EffectCertainty::Unknown, control);
            }
        }

        let response = match assembler.finish(&mut |event| events.emit(event, &mut *emit)) {
            Ok(response) => response,
            Err(error) => {
                if let Err(stop) = check_running(control) {
                    return chatgpt_failure(stop, 1, EffectCertainty::Unknown, control);
                }
                return ProviderSettlement::failed(
                    ProviderError::Other(redactor.redact_string(&error.to_string())),
                    1,
                    EffectCertainty::Completed,
                );
            }
        };
        events.flush(&mut *emit);
        ProviderSettlement::succeeded(
            Response {
                message: redact_message(&redactor, response.message),
            },
            1,
        )
    }
}

impl RequestSizer for Client {
    /// The exact number of bytes the request occupies on the wire.
    fn serialized_request_size(&self, request: &Request) -> Result<usize, ProviderError> {
        serialized_request_size(request)
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
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(KEEPALIVE)
        .read_timeout(READ_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Credentials;
    use crate::auth::testserver::{self, sse_response};
    use otto_core::model::{Block, BlockType, FinishReason, Message, Role, ToolDefinition, Usage};
    use otto_core::provider::StreamEvent;
    use tokio_util::sync::CancellationToken;

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

    const CANNED_STREAM: &str = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n",
        "\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_abc\",\"name\":\"get_time\"}}\n",
        "\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{\\\"tz\\\":\"}\n",
        "\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"\\\"utc\\\"}\"}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n",
        "\n",
    );

    /// A token source that never refreshes: the zero expiry never expires, so
    /// no endpoint is contacted.
    fn static_tokens(access_token: &str) -> TokenSource {
        TokenSource::new(
            Credentials {
                access_token: access_token.to_owned(),
                ..Credentials::default()
            },
            std::path::PathBuf::from("/nonexistent/chatgpt.json"),
            CancellationToken::new(),
        )
    }

    fn model_request() -> Request {
        Request {
            model: "gpt-5-codex".to_owned(),
            system_prompt: "sys".to_owned(),
            messages: vec![Message {
                role: Role::User,
                blocks: vec![Block {
                    block_type: BlockType::Text,
                    text: "hi".to_owned(),
                    ..Block::default()
                }],
                ..Message::default()
            }],
            tools: vec![ToolDefinition {
                name: "get_time".to_owned(),
                description: "get the time".to_owned(),
                parameters: serde_json::value::RawValue::from_string(
                    r#"{"type":"object"}"#.to_owned(),
                )
                .ok(),
            }],
            ..Request::default()
        }
    }

    async fn complete(
        client: &Client,
        request: &Request,
    ) -> (otto_core::provider::ProviderSettlement, Vec<StreamEvent>) {
        let mut events = Vec::new();
        let result = {
            let mut sink = |event: StreamEvent| events.push(event);
            client
                .complete(request, &mut sink, &CancellationToken::new())
                .await
        };
        (result, events)
    }

    /// Text and tool call are parsed, and the four request headers are
    /// asserted.
    #[tokio::test]
    async fn sends_the_expected_request_and_parses_text_and_a_tool_call() {
        let server = testserver::spawn(|_| sse_response(CANNED_STREAM)).await;
        let client = Client::with_base_url(&server.url, static_tokens("test-token"), "acct-1");

        let (settlement, events) = complete(&client, &model_request()).await;
        let response = settlement.result.unwrap();

        let sent = server.requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].target, "/responses");
        assert_eq!(sent[0].header("Authorization"), "Bearer test-token");
        assert_eq!(sent[0].header("chatgpt-account-id"), "acct-1");
        assert_eq!(sent[0].header("Content-Type"), "application/json");
        assert_eq!(sent[0].header("Accept"), "text/event-stream");
        assert_eq!(sent[0].header("OpenAI-Beta"), "responses=experimental");
        assert_eq!(sent[0].header("originator"), "codex_cli_rs");

        let body: serde_json::Value = serde_json::from_str(&sent[0].body).unwrap();
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "get_time");

        assert_eq!(response.message.blocks.len(), 2);
        assert_eq!(response.message.blocks[0].block_type, BlockType::Text);
        assert_eq!(response.message.blocks[0].text, "Hello world");
        let call = &response.message.blocks[1];
        assert_eq!(call.block_type, BlockType::ToolCall);
        assert_eq!(call.tool_call_id, "call_abc");
        assert_eq!(call.tool_name, "get_time");
        assert_eq!(
            call.arguments.as_ref().map(|raw| raw.get()),
            Some(r#"{"tz":"utc"}"#)
        );
        assert_eq!(
            response.message.finish_reason,
            Some(FinishReason::ToolCalls)
        );
        assert_eq!(
            response.message.usage,
            Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 2,
            })
        );
        assert!(!events.is_empty());
    }

    #[tokio::test]
    async fn preserves_usage_presence() {
        for (name, body, want) in [
            (
                "missing",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                None,
            ),
            (
                "explicit zero",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n",
                Some(Usage::default()),
            ),
        ] {
            let server = testserver::spawn(move |_| sse_response(body)).await;
            let client = Client::with_base_url(&server.url, static_tokens("test-token"), "acct-1");
            let (settlement, _) = complete(&client, &Request::default()).await;
            assert_eq!(settlement.result.unwrap().message.usage, want, "{name}");
        }
    }

    #[tokio::test]
    async fn a_rotated_token_and_account_id_are_redacted_from_the_stream_and_the_response() {
        let access_token = format!("rotated-token-{}", "a".repeat(1694));
        let account_id = "acct-rotated-456";
        let marker = redaction_marker(&[access_token.clone(), account_id.to_owned()]);
        let stream = [
            "event: response.output_text.delta".to_owned(),
            format!(
                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"before {}\"}}",
                &access_token[..8]
            ),
            String::new(),
            "event: response.output_text.delta".to_owned(),
            format!(
                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{} after {}\"}}",
                &access_token[8..],
                &account_id[..7]
            ),
            String::new(),
            "event: response.output_text.delta".to_owned(),
            format!(
                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{} done\"}}",
                &account_id[7..]
            ),
            String::new(),
            "event: response.output_item.added".to_owned(),
            format!(
                "data: {{\"type\":\"response.output_item.added\",\"item\":{{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call-{access_token}\",\"name\":\"tool-{account_id}\"}}}}"
            ),
            String::new(),
            "event: response.function_call_arguments.delta".to_owned(),
            format!(
                "data: {{\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{{\\\"token\\\":\\\"{}\"}}",
                &access_token[..8]
            ),
            String::new(),
            "event: response.function_call_arguments.delta".to_owned(),
            format!(
                "data: {{\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{}\\\",\\\"account\\\":\\\"{}\"}}",
                &access_token[8..],
                &account_id[..7]
            ),
            String::new(),
            "event: response.function_call_arguments.delta".to_owned(),
            format!(
                "data: {{\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{}\\\"}}\"}}",
                &account_id[7..]
            ),
            String::new(),
            "event: response.completed".to_owned(),
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}".to_owned(),
            String::new(),
        ]
        .join("\n");
        let server = testserver::spawn(move |_| sse_response(&stream)).await;
        let client = Client::with_base_url(&server.url, static_tokens(&access_token), account_id);

        let (settlement, events) = complete(&client, &Request::default()).await;
        let response = settlement.result.unwrap();
        assert!(!events.is_empty());

        let call = &response.message.blocks[1];
        let arguments = call.arguments.as_ref().unwrap().get().to_owned();
        for secret in [access_token.as_str(), account_id] {
            for event in &events {
                let rendered = format!("{event:?}");
                assert!(!rendered.contains(secret), "event leaked: {rendered}");
            }
            assert!(!response.message.text().contains(secret));
            assert!(!response.message.id.contains(secret));
            assert!(!call.tool_call_id.contains(secret));
            assert!(!call.tool_name.contains(secret));
            assert!(!arguments.contains(secret));
        }
        let visible = format!(
            "{}\n{}\n{}\n{}",
            response.message.text(),
            call.tool_call_id,
            call.tool_name,
            arguments
        );
        assert!(visible.contains(&marker), "no marker in {visible}");
    }

    #[tokio::test]
    async fn a_credential_split_across_reasoning_deltas_is_redacted() {
        let account_id = "acct-reasoning-789";
        let marker = redaction_marker(&["token".to_owned(), account_id.to_owned()]);
        let delta = |text: &str| {
            format!(
                "event: response.reasoning_summary_text.delta\ndata: {{\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"summary_index\":0,\"delta\":\"{text}\"}}\n"
            )
        };
        let stream = [
            delta(&format!("see {}", &account_id[..6])),
            delta(&format!("{} now", &account_id[6..])),
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n".to_owned(),
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n".to_owned(),
        ]
        .join("\n");
        let server = testserver::spawn(move |_| sse_response(&stream)).await;
        let client = Client::with_base_url(&server.url, static_tokens("token"), account_id);

        let (settlement, events) = complete(&client, &Request::default()).await;
        let response = settlement.result.unwrap();

        let reasoning: String = events
            .iter()
            .map_while(|event| match event {
                StreamEvent::ReasoningDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let expected = format!("see {marker} now");
        assert_eq!(reasoning, expected);
        assert!(matches!(events.last(), Some(StreamEvent::TextDelta { text }) if text == "ok"));
        assert_eq!(response.message.blocks[0], Block::reasoning(expected));
    }

    /// An HTTP error names only the status, so no part of the body can reach
    /// it.
    #[tokio::test]
    async fn a_non_2xx_response_reports_only_the_status() {
        let access_token = "secret-abc";
        let account_id = "acct-secret";
        let body = format!(
            "{}{access_token} account {account_id}",
            "x".repeat(32 << 10)
        );
        let server = testserver::spawn(move |_| testserver::status_response(401, &body)).await;
        let client = Client::with_base_url(&server.url, static_tokens(access_token), account_id);

        let (settlement, _) = complete(&client, &Request::default()).await;
        let error = settlement.result.unwrap_err();
        assert_eq!(error.to_string(), "chatgpt responses HTTP 401");
    }

    #[tokio::test]
    async fn a_redirect_is_reported_as_its_status_without_forwarding_the_request() {
        let target = testserver::spawn(|_| testserver::status_response(500, "unreachable")).await;
        let location = format!("{}/responses", target.url);
        let source =
            testserver::spawn(move |_| testserver::redirect_response(307, &location)).await;
        let client = Client::with_base_url(
            &source.url,
            static_tokens("redirect-access-token"),
            "redirect-account-id",
        );

        let (settlement, _) = complete(&client, &model_request()).await;
        assert_eq!(
            settlement.result.unwrap_err().to_string(),
            "chatgpt responses HTTP 307"
        );
        assert_eq!(source.count(), 1);
        assert_eq!(target.count(), 0);
    }

    #[tokio::test]
    async fn an_unusable_token_source_reports_a_fixed_authorization_error() {
        // A refresh that cannot reach its endpoint, and a token source that
        // holds no access token, are the two ways the token source fails.
        let unreachable = crate::auth::oauth::Endpoint {
            authorize_url: "http://127.0.0.1:1/authorize".to_owned(),
            token_url: "http://127.0.0.1:1/token".to_owned(),
        };
        let expired = TokenSource::with_endpoint(
            unreachable,
            Credentials {
                access_token: "access-secret".to_owned(),
                refresh_token: "refresh-secret".to_owned(),
                expiry: crate::auth::expiry("2000-01-01T00:00:00Z"),
                ..Credentials::default()
            },
            std::path::PathBuf::from("/nonexistent/chatgpt.json"),
            CancellationToken::new(),
        );
        for tokens in [expired, static_tokens("")] {
            let client = Client::with_base_url("http://127.0.0.1:1", tokens, "acct-secret");
            let (settlement, events) = complete(&client, &Request::default()).await;
            let error = settlement.result.unwrap_err();
            assert_eq!(
                error.to_string(),
                "chatgpt authorization failed; run 'otto login'"
            );
            for secret in ["access-secret", "refresh-secret", "acct-secret"] {
                assert!(!error.to_string().contains(secret));
            }
            assert!(events.is_empty());
        }
    }

    /// An empty account id is as unusable as an empty access token.
    #[tokio::test]
    async fn an_empty_account_id_reports_a_fixed_authorization_error() {
        let client = Client::with_base_url("http://127.0.0.1:1", static_tokens("token"), "  ");
        let (settlement, _) = complete(&client, &Request::default()).await;
        assert_eq!(
            settlement.result.unwrap_err().to_string(),
            "chatgpt authorization failed; run 'otto login'"
        );
    }

    #[tokio::test]
    async fn an_unrepresentable_redaction_boundary_is_rejected_before_any_request() {
        let server = testserver::spawn(|_| sse_response(CANNED_STREAM)).await;
        let oversized = "x".repeat(otto_core::safetext::MAX_DYNAMIC_VALUE_BYTES + 1);
        let client = Client::with_base_url(&server.url, static_tokens(&oversized), "acct-1");

        let (settlement, events) = complete(&client, &Request::default()).await;
        assert_eq!(
            settlement.result.unwrap_err().to_string(),
            "chatgpt request failed"
        );
        assert_eq!(server.count(), 0);
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn rate_limits_and_server_errors_are_not_retried() {
        for status in [429, 503] {
            let server = testserver::spawn(move |_| {
                testserver::status_response(status, "provider body is not exposed")
            })
            .await;
            let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");

            let (settlement, events) = complete(&client, &Request::default()).await;
            assert_eq!(
                settlement.result.unwrap_err().to_string(),
                format!("chatgpt responses HTTP {status}")
            );
            assert_eq!(server.count(), 1, "status {status}");
            assert!(events.is_empty());
        }
    }

    /// A connection that is refused carries no provider text into the error.
    #[tokio::test]
    async fn a_transport_failure_stops_after_three_retries() {
        let client = Client::with_base_url("http://127.0.0.1:1", static_tokens("token"), "acct-1");
        let (settlement, events) = complete(&client, &Request::default()).await;
        assert_eq!(settlement.attempts, 4);
        assert_eq!(
            settlement.outcome.effect_certainty,
            EffectCertainty::Unknown
        );
        assert_eq!(settlement.result.unwrap_err().to_string(), REQUEST_FAILED);
        assert_eq!(
            events,
            (2..=4)
                .map(|attempt| StreamEvent::Retry {
                    attempt,
                    max_attempts: 4,
                    delay: Duration::from_secs(1 << (attempt - 2)),
                    reason: "connection interrupted".into(),
                })
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_transport_retry_recovers_with_the_same_request() {
        let count = std::sync::atomic::AtomicUsize::new(0);
        let server = testserver::spawn(move |_| {
            if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                // Valid headers, then disconnect before any response output.
                testserver::truncated_sse_response("", 4096)
            } else {
                sse_response(CANNED_STREAM)
            }
        })
        .await;
        let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
        let (settlement, events) = complete(&client, &model_request()).await;
        assert!(settlement.result.is_ok());
        assert_eq!(settlement.attempts, 2);
        assert!(matches!(
            events.first(),
            Some(StreamEvent::Retry { attempt: 2, .. })
        ));
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body, requests[1].body);
    }

    #[tokio::test]
    async fn a_retry_after_a_tool_result_does_not_execute_the_tool_again() {
        use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct CountingTool(Arc<AtomicUsize>);
        #[async_trait::async_trait]
        impl ToolExecutor for CountingTool {
            fn definitions(&self) -> Vec<ToolDefinition> {
                model_request().tools
            }
            async fn execute(&self, call: ToolCall<'_>, _: &dyn OperationControl) -> ToolExecution {
                assert_eq!(call.name, "get_time");
                self.0.fetch_add(1, Ordering::SeqCst);
                ToolExecution::completed(ToolResult {
                    content: "tool completed once".into(),
                    ..ToolResult::default()
                })
            }
        }
        let attempts = AtomicUsize::new(0);
        let server = testserver::spawn(move |_| match attempts.fetch_add(1, Ordering::SeqCst) {
            0 => sse_response(CANNED_STREAM),
            1 => testserver::truncated_sse_response("", 4096),
            2 => sse_response(&format!(
                "{}{}",
                &CANNED_STREAM[..CANNED_STREAM
                    .find("event: response.output_item.added")
                    .unwrap()],
                &CANNED_STREAM[CANNED_STREAM.find("event: response.completed").unwrap()..]
            )),
            _ => testserver::status_response(500, "unexpected repeat"),
        })
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = otto_core::agent::Agent::new(
            Client::with_base_url(&server.url, static_tokens("token"), "acct-1"),
            CountingTool(calls.clone()),
            otto_core::session::MemorySession::new(),
            otto_core::agent::Options {
                model: "test-model".into(),
                ..Default::default()
            },
        );
        let mut events = Vec::new();
        agent
            .run(
                "hi",
                &mut |event| events.push(event),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].body.contains("tool completed once"));
        assert_eq!(requests[1].body, requests[2].body);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, otto_core::agent::Event::ProviderRetry { .. }))
        );
        assert!(matches!(
            events.last(),
            Some(otto_core::agent::Event::AgentFinished)
        ));
    }

    #[tokio::test]
    async fn stopping_during_backoff_prevents_another_request() {
        for reason in [
            OperationStopReason::UserCancellation,
            OperationStopReason::Deadline,
        ] {
            let server = testserver::spawn(|_| testserver::truncated_sse_response("", 4096)).await;
            let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
            let control = crate::deadline::Control::new(crate::deadline::Deadline::unlimited());
            let mut retries = 0;
            let settlement = client
                .complete(
                    &model_request(),
                    &mut |event| {
                        assert!(matches!(event, StreamEvent::Retry { .. }));
                        retries += 1;
                        control.stop(reason);
                    },
                    &control,
                )
                .await;
            assert_eq!(retries, 1);
            assert_eq!(server.count(), 1);
            assert_eq!(settlement.attempts, 1);
            assert_eq!(settlement.outcome.stop_reason, Some(reason));
            assert_eq!(
                settlement.outcome.effect_certainty,
                EffectCertainty::Unknown
            );
            assert!(matches!(
                (reason, settlement.result.unwrap_err()),
                (
                    OperationStopReason::UserCancellation,
                    ProviderError::Cancelled
                ) | (
                    OperationStopReason::Deadline,
                    ProviderError::DeadlineExceeded
                )
            ));
        }
    }

    #[tokio::test]
    async fn insufficient_deadline_budget_skips_backoff() {
        let server = testserver::spawn(|_| testserver::truncated_sse_response("", 4096)).await;
        let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
        let control = crate::deadline::Control::new(crate::deadline::Deadline::after(
            Duration::from_millis(1500),
        ));
        let settlement = client
            .complete(
                &model_request(),
                &mut |_| panic!("no retry fits the budget"),
                &control,
            )
            .await;
        assert_eq!(settlement.attempts, 1);
        assert_eq!(server.count(), 1);
        assert_eq!(settlement.result.unwrap_err().to_string(), REQUEST_FAILED);
    }

    #[tokio::test]
    async fn a_permanent_request_error_is_not_retried() {
        let client = Client::with_base_url("invalid-url", static_tokens("token"), "acct-1");
        let (settlement, events) = complete(&client, &model_request()).await;
        assert_eq!(settlement.attempts, 1);
        assert_eq!(settlement.result.unwrap_err().to_string(), REQUEST_FAILED);
        assert!(events.is_empty());
    }

    /// A body cut short reports the fixed request failure, with no token or
    /// account id in it.
    #[tokio::test]
    async fn a_body_cut_short_reports_a_fixed_request_failure() {
        let prefix = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial reply long enough to pass the secret redactor and become visible\"}\n\n";
        let server = testserver::spawn(move |_| {
            testserver::truncated_sse_response(prefix, prefix.len() + 4096)
        })
        .await;
        let client = Client::with_base_url(
            &server.url,
            static_tokens("stream-token-secret"),
            "stream-account-secret",
        );
        let (settlement, events) = complete(&client, &Request::default()).await;
        assert_eq!(settlement.attempts, 1);
        assert_eq!(server.count(), 1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::TextDelta { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Retry { .. }))
        );
        let error = settlement.result.unwrap_err().to_string();
        assert_eq!(error, "chatgpt request failed");
    }

    /// A stream that ends without `response.completed` is a protocol failure,
    /// and its message is redacted before it leaves the client.
    #[tokio::test]
    async fn a_protocol_failure_is_reported_with_a_redacted_message() {
        let server = testserver::spawn(|_| {
            sse_response("event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n")
        })
        .await;
        let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
        let (settlement, _) = complete(&client, &Request::default()).await;
        assert_eq!(
            settlement.result.unwrap_err().to_string(),
            "responses stream ended without response.completed"
        );
    }

    #[tokio::test]
    async fn a_deadline_stopped_call_reports_deadline_exceeded() {
        let server = testserver::spawn(|_| sse_response(CANNED_STREAM)).await;
        let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
        let mut sink = |_: StreamEvent| panic!("no event is emitted after deadline");
        let settlement = client
            .complete(&Request::default(), &mut sink, &StoppedControl::deadline())
            .await;
        assert_eq!(settlement.attempts, 0);
        assert_eq!(
            settlement.outcome.effect_certainty,
            EffectCertainty::NotStarted
        );
        let error = settlement.result.unwrap_err();
        assert!(
            matches!(error, ProviderError::DeadlineExceeded),
            "{error:?}"
        );
        assert_eq!(server.count(), 0);
    }

    /// The per-call token, not a process-level one, decides that the turn is
    /// over.
    #[tokio::test]
    async fn a_cancelled_call_reports_cancellation() {
        let server = testserver::spawn(|_| sse_response(CANNED_STREAM)).await;
        let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut sink = |_: StreamEvent| panic!("no event is emitted after cancellation");
        let settlement = client
            .complete(&Request::default(), &mut sink, &cancel)
            .await;
        assert_eq!(settlement.attempts, 0);
        assert_eq!(
            settlement.outcome.effect_certainty,
            EffectCertainty::NotStarted
        );
        let error = settlement.result.unwrap_err();
        assert!(matches!(error, ProviderError::Cancelled), "{error:?}");
        assert_eq!(server.count(), 0);
    }

    #[test]
    fn serialized_request_size_matches_the_exact_wire_payload() {
        let client = Client::new(static_tokens("token"), "acct-1");
        let request = model_request();
        let expected = serde_json::to_vec(&otto_core::openairesponses::protocol::build_request(
            &request,
        ))
        .unwrap()
        .len();
        assert_eq!(client.serialized_request_size(&request).unwrap(), expected);
    }

    fn redaction_marker(values: &[String]) -> String {
        let mut collector = otto_core::safetext::SecretCollector::new();
        for value in values {
            assert!(collector.add(value));
        }
        otto_core::safetext::dynamic_redaction_marker(&collector.values()).unwrap()
    }
}
