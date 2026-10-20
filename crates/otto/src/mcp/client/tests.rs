
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Notify;

use super::*;
use crate::mcp::MODERN_VERSION;
use crate::mcp::jsonrpc::RpcError;

fn modern_discover_ok() -> Result<Result<Value, RpcError>, CallError> {
    Ok(Ok(json!({"protocolVersion": MODERN_VERSION})))
}

/// Rejects every request as a transport failure, so era negotiation
/// never succeeds. Counts `close` calls.
struct RejectingTransport {
    close_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Transport for RejectingTransport {
    async fn request(
        &self,
        outbound: Outbound,
        _cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        assert_eq!(outbound.method, "server/discover");
        Err(CallError::Transport("boom".to_string()))
    }
    async fn notify(&self, _outbound: Outbound) -> Result<(), CallError> {
        Ok(())
    }
    async fn close(&self) {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn connect_closes_transport_when_negotiation_fails() {
    let close_calls = Arc::new(AtomicUsize::new(0));
    let transport: Box<dyn Transport> = Box::new(RejectingTransport {
        close_calls: close_calls.clone(),
    });
    let cancel = CancellationToken::new();

    let result = Client::connect(
        "test".to_string(),
        transport,
        Duration::from_secs(5),
        Duration::from_secs(5),
        &cancel,
    )
    .await;

    assert!(result.is_err(), "expected connect to fail");
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
}

struct HangingCloseTransport;

#[async_trait::async_trait]
impl Transport for HangingCloseTransport {
    async fn request(
        &self,
        _outbound: Outbound,
        _cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        Err(CallError::Transport("connect failed".to_string()))
    }

    async fn notify(&self, _outbound: Outbound) -> Result<(), CallError> {
        Ok(())
    }

    async fn close(&self) {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn failed_connect_cleanup_is_bounded() {
    let handle = tokio::spawn(async {
        Client::connect(
            "test".to_string(),
            Box::new(HangingCloseTransport),
            Duration::from_secs(1),
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await
    });

    tokio::time::advance(CONNECT_CLOSE_TIMEOUT).await;
    let error = match handle.await.unwrap() {
        Ok(_) => panic!("connect should fail"),
        Err(error) => error,
    };
    assert!(matches!(error, CallError::Transport(_)));
}

/// Negotiates modern successfully, then never answers `tools/list`.
struct PendingToolsListTransport {
    close_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Transport for PendingToolsListTransport {
    async fn request(
        &self,
        outbound: Outbound,
        _cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        match outbound.method.as_str() {
            "server/discover" => modern_discover_ok(),
            "tools/list" => std::future::pending().await,
            other => panic!("unexpected method {other}"),
        }
    }
    async fn notify(&self, _outbound: Outbound) -> Result<(), CallError> {
        Ok(())
    }
    async fn close(&self) {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn connect_times_out_when_tools_list_never_answers() {
    let close_calls = Arc::new(AtomicUsize::new(0));
    let transport: Box<dyn Transport> = Box::new(PendingToolsListTransport {
        close_calls: close_calls.clone(),
    });

    let handle = tokio::spawn(async move {
        let cancel = CancellationToken::new();
        Client::connect(
            "test".to_string(),
            transport,
            Duration::from_millis(50),
            Duration::from_secs(5),
            &cancel,
        )
        .await
    });

    tokio::time::advance(Duration::from_secs(1)).await;
    let result = handle.await.expect("connect task panicked");
    let error = match result {
        Ok(_) => panic!("expected connect to time out"),
        Err(error) => error,
    };

    assert!(
        matches!(error, CallError::Timeout),
        "expected Timeout, got {error:?}"
    );
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
}

/// Negotiates modern successfully, then answers every `tools/list` with
/// the same `nextCursor`.
struct RepeatingCursorTransport {
    tools_list_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Transport for RepeatingCursorTransport {
    async fn request(
        &self,
        outbound: Outbound,
        _cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        match outbound.method.as_str() {
            "server/discover" => modern_discover_ok(),
            "tools/list" => {
                self.tools_list_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Ok(json!({"tools": [], "nextCursor": "a"})))
            }
            other => panic!("unexpected method {other}"),
        }
    }
    async fn notify(&self, _outbound: Outbound) -> Result<(), CallError> {
        Ok(())
    }
    async fn close(&self) {}
}

#[tokio::test]
async fn connect_fails_when_tools_list_repeats_its_cursor() {
    let tools_list_calls = Arc::new(AtomicUsize::new(0));
    let transport: Box<dyn Transport> = Box::new(RepeatingCursorTransport {
        tools_list_calls: tools_list_calls.clone(),
    });
    let cancel = CancellationToken::new();

    let result = Client::connect(
        "test".to_string(),
        transport,
        Duration::from_secs(5),
        Duration::from_secs(5),
        &cancel,
    )
    .await;

    let error = match result {
        Ok(_) => panic!("expected a repeated-cursor error"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, CallError::Transport(message) if message.contains("same cursor twice")),
        "unexpected error: {error:?}"
    );
    assert_eq!(tools_list_calls.load(Ordering::SeqCst), 2);
}

/// Rejects `server/discover` with an unrecognized RPC error carrying a
/// long message and a `data` field that must never reach the client's
/// error text.
struct LongRpcErrorTransport;

#[async_trait::async_trait]
impl Transport for LongRpcErrorTransport {
    async fn request(
        &self,
        outbound: Outbound,
        _cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        assert_eq!(outbound.method, "server/discover");
        Ok(Err(RpcError {
            code: -1,
            message: "x".repeat(1000),
            data: Some(json!({"top-secret-marker": "must-not-leak"})),
        }))
    }
    async fn notify(&self, _outbound: Outbound) -> Result<(), CallError> {
        Ok(())
    }
    async fn close(&self) {}
}

#[tokio::test]
async fn connect_truncates_discover_rpc_error_and_drops_data() {
    let transport: Box<dyn Transport> = Box::new(LongRpcErrorTransport);
    let cancel = CancellationToken::new();

    let result = Client::connect(
        "test".to_string(),
        transport,
        Duration::from_secs(5),
        Duration::from_secs(5),
        &cancel,
    )
    .await;

    let error = match result {
        Ok(_) => panic!("expected a negotiation failure"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.len() < 300,
        "message too long ({} bytes): {message}",
        message.len()
    );
    assert!(!message.contains("top-secret-marker"));
    assert!(!message.contains("must-not-leak"));
}
/// Negotiates modern with an empty tool list, then waits for the caller's
/// token on every `tools/call` and records that it saw the cancellation,
/// the way a real transport releases its pending request.
struct CancelAwareTransport {
    cancels: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Transport for CancelAwareTransport {
    async fn request(
        &self,
        outbound: Outbound,
        cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        match outbound.method.as_str() {
            "server/discover" => modern_discover_ok(),
            "tools/list" => Ok(Ok(json!({"tools": []}))),
            "tools/call" => {
                cancel.cancelled().await;
                self.cancels.fetch_add(1, Ordering::SeqCst);
                Err(CallError::Cancelled)
            }
            other => panic!("unexpected method {other}"),
        }
    }
    async fn notify(&self, _outbound: Outbound) -> Result<(), CallError> {
        Ok(())
    }
    async fn close(&self) {}
}

async fn cancel_aware_client(call_timeout: Duration) -> (Client, Arc<AtomicUsize>) {
    let cancels = Arc::new(AtomicUsize::new(0));
    let transport: Box<dyn Transport> = Box::new(CancelAwareTransport {
        cancels: cancels.clone(),
    });
    let cancel = CancellationToken::new();
    let client = Client::connect(
        "test".to_string(),
        transport,
        Duration::from_secs(5),
        call_timeout,
        &cancel,
    )
    .await
    .expect("connect succeeds");
    (client, cancels)
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_call_cancels_the_request_it_gives_up_on() {
    let (client, cancels) = cancel_aware_client(Duration::from_millis(50)).await;
    let cancel = CancellationToken::new();

    let result = client.call("slow", json!({}), &cancel).await;

    assert!(
        matches!(result, Err(CallError::Timeout)),
        "expected Timeout, got {result:?}"
    );
    assert_eq!(
        cancels.load(Ordering::SeqCst),
        1,
        "the transport must observe the cancellation so the call is released"
    );
    assert!(
        !cancel.is_cancelled(),
        "a call timeout must not cancel the caller's own token"
    );
}

struct DeadlineCompletionTransport;

#[async_trait::async_trait]
impl Transport for DeadlineCompletionTransport {
    async fn request(
        &self,
        outbound: Outbound,
        _cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        match outbound.method.as_str() {
            "server/discover" => modern_discover_ok(),
            "tools/list" => Ok(Ok(json!({"tools": []}))),
            "tools/call" => {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(Ok(json!({"content": []})))
            }
            other => panic!("unexpected method {other}"),
        }
    }

    async fn notify(&self, _outbound: Outbound) -> Result<(), CallError> {
        Ok(())
    }

    async fn close(&self) {}
}

#[tokio::test(start_paused = true)]
async fn verified_completion_wins_at_the_deadline() {
    let client = Client::connect(
        "test".to_string(),
        Box::new(DeadlineCompletionTransport),
        Duration::from_secs(5),
        Duration::from_secs(1),
        &CancellationToken::new(),
    )
    .await
    .expect("connect");
    let task = tokio::spawn(async move {
        client
            .call("finishes", json!({}), &CancellationToken::new())
            .await
    });

    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(task.await.unwrap().is_ok());
}

struct BlockingCloseTransport {
    close_calls: Arc<AtomicUsize>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl Transport for BlockingCloseTransport {
    async fn request(
        &self,
        outbound: Outbound,
        _cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        match outbound.method.as_str() {
            "server/discover" => modern_discover_ok(),
            "tools/list" => Ok(Ok(json!({"tools": []}))),
            other => panic!("unexpected method {other}"),
        }
    }

    async fn notify(&self, _outbound: Outbound) -> Result<(), CallError> {
        Ok(())
    }

    async fn close(&self) {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        self.release.notified().await;
    }
}

#[tokio::test]
async fn every_close_caller_awaits_one_shared_completion() {
    let close_calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let client = Arc::new(
        Client::connect(
            "test".to_string(),
            Box::new(BlockingCloseTransport {
                close_calls: close_calls.clone(),
                release: release.clone(),
            }),
            Duration::from_secs(5),
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .expect("connect"),
    );
    let first = {
        let client = client.clone();
        tokio::spawn(async move { client.close().await })
    };
    let second = {
        let client = client.clone();
        tokio::spawn(async move { client.close().await })
    };
    while close_calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    assert!(!first.is_finished());
    assert!(!second.is_finished());

    release.notify_one();
    first.await.unwrap();
    second.await.unwrap();
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
}

struct GraceTransport {
    close_calls: Arc<AtomicUsize>,
    stop_server: bool,
}

#[async_trait::async_trait]
impl Transport for GraceTransport {
    async fn request(
        &self,
        outbound: Outbound,
        _cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        match outbound.method.as_str() {
            "server/discover" => modern_discover_ok(),
            "tools/list" => Ok(Ok(json!({"tools": []}))),
            "tools/call" => std::future::pending().await,
            other => panic!("unexpected method {other}"),
        }
    }

    async fn notify(&self, _outbound: Outbound) -> Result<(), CallError> {
        Ok(())
    }

    fn timeout_stops_server(&self) -> bool {
        self.stop_server
    }

    async fn close(&self) {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
    }
}

async fn grace_client(
    stop_server: bool,
    cancellation_grace: Duration,
) -> (Arc<Client>, Arc<AtomicUsize>) {
    let close_calls = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let client = Client::connect_with_grace(
        "test".to_string(),
        Box::new(GraceTransport {
            close_calls: close_calls.clone(),
            stop_server,
        }),
        Duration::from_secs(5),
        Duration::from_secs(1),
        cancellation_grace,
        &cancel,
    )
    .await
    .expect("connect");
    (Arc::new(client), close_calls)
}

/// A real client over a real HTTP transport. The server answers each
/// request on its own connection: discover, tool list, then a first
/// `tools/call` that it accepts and never answers, then a second one.
#[tokio::test]
async fn a_stalled_http_request_times_out_and_the_next_request_succeeds() {
    use crate::faults::http::{Script, spawn_scripted};

    let discover =
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{MODERN_VERSION}"}}}}"#);
    let server = spawn_scripted(vec![
        Script::status(200, &discover),
        Script::status(200, r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#),
        Script::stall_before_head(),
        Script::status(
            200,
            r#"{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"ok"}]}}"#,
        ),
    ])
    .await;
    let transport = super::super::http::HttpTransport::new(
        server.url.clone(),
        vec![],
        None,
        Duration::from_secs(30),
    )
    .expect("http transport");
    let client = Client::connect_with_grace(
        "test".to_string(),
        Box::new(transport),
        Duration::from_secs(5),
        Duration::from_millis(150),
        Duration::from_millis(100),
        &CancellationToken::new(),
    )
    .await
    .expect("connect");

    let first = client
        .call("slow", json!({}), &CancellationToken::new())
        .await;
    assert_eq!(first.unwrap_err(), CallError::Timeout);

    let second = client
        .call("slow", json!({}), &CancellationToken::new())
        .await
        .expect("the next request succeeds on the same client");
    assert_eq!(
        second.content,
        vec![crate::mcp::ContentBlock::Text {
            text: "ok".to_string()
        }]
    );
    assert_eq!(server.count(), 4);
    client.close().await;
}

#[tokio::test(start_paused = true)]
async fn http_timeout_drops_only_one_request_after_grace() {
    let grace = Duration::from_millis(25);
    let (client, close_calls) = grace_client(false, grace).await;
    let task = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .call("slow", json!({}), &CancellationToken::new())
                .await
        })
    };

    tokio::time::advance(Duration::from_secs(1) + grace - Duration::from_millis(1)).await;
    assert!(!task.is_finished(), "the configured grace must be honored");
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(matches!(task.await.unwrap(), Err(CallError::Timeout)));
    assert_eq!(close_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn uncooperative_stdio_parent_cancel_gets_bounded_cleanup() {
    let grace = Duration::from_millis(25);
    let (client, close_calls) = grace_client(true, grace).await;
    let cancel = CancellationToken::new();
    let task = {
        let client = client.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { client.call("slow", json!({}), &cancel).await })
    };
    tokio::task::yield_now().await;

    cancel.cancel();
    tokio::time::advance(grace - Duration::from_millis(1)).await;
    assert!(!task.is_finished(), "caller cancellation must allow grace");
    tokio::time::advance(Duration::from_millis(1)).await;

    assert!(matches!(task.await.unwrap(), Err(CallError::Cancelled)));
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stdio_waits_for_grace_when_transport_acknowledges_cancellation() {
    let grace = Duration::from_secs(1);
    let cancels = Arc::new(AtomicUsize::new(0));
    let close_calls = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(
        Client::connect_with_grace(
            "test".to_string(),
            Box::new(CancelledStdioTransport {
                cancels: cancels.clone(),
                close_calls: close_calls.clone(),
            }),
            Duration::from_secs(5),
            Duration::from_secs(60),
            grace,
            &CancellationToken::new(),
        )
        .await
        .expect("connect"),
    );
    let cancel = CancellationToken::new();
    let task = {
        let client = client.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { client.call("slow", json!({}), &cancel).await })
    };
    tokio::task::yield_now().await;
    cancel.cancel();
    while cancels.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!task.is_finished(), "stdio cancellation must retain grace");

    assert!(matches!(task.await.unwrap(), Err(CallError::Cancelled)));
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        client
            .call("later", json!({}), &CancellationToken::new())
            .await,
        Err(CallError::Stopping)
    ));
}

struct CancelledStdioTransport {
    cancels: Arc<AtomicUsize>,
    close_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Transport for CancelledStdioTransport {
    async fn request(
        &self,
        outbound: Outbound,
        cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        match outbound.method.as_str() {
            "server/discover" => modern_discover_ok(),
            "tools/list" => Ok(Ok(json!({"tools": []}))),
            "tools/call" => {
                cancel.cancelled().await;
                self.cancels.fetch_add(1, Ordering::SeqCst);
                Err(CallError::Cancelled)
            }
            other => panic!("unexpected method {other}"),
        }
    }

    async fn notify(&self, _outbound: Outbound) -> Result<(), CallError> {
        Ok(())
    }

    fn timeout_stops_server(&self) -> bool {
        true
    }

    async fn close(&self) {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn stdio_timeout_stops_once_and_rejects_later_calls() {
    let grace = Duration::from_millis(25);
    let (client, close_calls) = grace_client(true, grace).await;
    let task = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .call("slow", json!({}), &CancellationToken::new())
                .await
        })
    };

    tokio::time::advance(Duration::from_secs(1) + grace).await;
    assert!(matches!(task.await.unwrap(), Err(CallError::Timeout)));
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    let later = client
        .call("later", json!({}), &CancellationToken::new())
        .await;
    assert!(matches!(later, Err(CallError::Stopping)));
    client.close().await;
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_call_is_not_reported_as_a_timeout() {
    let (client, cancels) = cancel_aware_client(Duration::from_secs(60)).await;
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = client.call("slow", json!({}), &cancel).await;

    assert!(
        matches!(result, Err(CallError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert_eq!(cancels.load(Ordering::SeqCst), 1);
}
