//! The per-server client: era negotiation, `tools/list`, `tools/call`.
//!
//! Owned by the stdio/client step; see `docs/specs/2026-09-19-mcp-design.md`.
//! Era negotiation ([`Client::connect`]) probes `server/discover`: a modern
//! server answers it, a legacy server rejects it with a recognizable error,
//! and only then is the `initialize`/`notifications/initialized` handshake
//! attempted. Every later request is stamped with the negotiated era so the
//! transport can frame it correctly (modern requests carry `_meta`).

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use super::jsonrpc::{self, METHOD_NOT_FOUND, UNSUPPORTED_PROTOCOL_VERSION};
use super::{CallError, CallOutcome, Era, Outbound, ToolInfo, ToolServer, Transport};

/// How long era negotiation waits for `server/discover` before assuming the
/// server does not understand it and falling back to the legacy handshake.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);
/// Grace for cooperative per-request cancellation before hard cleanup.
const CALL_CANCELLATION_GRACE: Duration = Duration::from_secs(5);
/// Connect failure cleanup is itself bounded, so a broken transport cannot
/// turn a connect deadline into an unbounded wait.
const CONNECT_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

const ACTIVE: u8 = 0;
const STOPPING: u8 = 1;

/// One connected server: its transport, negotiated era, and advertised tools.
pub struct Client {
    name: String,
    transport: Box<dyn Transport>,
    era: Era,
    tools: Vec<ToolInfo>,
    call_timeout: Duration,
    lifecycle: AtomicU8,
    close_done: OnceCell<()>,
}

impl Client {
    /// Connects to a server: negotiates the protocol era, then fetches its
    /// full tool list (paging through `nextCursor`). `connect_timeout` bounds
    /// the whole connect (probe, handshake, and tool listing together), not
    /// just the initial handshake. On any failure, including a timeout, the
    /// transport is closed before the error is returned so a stdio child
    /// never outlives a failed connect.
    pub async fn connect(
        name: String,
        transport: Box<dyn Transport>,
        connect_timeout: Duration,
        call_timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Client, CallError> {
        let outcome = tokio::time::timeout(connect_timeout, async {
            let era = negotiate_era(transport.as_ref(), connect_timeout, cancel).await?;
            let tools = fetch_all_tools(transport.as_ref(), &era, cancel).await?;
            Ok((era, tools))
        })
        .await;

        match outcome {
            Ok(Ok((era, tools))) => Ok(Client {
                name,
                transport,
                era,
                tools,
                call_timeout,
                lifecycle: AtomicU8::new(ACTIVE),
                close_done: OnceCell::new(),
            }),
            Ok(Err(error)) => {
                close_failed_connect(transport.as_ref()).await;
                Err(error)
            }
            Err(_) => {
                close_failed_connect(transport.as_ref()).await;
                Err(CallError::Timeout)
            }
        }
    }

    /// Sends `outbound`, cooperatively cancels it at `call_timeout`, then
    /// affords cancellation a bounded grace period. HTTP may abandon just
    /// this request after grace; stdio must stop the shared stream.
    async fn request_within_timeout(
        &self,
        outbound: Outbound,
        cancel: &CancellationToken,
    ) -> Result<Result<Value, jsonrpc::RpcError>, CallError> {
        if self.lifecycle.load(Ordering::Acquire) != ACTIVE {
            return Err(CallError::Stopping);
        }

        let child = cancel.child_token();
        let request = self.transport.request(outbound, &child);
        let mut request = std::pin::pin!(request);
        let deadline = tokio::time::sleep(self.call_timeout);
        let mut deadline = std::pin::pin!(deadline);
        let mut caller_cancelled = tokio::select! {
            // A response that is already available is authoritative even when
            // cancellation or the deadline becomes ready in the same poll.
            biased;
            outcome = &mut request => return outcome,
            () = cancel.cancelled() => true,
            () = &mut deadline => false,
        };

        child.cancel();
        let grace = tokio::time::sleep(CALL_CANCELLATION_GRACE);
        let mut grace = std::pin::pin!(grace);
        tokio::select! {
            // Verified completion wins if completion and grace are both ready.
            biased;
            outcome = &mut request => {
                match outcome {
                    // Transport-level cancellation only proves that the local
                    // waiter was released, not that the remote effect stopped.
                    // Keep the full grace before applying hard cleanup.
                    Err(CallError::Cancelled) if caller_cancelled => {
                        return Err(CallError::Cancelled);
                    }
                    Err(CallError::Cancelled) => {
                        tokio::select! {
                            biased;
                            () = cancel.cancelled() => caller_cancelled = true,
                            () = &mut grace => {}
                        }
                    }
                    outcome => return outcome,
                }
            }
            () = cancel.cancelled(), if !caller_cancelled => {
                caller_cancelled = true;
                (&mut grace).await;
            }
            () = &mut grace => {}
        }

        if self.transport.timeout_stops_server() {
            let _ = self.lifecycle.compare_exchange(
                ACTIVE,
                STOPPING,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.close_transport().await;
        }
        if caller_cancelled {
            Err(CallError::Cancelled)
        } else {
            Err(CallError::Timeout)
        }
    }

    async fn close_transport(&self) {
        self.close_done
            .get_or_init(|| async { self.transport.close().await })
            .await;
    }

    pub fn era(&self) -> &Era {
        &self.era
    }

    pub fn tools(&self) -> &[ToolInfo] {
        &self.tools
    }

    /// Shuts the server down. Idempotent.
    pub async fn close(&self) {
        self.lifecycle.store(STOPPING, Ordering::Release);
        self.close_transport().await;
    }
}

#[async_trait::async_trait]
impl ToolServer for Client {
    fn name(&self) -> &str {
        &self.name
    }

    async fn call(
        &self,
        tool: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<CallOutcome, CallError> {
        let params = era_params(&self.era, json!({"name": tool, "arguments": arguments}));
        let outbound = Outbound {
            method: "tools/call".to_string(),
            params,
            era: Some(self.era.clone()),
        };
        let result = self.request_within_timeout(outbound, cancel).await?;
        let result = result.map_err(|error| CallError::Rpc {
            code: error.code,
            message: error.message,
        })?;
        jsonrpc::decode_call_result(&result, &self.era).map_err(CallError::Transport)
    }
}

async fn close_failed_connect(transport: &dyn Transport) {
    let _ = tokio::time::timeout(CONNECT_CLOSE_TIMEOUT, transport.close()).await;
}

/// Wraps `params` with `_meta` on the modern era; leaves legacy params as is.
fn era_params(era: &Era, params: Value) -> Value {
    match era {
        Era::Modern => jsonrpc::modern_meta(params),
        Era::Legacy(_) => params,
    }
}

/// Probes `server/discover`; on a recognized rejection (or a timeout), falls
/// back to the legacy `initialize` handshake. Any other outcome is a
/// transport failure: the server exists but does not speak a supported era.
async fn negotiate_era(
    transport: &dyn Transport,
    connect_timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Era, CallError> {
    let discover = Outbound {
        method: "server/discover".to_string(),
        params: jsonrpc::modern_meta(json!({})),
        era: None,
    };
    let outcome = tokio::time::timeout(DISCOVER_TIMEOUT, transport.request(discover, cancel)).await;

    let is_legacy_rejection = match &outcome {
        Ok(Ok(Ok(_))) => return Ok(Era::Modern),
        Ok(Ok(Err(error))) => {
            error.code == METHOD_NOT_FOUND
                || error.code == jsonrpc::INVALID_PARAMS
                || (error.code == UNSUPPORTED_PROTOCOL_VERSION && !jsonrpc::supports_modern(error))
        }
        Ok(Err(CallError::Timeout)) | Err(_) => true,
        Ok(Err(_)) => false,
    };

    if !is_legacy_rejection {
        let message = match outcome {
            // A recognized RPC error, reported with just code and message:
            // `data` (which can carry the server's whole error payload) is
            // never included, and the message is bounded so a hostile or
            // buggy server cannot grow the status/HTTP-error text without
            // limit.
            Ok(Ok(Err(error))) => format!(
                "server/discover failed: {} {}",
                error.code,
                truncated(&error.message, MAX_DISCOVER_ERROR_BYTES)
            ),
            // `Ok(Ok(Ok(_)))` returned above and `Ok(Err(Timeout)) | Err(_)`
            // are legacy rejections handled by `is_legacy_rejection`, so the
            // only other case reaching here is a transport-level error.
            Ok(Err(other)) => format!(
                "server/discover failed: {}",
                truncated(&other.to_string(), MAX_DISCOVER_ERROR_BYTES)
            ),
            _ => unreachable!("legacy rejection cases are handled above"),
        };
        return Err(CallError::Transport(message));
    }

    legacy_handshake(transport, connect_timeout, cancel).await
}

/// How much of an RPC error message or transport error text is kept when
/// reporting a `server/discover` failure. Bytes, not chars, but the cut
/// always lands on a char boundary.
const MAX_DISCOVER_ERROR_BYTES: usize = 200;

/// `text` truncated to at most `max` bytes, cut on a char boundary.
fn truncated(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

async fn legacy_handshake(
    transport: &dyn Transport,
    connect_timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Era, CallError> {
    let initialize = Outbound {
        method: "initialize".to_string(),
        params: jsonrpc::initialize_params(),
        era: None,
    };
    let result = tokio::time::timeout(connect_timeout, transport.request(initialize, cancel))
        .await
        .map_err(|_| CallError::Timeout)??
        .map_err(|error| CallError::Rpc {
            code: error.code,
            message: error.message,
        })?;
    let version = jsonrpc::decode_initialize(&result).map_err(CallError::Transport)?;

    transport
        .notify(Outbound {
            method: "notifications/initialized".to_string(),
            params: Value::Null,
            era: Some(Era::Legacy(version.clone())),
        })
        .await?;

    Ok(Era::Legacy(version))
}

/// The most `tools/list` pages a single connect will follow before giving up.
/// A well-behaved server pages a tool catalog in single digits to low
/// hundreds; this only guards against a server that never terminates paging.
const MAX_TOOLS_LIST_PAGES: usize = 1000;

/// Fetches every page of `tools/list`, following `nextCursor` until it is
/// `None`. Guards against a server that never terminates paging: a page
/// count over [`MAX_TOOLS_LIST_PAGES`] or a `nextCursor` repeating the cursor
/// just sent are both reported as transport failures instead of looping
/// forever.
async fn fetch_all_tools(
    transport: &dyn Transport,
    era: &Era,
    cancel: &CancellationToken,
) -> Result<Vec<ToolInfo>, CallError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_TOOLS_LIST_PAGES {
        let sent_cursor = cursor.clone();
        let mut params = json!({});
        if let Some(cursor) = &sent_cursor {
            params["cursor"] = json!(cursor);
        }
        let outbound = Outbound {
            method: "tools/list".to_string(),
            params: era_params(era, params),
            era: Some(era.clone()),
        };
        let result = transport
            .request(outbound, cancel)
            .await?
            .map_err(|error| CallError::Rpc {
                code: error.code,
                message: error.message,
            })?;
        let (mut page, next_cursor) =
            jsonrpc::decode_tools_list(&result).map_err(CallError::Transport)?;
        tools.append(&mut page);
        match next_cursor {
            Some(next) if Some(&next) == sent_cursor.as_ref() => {
                return Err(CallError::Transport(
                    "tools/list returned the same cursor twice".to_string(),
                ));
            }
            Some(next) => cursor = Some(next),
            None => return Ok(tools),
        }
    }
    Err(CallError::Transport(format!(
        "tools/list exceeded {MAX_TOOLS_LIST_PAGES} pages"
    )))
}

#[cfg(test)]
mod tests {
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

    async fn grace_client(stop_server: bool) -> (Arc<Client>, Arc<AtomicUsize>) {
        let close_calls = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let client = Client::connect(
            "test".to_string(),
            Box::new(GraceTransport {
                close_calls: close_calls.clone(),
                stop_server,
            }),
            Duration::from_secs(5),
            Duration::from_secs(1),
            &cancel,
        )
        .await
        .expect("connect");
        (Arc::new(client), close_calls)
    }

    #[tokio::test(start_paused = true)]
    async fn http_timeout_drops_only_one_request_after_grace() {
        let (client, close_calls) = grace_client(false).await;
        let task = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .call("slow", json!({}), &CancellationToken::new())
                    .await
            })
        };

        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(!task.is_finished(), "the five-second grace must be honored");
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(task.await.unwrap(), Err(CallError::Timeout)));
        assert_eq!(close_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn uncooperative_stdio_parent_cancel_gets_bounded_cleanup() {
        let (client, close_calls) = grace_client(true).await;
        let cancel = CancellationToken::new();
        let task = {
            let client = client.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { client.call("slow", json!({}), &cancel).await })
        };
        tokio::task::yield_now().await;

        cancel.cancel();
        tokio::time::advance(CALL_CANCELLATION_GRACE - Duration::from_millis(1)).await;
        assert!(!task.is_finished(), "caller cancellation must allow grace");
        tokio::time::advance(Duration::from_millis(1)).await;

        assert!(matches!(task.await.unwrap(), Err(CallError::Cancelled)));
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn stdio_timeout_stops_once_and_rejects_later_calls() {
        let (client, close_calls) = grace_client(true).await;
        let task = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .call("slow", json!({}), &CancellationToken::new())
                    .await
            })
        };

        tokio::time::advance(Duration::from_secs(6)).await;
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
}
