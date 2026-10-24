use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use otto_core::model::{Block, Message, OperationDisposition, Role};
use otto_core::provider::StreamEvent;

use super::*;
use crate::faults::http;

mod cache_key;
mod errors;
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
