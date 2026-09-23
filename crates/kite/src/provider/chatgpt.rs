//! HTTP transport for the ChatGPT backend Responses API.
//!
//! The wire codec lives in [`kite_core::openairesponses`]; this module owns the
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
//! Cancellation: the token fetch, the request send, and every body read race
//! the caller's [`CancellationToken`]. A cancelled call returns
//! [`ProviderError::Cancelled`].
//!
//! Errors: every failure outside the stream decoder is one of three fixed
//! strings, so no endpoint text and no credential can reach the caller. A
//! decoder failure carries the decoder's own message with the access token and
//! the account id redacted.
//!
//! Two deliberate decisions:
//!   - ponytail: no retry on 429/5xx, so the status is returned on the first
//!     attempt.
//!   - reqwest exposes no cap on the size of a response header block, so no
//!     bound is enforced on it. The same gap exists in
//!     [`crate::provider::openaicompat`].

use std::collections::HashMap;
use std::time::Duration;

use futures_util::TryStreamExt;
use kite_core::agent::redactor::{Redactor, StreamRedactor};
use kite_core::model::Message;
use kite_core::openairesponses::protocol::{build_request, serialized_request_size};
use kite_core::openairesponses::stream::StreamAssembler;
use kite_core::provider::{
    Provider, ProviderError, Request, RequestSizer, Response, StreamEvent, StreamSink,
};
use tokio_util::sync::CancellationToken;

use crate::auth::token::TokenSource;

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
const AUTHORIZATION_FAILED: &str = "chatgpt authorization failed; run 'kite login'";
/// The request did not complete, with no detail that could carry a secret.
const REQUEST_FAILED: &str = "chatgpt request failed";

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
    /// Sends one request to the Responses backend and assembles the stream.
    ///
    /// There is no retry: a 429 or a 5xx is reported on the first attempt.
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Response, ProviderError> {
        let http = match &self.http {
            Ok(http) => http,
            Err(error) => return Err(ProviderError::Other(error.clone())),
        };
        let credentials = match self.tokens.token(cancel).await {
            Ok(credentials) => credentials,
            // The token source has its own fixed errors; none of them is
            // inspected, so nothing it saw can reach the caller.
            Err(_) if cancel.is_cancelled() => return Err(ProviderError::Cancelled),
            Err(_) => return Err(ProviderError::Other(AUTHORIZATION_FAILED.to_owned())),
        };
        let access_token = credentials.access_token;
        if access_token.trim().is_empty() || self.account_id.trim().is_empty() {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            return Err(ProviderError::Other(AUTHORIZATION_FAILED.to_owned()));
        }

        // The redactor is built before anything is sent, so a credential that
        // cannot be redacted stops the request instead of streaming output
        // that could carry it.
        let redactor = Redactor::new(&[access_token.clone(), self.account_id.clone()]);
        if !redactor.allows_dynamic_content() {
            return Err(ProviderError::Other(REQUEST_FAILED.to_owned()));
        }

        let payload = serde_json::to_vec(&build_request(request))
            .map_err(|error| ProviderError::Other(format!("encode responses request: {error}")))?;

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
        let response = match with_cancel(cancel, send).await {
            None => return Err(ProviderError::Cancelled),
            Some(Ok(response)) => response,
            Some(Err(_)) => return Err(ProviderError::Other(REQUEST_FAILED.to_owned())),
        };

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            // The body is never read, so no part of it can reach the error.
            return Err(ProviderError::Other(format!(
                "chatgpt responses HTTP {status}"
            )));
        }

        let mut events = EventRedactor::new(&redactor);
        let mut assembler = StreamAssembler::new();
        let mut body = Box::pin(response.bytes_stream());
        let result = loop {
            if assembler.is_done() {
                break Ok(());
            }
            let chunk = match with_cancel(cancel, body.try_next()).await {
                None => return Err(ProviderError::Cancelled),
                Some(Ok(Some(chunk))) => chunk,
                Some(Ok(None)) => break Ok(()),
                // A read failure reports the fixed request failure, never the
                // transport's own text.
                Some(Err(_)) => break Err(ProviderError::Other(REQUEST_FAILED.to_owned())),
            };
            if let Err(error) = assembler.push(&chunk, &mut |event| events.emit(event, &mut *emit))
            {
                break Err(ProviderError::Other(
                    redactor.redact_string(&error.to_string()),
                ));
            }
        };
        if let Err(error) = result {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            return Err(error);
        }
        let response = assembler
            .finish(&mut |event| events.emit(event, &mut *emit))
            .map_err(|error| {
                if cancel.is_cancelled() {
                    ProviderError::Cancelled
                } else {
                    ProviderError::Other(redactor.redact_string(&error.to_string()))
                }
            })?;
        events.flush(&mut *emit);
        Ok(Response {
            message: redact_message(&redactor, response.message),
        })
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
    text: StreamRedactor<'a>,
    tools: HashMap<String, ToolState<'a>>,
    /// Insertion order of `tools`, so the flush order is deterministic.
    order: Vec<String>,
}

impl<'a> EventRedactor<'a> {
    fn new(redactor: &'a Redactor) -> Self {
        Self {
            redactor,
            text: redactor.new_stream(),
            tools: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// Forwards `event` with every field redacted. An event whose fields are
    /// all empty afterwards is dropped.
    fn emit(&mut self, event: StreamEvent, sink: StreamSink<'_>) {
        match event {
            StreamEvent::TextDelta { text } => {
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

/// Races `future` against the cancellation token. `None` means the token was
/// cancelled and `future` was dropped without completing.
async fn with_cancel<T>(
    cancel: &CancellationToken,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        value = future => Some(value),
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
    use kite_core::model::{Block, BlockType, FinishReason, Message, Role, ToolDefinition, Usage};
    use kite_core::provider::StreamEvent;

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
    ) -> (Result<Response, ProviderError>, Vec<StreamEvent>) {
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

        let (result, events) = complete(&client, &model_request()).await;
        let response = result.unwrap();

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
            let (result, _) = complete(&client, &Request::default()).await;
            assert_eq!(result.unwrap().message.usage, want, "{name}");
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

        let (result, events) = complete(&client, &Request::default()).await;
        let response = result.unwrap();
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

        let (result, _) = complete(&client, &Request::default()).await;
        let error = result.unwrap_err();
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

        let (result, _) = complete(&client, &model_request()).await;
        assert_eq!(
            result.unwrap_err().to_string(),
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
            let (result, events) = complete(&client, &Request::default()).await;
            let error = result.unwrap_err();
            assert_eq!(
                error.to_string(),
                "chatgpt authorization failed; run 'kite login'"
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
        let (result, _) = complete(&client, &Request::default()).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "chatgpt authorization failed; run 'kite login'"
        );
    }

    #[tokio::test]
    async fn an_unrepresentable_redaction_boundary_is_rejected_before_any_request() {
        let server = testserver::spawn(|_| sse_response(CANNED_STREAM)).await;
        let oversized = "x".repeat(kite_core::safetext::MAX_DYNAMIC_VALUE_BYTES + 1);
        let client = Client::with_base_url(&server.url, static_tokens(&oversized), "acct-1");

        let (result, events) = complete(&client, &Request::default()).await;
        assert_eq!(result.unwrap_err().to_string(), "chatgpt request failed");
        assert_eq!(server.count(), 0);
        assert!(events.is_empty());
    }

    /// A connection that is refused carries no provider text into the error.
    #[tokio::test]
    async fn a_transport_failure_reports_a_fixed_request_failure() {
        let client = Client::with_base_url("http://127.0.0.1:1", static_tokens("token"), "acct-1");
        let (result, _) = complete(&client, &Request::default()).await;
        assert_eq!(result.unwrap_err().to_string(), "chatgpt request failed");
    }

    /// A body cut short reports the fixed request failure, with no token or
    /// account id in it.
    #[tokio::test]
    async fn a_body_cut_short_reports_a_fixed_request_failure() {
        let prefix = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n";
        let server = testserver::spawn(move |_| {
            testserver::truncated_sse_response(prefix, prefix.len() + 4096)
        })
        .await;
        let client = Client::with_base_url(
            &server.url,
            static_tokens("stream-token-secret"),
            "stream-account-secret",
        );
        let (result, _) = complete(&client, &Request::default()).await;
        let error = result.unwrap_err().to_string();
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
        let (result, _) = complete(&client, &Request::default()).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "responses stream ended without response.completed"
        );
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
        let error = client
            .complete(&Request::default(), &mut sink, &cancel)
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::Cancelled), "{error:?}");
        assert_eq!(server.count(), 0);
    }

    #[test]
    fn serialized_request_size_matches_the_exact_wire_payload() {
        let client = Client::new(static_tokens("token"), "acct-1");
        let request = model_request();
        let expected = serde_json::to_vec(&kite_core::openairesponses::protocol::build_request(
            &request,
        ))
        .unwrap()
        .len();
        assert_eq!(client.serialized_request_size(&request).unwrap(), expected);
    }

    fn redaction_marker(values: &[String]) -> String {
        let mut collector = kite_core::safetext::SecretCollector::new();
        for value in values {
            assert!(collector.add(value));
        }
        kite_core::safetext::dynamic_redaction_marker(&collector.values()).unwrap()
    }
}
