
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use otto_core::model::{Block, Message, OperationDisposition, Role};
use otto_core::provider::StreamEvent;

use super::*;
use crate::faults::http;

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

/// A control that has not stopped but has only `remaining` of budget, so
/// the retry policy is limited by the budget alone.
struct BudgetControl {
    token: CancellationToken,
    remaining: Duration,
}

impl OperationControl for BudgetControl {
    fn cancellation_token(&self) -> &CancellationToken {
        &self.token
    }

    fn remaining(&self) -> Option<Duration> {
        Some(self.remaining)
    }

    fn stop_reason(&self) -> Option<OperationStopReason> {
        None
    }
}

/// Starts a loopback origin server on `faults::http`.
///
/// `handler` receives the request head as text and the request body, and
/// returns the raw response bytes. An empty return closes the connection
/// without a response, which is how the tests produce a transport error.
async fn spawn_server<H>(handler: H) -> http::Server
where
    H: Fn(&str, &[u8]) -> Vec<u8> + Send + Sync + 'static,
{
    http::spawn(move |request| handler(&request.head, request.body.as_bytes())).await
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
        .result
}

#[tokio::test]
async fn a_deadline_stopped_call_reports_deadline_exceeded() {
    let server = spawn_server(|_head, _body| {
        http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
    })
    .await;
    let client = Client::new(&server.url, "key");
    let mut emit = |_: StreamEvent| panic!("no event is emitted after deadline");
    let settlement = client
        .complete(&model_request(), &mut emit, &StoppedControl::deadline())
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
}

// --- transport retry -------------------------------------------------

#[tokio::test]
async fn rate_limits_and_server_errors_are_not_retried() {
    #[derive(Default)]
    struct Statuses(std::sync::Mutex<Vec<u16>>);
    impl otto_core::diagnostics::Observer for Statuses {
        fn activity(&self, _: otto_core::diagnostics::Phase, _: usize, status: Option<u16>) {
            if let Some(status) = status {
                self.0.lock().unwrap().push(status);
            }
        }
    }
    for status in [429u16, 503] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            counter.fetch_add(1, Ordering::SeqCst);
            http_response(status, &[], "try again")
        })
        .await;

        let client = Client::new(&server.url, "key");
        let mut events = Vec::new();
        let mut emit = |event| events.push(event);
        let statuses = Statuses::default();
        let settlement = client
            .complete_observed(
                &model_request(),
                &mut emit,
                &CancellationToken::new(),
                &statuses,
            )
            .await;
        assert_eq!(*statuses.0.lock().unwrap(), vec![status]);
        assert_eq!(settlement.attempts, 1);
        assert_eq!(
            settlement.outcome.effect_certainty,
            EffectCertainty::Completed
        );
        let error = settlement.result.expect_err("the first status is returned");
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

/// A connection that is refused never reached the endpoint: it is tried
/// again 1, 2 and 4 s later, then fails as not started. Paused time skips
/// the backoff.
#[tokio::test(start_paused = true)]
async fn a_refused_connection_is_retried_three_times_and_not_started() {
    let client = Client::new("http://127.0.0.1:1", "key");
    let mut events = Vec::new();
    let settlement = client
        .complete(
            &model_request(),
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(settlement.attempts, 4);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::NotStarted
    );
    assert!(settlement.result.is_err());
    assert_eq!(
        events,
        (2..=4)
            .map(|attempt| StreamEvent::Retry {
                attempt,
                max_attempts: 4,
                delay: Duration::from_secs(1 << (attempt - 2)),
                reason: "connection failed".into(),
                interrupted_output: false,
            })
            .collect::<Vec<_>>()
    );
}

/// The endpoint starts listening after the first refusal; the retry
/// sends the same request and succeeds.
#[tokio::test]
async fn a_connection_retry_recovers_once_the_endpoint_listens() {
    let server = http::late_listener(
        http::refused_port(),
        Duration::from_millis(300),
        vec![http::Script::sse_done(&[
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ])],
    );
    let client = Client::new(&server.url, "key");
    let mut retries = 0;
    let settlement = client
        .complete(
            &model_request(),
            &mut |event| {
                if matches!(event, StreamEvent::Retry { .. }) {
                    retries += 1;
                }
            },
            &CancellationToken::new(),
        )
        .await;
    assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(retries, 1);
}

/// The endpoint accepts the request and closes before any response. The
/// request may already be at the endpoint, so the retry is reported and
/// the settlement would keep unknown effect certainty.
#[tokio::test]
async fn a_connection_closed_before_the_response_is_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let server = spawn_server(move |_head, _body| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            return Vec::new();
        }
        http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
    })
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await;
    assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(retry_reasons(&events), [TRANSPORT_RETRY_REASON]);
}

/// The response stream is cut before any delta; nothing was visible, so
/// the retry sends the same request again and returns the whole response.
#[tokio::test]
async fn a_stream_cut_before_any_delta_is_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let server = spawn_server(move |_head, _body| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            return truncated_stream("");
        }
        http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
    })
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await;
    assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(retry_reasons(&events), [TRANSPORT_RETRY_REASON]);
}

/// A connection reset that keeps repeating is tried 1, 2 and 4 seconds
/// later (real time: paused time would trip the read timeout on the
/// loopback socket), then reported as a lost transport whose effect is
/// unknown.
#[tokio::test]
async fn a_repeated_stream_cut_stops_as_transport_lost() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let server = spawn_server(move |_head, _body| {
        counter.fetch_add(1, Ordering::SeqCst);
        truncated_stream("")
    })
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await;
    assert_eq!(settlement.attempts, 4);
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    assert_eq!(retry_reasons(&events), [TRANSPORT_RETRY_REASON; 3]);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Unknown
    );
    assert_eq!(
        settlement.outcome.disposition,
        OperationDisposition::Interrupted
    );
    assert_eq!(
        settlement.outcome.stop_reason,
        Some(OperationStopReason::TransportLost)
    );
    let error = settlement.result.unwrap_err().to_string();
    assert!(
        error.starts_with("read chat completion stream:"),
        "error = {error}"
    );
}

/// A stream cut is not retried when the remaining budget cannot hold the
/// backoff and another attempt.
#[tokio::test]
async fn a_stream_cut_without_retry_budget_is_not_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let server = spawn_server(move |_head, _body| {
        counter.fetch_add(1, Ordering::SeqCst);
        truncated_stream("")
    })
    .await;

    let control = BudgetControl {
        token: CancellationToken::new(),
        remaining: Duration::from_secs(1),
    };
    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &control)
        .await;
    assert_eq!(settlement.attempts, 1);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(events.is_empty());
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Unknown
    );
    assert_eq!(
        settlement.outcome.stop_reason,
        Some(OperationStopReason::TransportLost)
    );
}

/// A delta already reached the caller, so the output cannot be replayed
/// and the failure stays single-attempt.
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
    Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await
        .result
        .expect_err("the first interrupted stream is returned");
    assert!(!events.is_empty());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Retry { .. }))
    );
}

/// The `Retry` event reason of every retry in `events`.
fn retry_reasons(events: &[StreamEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Retry { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect()
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
    let location = format!("{}/chat/completions", target.url);
    let source = spawn_server(move |_head, _body| {
        counted.fetch_add(1, Ordering::SeqCst);
        http_response(302, &[("Location", &location)], "")
    })
    .await;

    let client = Client::new(&source.url, "secret");
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

    let client = Client::new(&format!("{}/redirect", server.url), "secret");
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

    let client = Client::new(&server.url, KEY);
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

    let client = Client::new(&server.url, KEY);
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

    let client = Client::new(&server.url, "secret");
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

    let client = Client::new(&server.url, KEY);
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

    let client = Client::new(&server.url, "key");
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
        cache_key: String::new(),
        messages: vec![Message {
            role: Role::User,
            blocks: vec![Block::text("escape-heavy: \\ \" \n \t 界 <>&")],
            ..Message::default()
        }],
        tools: Vec::new(),
        remote_compaction: None,
        request_remote_compaction: false,
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

    let client = Client::new(&format!("{}/gateway/v1/", server.url), "secret");
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
async fn catalog_preserves_compat_metadata_and_auth_without_required_metadata() {
    let server = crate::faults::http::spawn(|_| crate::faults::http::json_response(r#"{"data":[{"id":"fixture-a","display_name":"Provider name","description":"catalog-compat-secret"},{"id":"fixture-b"}]}"#)).await;
    let client = Client::new(&server.url, "catalog-compat-secret");
    let catalog = client.catalog(&CancellationToken::new()).await.unwrap();
    assert_eq!(catalog.models.len(), 2);
    assert_eq!(catalog.models[0].metadata["description"], "[REDACTED]");
    assert!(catalog.models[1].metadata.is_empty());
    assert_eq!(
        server.requests()[0].header("authorization"),
        "Bearer catalog-compat-secret"
    );
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

    let client = Client::new(&format!("{}/gateway/v1/", server.url), "secret");
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

    let client = Client::new(&server.url, "sk-secret-value-123");
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

    let client = Client::new(&server.url, "key");
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
