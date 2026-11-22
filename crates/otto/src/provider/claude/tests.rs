mod cache_usage_tests;
mod recovery_tests;
mod stream_tests;

use super::*;
use otto_core::model::{Block, Message, Role};
use otto_faults::http;

fn request() -> Request {
    Request {
        model: "fixture-claude".into(),
        messages: vec![Message {
            role: Role::User,
            blocks: vec![Block::text("hello")],
            ..Message::default()
        }],
        ..Request::default()
    }
}

fn response(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

const STREAM: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fixture\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
);

#[tokio::test]
async fn messages_use_native_authentication_and_stream_codec() {
    let server = http::spawn(|incoming| {
        assert!(
            incoming.head.starts_with("POST /v1/messages "),
            "{}",
            incoming.head
        );
        let headers = incoming.head.to_ascii_lowercase();
        assert!(headers.contains("x-api-key: fixture-key"));
        assert!(headers.contains("anthropic-version: 2026-10-21"));
        assert!(headers.contains("anthropic-workspace-id: fixture-workspace"));
        assert!(!headers.contains("authorization:"));
        let body: serde_json::Value = serde_json::from_str(&incoming.body).unwrap();
        assert_eq!(body["model"], "fixture-claude");
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body["max_tokens"].as_u64().is_some_and(|value| value > 0));
        response(200, STREAM)
    })
    .await;
    let client = trusted_test_client(
        &server.url,
        "fixture-key",
        "2026-10-21",
        "fixture-workspace",
    );
    let mut events = Vec::new();
    let settlement = client
        .complete(
            &request(),
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await;
    let result = settlement.result.expect("successful Claude stream");
    assert_eq!(settlement.attempts, 1);
    assert_eq!(result.message.text(), "hello");
    assert!(!events.is_empty());
}

#[tokio::test]
async fn both_model_listing_paths_use_claude_headers() {
    let server = http::spawn(|incoming| {
        assert!(incoming.head.starts_with("GET /v1/models "));
        let headers = incoming.head.to_ascii_lowercase();
        assert!(headers.contains("x-api-key: fixture-key"));
        assert!(headers.contains("anthropic-version: 2026-10-21"));
        assert!(!headers.contains("authorization:"));
        assert!(!headers.contains("anthropic-workspace-id:"));
        response(
            200,
            r#"{"data":[{"id":"fixture-z"},{"id":"fixture-a"},{"id":"fixture-z"}]}"#,
        )
    })
    .await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    assert_eq!(
        client.list_models(&CancellationToken::new()).await.unwrap(),
        ["fixture-a", "fixture-z"]
    );
    let catalog = client.catalog(&CancellationToken::new()).await.unwrap();
    assert_eq!(catalog.models.len(), 2);
}

#[tokio::test]
async fn errors_redact_key_and_workspace_and_cancel_before_dispatch() {
    let server =
        http::spawn(|_| response(400, "fixture-api-secret fixture-workspace-secret")).await;
    let client = trusted_test_client(
        &server.url,
        "fixture-api-secret",
        "2026-10-21",
        "fixture-workspace-secret",
    );
    let error = client
        .complete(&request(), &mut |_| {}, &CancellationToken::new())
        .await
        .result
        .unwrap_err();
    let error = error.to_string();
    assert!(!error.contains("fixture-api-secret"), "{error}");
    assert!(!error.contains("fixture-workspace-secret"), "{error}");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let settlement = client.complete(&request(), &mut |_| {}, &cancel).await;
    assert!(matches!(settlement.result, Err(ProviderError::Cancelled)));
    assert_eq!(settlement.attempts, 0);
}

#[test]
fn request_size_matches_claude_wire_encoding() {
    let client = trusted_test_client("https://api.anthropic.com", "", "2026-10-21", "");
    let request = request();
    assert_eq!(
        client.serialized_request_size(&request).unwrap(),
        serde_json::to_vec(&otto_core::claude::protocol::build_request(&request).unwrap())
            .unwrap()
            .len()
    );
}

#[tokio::test]
async fn rate_limits_and_overload_observe_retry_after_and_retry_events() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for status in [429, 503] {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let server = http::spawn(move |_| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                format!("HTTP/1.1 {status} Retry\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
            } else { response(200, STREAM) }
        }).await;
        let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
        let mut retries = Vec::new();
        let result = client
            .complete(
                &request(),
                &mut |event| {
                    if let StreamEvent::Retry { delay, reason, .. } = event {
                        retries.push((delay, reason));
                    }
                },
                &CancellationToken::new(),
            )
            .await;
        assert!(result.result.is_ok());
        assert_eq!(result.attempts, 2);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(retries, [(Duration::ZERO, format!("HTTP {status}"))]);
    }
}

#[tokio::test]
async fn context_overflow_is_typed_and_model_catalog_body_is_bounded() {
    let server = http::spawn(|_| response(400, r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#)).await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    assert!(matches!(
        client
            .complete(&request(), &mut |_| {}, &CancellationToken::new())
            .await
            .result,
        Err(ProviderError::Overflow(_))
    ));
    let server = http::spawn(|_| response(200, &"x".repeat(MAX_MODELS_BODY + 1))).await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    assert!(
        client
            .list_models(&CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("larger than")
    );
}

struct DeadlineControl(CancellationToken);
impl OperationControl for DeadlineControl {
    fn cancellation_token(&self) -> &CancellationToken {
        &self.0
    }
    fn remaining(&self) -> Option<Duration> {
        Some(Duration::ZERO)
    }
    fn stop_reason(&self) -> Option<OperationStopReason> {
        Some(OperationStopReason::Deadline)
    }
}
#[tokio::test]
async fn deadline_before_dispatch_is_not_user_cancellation() {
    let client = trusted_test_client("https://api.anthropic.com", "fixture-key", "2026-10-21", "");
    let result = client
        .complete(
            &request(),
            &mut |_| {},
            &DeadlineControl(CancellationToken::new()),
        )
        .await;
    assert_eq!(result.attempts, 0);
    assert!(matches!(
        result.result,
        Err(ProviderError::DeadlineExceeded)
    ));
}
#[tokio::test]
async fn observed_calls_report_headers_and_received_bytes() {
    use otto_core::diagnostics::{Observer, Phase};
    struct Seen(std::sync::Mutex<Vec<(Phase, usize, Option<u16>)>>);
    impl Observer for Seen {
        fn activity(&self, phase: Phase, bytes: usize, status: Option<u16>) {
            self.0.lock().unwrap().push((phase, bytes, status));
        }
    }
    let server = http::spawn(|_| response(200, STREAM)).await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    let observer = Seen(Default::default());
    let result = client
        .complete_observed(
            &request(),
            &mut |_| {},
            &CancellationToken::new(),
            &observer,
        )
        .await;
    assert!(result.result.is_ok());
    let seen = observer.0.lock().unwrap();
    assert!(
        seen.iter()
            .any(|(phase, _, status)| matches!(phase, Phase::Headers) && *status == Some(200))
    );
    assert!(
        seen.iter()
            .any(|(phase, bytes, _)| matches!(phase, Phase::Receiving) && *bytes > 0)
    );
}

// Mock endpoints receive explicit test-owned origin grants, never production bypasses.
fn trusted_test_client(
    base_url: &str,
    api_key: &str,
    api_version: &str,
    workspace_id: &str,
) -> Client {
    let origin = url::Url::parse(base_url)
        .ok()
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_default();
    match otto_core::config::endpoint::TrustedEndpoint::authorize(
        base_url,
        "claude",
        std::slice::from_ref(&origin),
        std::slice::from_ref(&origin),
    ) {
        Ok(endpoint) => Client::new_trusted(&endpoint, api_key, api_version, workspace_id),
        Err(_) => Client::new(base_url, api_key, api_version, workspace_id),
    }
}

#[tokio::test]
async fn endpoint_rejection_prevents_secret_body_and_network() {
    let server = http::spawn(|_| Vec::<u8>::new()).await;
    let clients = [Client::new(
        &server.url,
        "fixture-secret",
        "2026-10-21",
        "fixture-workspace",
    )];
    for client in clients {
        let settlement = client
            .complete(&request(), &mut |_| {}, &CancellationToken::new())
            .await;
        assert!(settlement.result.is_err());
        assert_eq!(settlement.attempts, 0);
        assert!(client.list_models(&CancellationToken::new()).await.is_err());
        assert!(client.catalog(&CancellationToken::new()).await.is_err());
    }
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn endpoint_catalog_redirect_does_not_reach_target() {
    let target = http::spawn(|_| Vec::<u8>::new()).await;
    let location = format!("{}/models", target.url);
    let source = http::spawn(move |_| format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await;
    let client = trusted_test_client(&source.url, "fixture-secret", "2026-10-21", "");
    assert!(client.catalog(&CancellationToken::new()).await.is_err());
    assert_eq!(source.count(), 1);
    assert_eq!(target.count(), 0);
}
