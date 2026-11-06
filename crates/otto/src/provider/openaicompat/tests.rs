use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use otto_core::model::{Block, Message, OperationDisposition, Role};
use otto_core::provider::StreamEvent;

use super::*;
use crate::faults::http;

mod cache_key;
mod errors;
mod output_retry;
mod retry;
mod wire;

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

// Mock endpoints receive explicit test-owned origin grants, never production bypasses.
fn trusted_test_client(base_url: &str, api_key: &str) -> Client {
    let origin = url::Url::parse(base_url)
        .ok()
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_default();
    match otto_core::config::endpoint::TrustedEndpoint::authorize(
        base_url,
        "openai-compatible",
        &[origin.clone()],
        &[origin],
    ) {
        Ok(endpoint) => Client::new_trusted(&endpoint, api_key),
        Err(_) => Client::new(base_url, api_key),
    }
}

#[tokio::test]
async fn endpoint_rejection_prevents_secret_body_network_and_injected_redirect_client() {
    let server = http::spawn(|_| Vec::<u8>::new()).await;
    let clients = [
        Client::new(&server.url, "fixture-secret"),
        Client::with_http_client(&server.url, "fixture-secret", reqwest::Client::new()),
    ];
    for client in clients {
        let settlement = client
            .complete(&model_request(), &mut |_| {}, &CancellationToken::new())
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
    let client = trusted_test_client(&source.url, "fixture-secret");
    assert!(client.catalog(&CancellationToken::new()).await.is_err());
    assert_eq!(source.count(), 1);
    assert_eq!(target.count(), 0);
}
