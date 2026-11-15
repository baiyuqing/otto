use std::sync::atomic::AtomicUsize;

use super::*;
use otto_faults::http as testserver;

struct FakeBearer {
    bearer_result: Mutex<Result<String, CallError>>,
    refresh_result: Mutex<Result<String, CallError>>,
    refresh_calls: Mutex<u32>,
}

impl FakeBearer {
    fn ok(token: &str) -> Self {
        Self {
            bearer_result: Mutex::new(Ok(token.to_string())),
            refresh_result: Mutex::new(Ok(token.to_string())),
            refresh_calls: Mutex::new(0),
        }
    }
}

#[async_trait]
impl BearerSource for FakeBearer {
    async fn bearer(&self, _cancel: &CancellationToken) -> Result<String, CallError> {
        self.bearer_result.lock().unwrap().clone()
    }

    async fn refresh(
        &self,
        _rejected: &str,
        _cancel: &CancellationToken,
    ) -> Result<String, CallError> {
        *self.refresh_calls.lock().unwrap() += 1;
        self.refresh_result.lock().unwrap().clone()
    }

    fn secrets(&self) -> Vec<String> {
        vec![]
    }
}

fn modern_outbound(method: &str, params: Value) -> Outbound {
    Outbound {
        method: method.to_string(),
        params,
        era: Some(Era::Modern),
    }
}

#[tokio::test]
async fn modern_headers_and_static_headers_sent() {
    let server = testserver::spawn(|req| {
        assert_eq!(req.header("mcp-protocol-version"), MODERN_VERSION);
        assert_eq!(req.header("mcp-method"), "tools/call");
        assert_eq!(req.header("mcp-name"), "my_tool");
        assert_eq!(req.header("x-custom"), "value");
        testserver::json_response(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
    })
    .await;
    let transport = HttpTransport::new(
        server.url.clone(),
        vec![("X-Custom".to_string(), "value".to_string())],
        None,
        Duration::from_secs(5),
    )
    .unwrap();
    let outbound = modern_outbound("tools/call", json!({"name": "my_tool", "arguments": {}}));
    let cancel = CancellationToken::new();
    let result = transport.request(outbound, &cancel).await.unwrap();
    assert_eq!(result.unwrap(), json!({}));
}

#[tokio::test]
async fn tool_name_with_control_char_uses_base64_header() {
    let expected = base64::engine::general_purpose::STANDARD.encode("tool\nname");
    let server = testserver::spawn(move |req| {
        assert_eq!(req.header("mcp-name"), format!("=?base64?{expected}?="));
        testserver::json_response(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
    })
    .await;
    let transport =
        HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
    let outbound = modern_outbound("tools/call", json!({"name": "tool\nname", "arguments": {}}));
    let cancel = CancellationToken::new();
    transport.request(outbound, &cancel).await.unwrap().unwrap();
}

#[tokio::test]
async fn sse_response_skips_keepalive_and_unrelated_events() {
    let server = testserver::spawn(|_req| {
            testserver::sse_response(
                ": keep-alive\n\nevent: other\ndata: {\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{}}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
            )
        })
        .await;
    let transport =
        HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
    let outbound = modern_outbound("tools/list", json!({}));
    let cancel = CancellationToken::new();
    let result = transport.request(outbound, &cancel).await.unwrap();
    assert_eq!(result.unwrap(), json!({"ok": true}));
}

#[tokio::test]
async fn legacy_session_id_captured_and_replayed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = Arc::clone(&calls);
    let server = testserver::spawn(move |req| {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                assert_eq!(req.header("mcp-session-id"), "");
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: sess-123\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                assert_eq!(req.header("mcp-session-id"), "sess-123");
                testserver::json_response(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#)
            }
        })
        .await;
    let transport =
        HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
    let cancel = CancellationToken::new();
    let outbound = Outbound {
        method: "tools/list".to_string(),
        params: json!({}),
        era: Some(Era::Legacy("2025-11-25".to_string())),
    };
    transport
        .request(outbound.clone(), &cancel)
        .await
        .unwrap()
        .unwrap();
    transport.request(outbound, &cancel).await.unwrap().unwrap();
}

#[tokio::test]
async fn legacy_initialize_sends_no_protocol_version_header() {
    let server = testserver::spawn(|req| {
        assert_eq!(req.header("mcp-protocol-version"), "");
        assert_eq!(req.header("mcp-method"), "");
        testserver::json_response(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}}"#,
        )
    })
    .await;
    let transport =
        HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
    let outbound = Outbound {
        method: "initialize".to_string(),
        params: jsonrpc::initialize_params(),
        era: None,
    };
    let cancel = CancellationToken::new();
    let result = transport.request(outbound, &cancel).await.unwrap();
    assert_eq!(result.unwrap(), json!({"protocolVersion": "2025-11-25"}));
}

#[tokio::test]
async fn probe_400_with_unsupported_version_falls_back() {
    let server = testserver::spawn(|_req| {
            testserver::status_response(
                400,
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32020,"message":"unsupported","data":{"supported":["2025-11-25"]}}}"#,
            )
        })
        .await;
    let transport =
        HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
    let outbound = Outbound {
        method: "tools/list".to_string(),
        params: json!({}),
        era: None,
    };
    let cancel = CancellationToken::new();
    let result = transport.request(outbound, &cancel).await.unwrap();
    let err = result.unwrap_err();
    assert_eq!(err.code, jsonrpc::UNSUPPORTED_PROTOCOL_VERSION);
}

#[tokio::test]
async fn probe_400_without_body_falls_back_to_sentinel() {
    let server = testserver::spawn(|_req| testserver::status_response(400, "")).await;
    let transport =
        HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
    let outbound = Outbound {
        method: "tools/list".to_string(),
        params: json!({}),
        era: None,
    };
    let cancel = CancellationToken::new();
    let result = transport.request(outbound, &cancel).await.unwrap();
    let err = result.unwrap_err();
    assert_eq!(err.code, jsonrpc::INVALID_PARAMS);
}

#[tokio::test]
async fn bearer_token_sent_when_configured() {
    let server = testserver::spawn(|req| {
        assert_eq!(req.header("authorization"), "Bearer tok-abc");
        testserver::json_response(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
    })
    .await;
    let bearer = Arc::new(FakeBearer::ok("tok-abc"));
    let transport = HttpTransport::new(
        server.url.clone(),
        vec![],
        Some(bearer),
        Duration::from_secs(5),
    )
    .unwrap();
    let outbound = modern_outbound("tools/list", json!({}));
    let cancel = CancellationToken::new();
    transport.request(outbound, &cancel).await.unwrap().unwrap();
}

#[tokio::test]
async fn unauthorized_then_refresh_then_retry_succeeds() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = Arc::clone(&calls);
    let server = testserver::spawn(move |req| {
        let n = calls_clone.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            testserver::status_response(401, "")
        } else {
            assert_eq!(req.header("authorization"), "Bearer new-token");
            testserver::json_response(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#)
        }
    })
    .await;
    let bearer = Arc::new(FakeBearer {
        bearer_result: Mutex::new(Ok("old-token".to_string())),
        refresh_result: Mutex::new(Ok("new-token".to_string())),
        refresh_calls: Mutex::new(0),
    });
    let transport = HttpTransport::new(
        server.url.clone(),
        vec![],
        Some(Arc::clone(&bearer) as Arc<dyn BearerSource>),
        Duration::from_secs(5),
    )
    .unwrap();
    let outbound = modern_outbound("tools/list", json!({}));
    let cancel = CancellationToken::new();
    let result = transport.request(outbound, &cancel).await.unwrap();
    assert_eq!(result.unwrap(), json!({"ok": true}));
    assert_eq!(*bearer.refresh_calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn unauthorized_then_refresh_fails_needs_login() {
    let server = testserver::spawn(|_req| testserver::status_response(401, "")).await;
    let bearer = Arc::new(FakeBearer {
        bearer_result: Mutex::new(Ok("old".to_string())),
        refresh_result: Mutex::new(Err(CallError::NeedsLogin)),
        refresh_calls: Mutex::new(0),
    });
    let transport = HttpTransport::new(
        server.url.clone(),
        vec![],
        Some(bearer),
        Duration::from_secs(5),
    )
    .unwrap();
    let outbound = modern_outbound("tools/list", json!({}));
    let cancel = CancellationToken::new();
    let err = transport.request(outbound, &cancel).await.unwrap_err();
    assert_eq!(err, CallError::NeedsLogin);
}

#[tokio::test]
async fn unauthorized_without_bearer_is_transport_error() {
    let server = testserver::spawn(|_req| testserver::status_response(401, "")).await;
    let transport =
        HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
    let outbound = modern_outbound("tools/list", json!({}));
    let cancel = CancellationToken::new();
    let err = transport.request(outbound, &cancel).await.unwrap_err();
    match err {
        CallError::Transport(msg) => assert!(msg.contains("401")),
        other => panic!("expected Transport, got {other:?}"),
    }
}

#[tokio::test]
async fn forbidden_insufficient_scope_reports_scope() {
    let server = testserver::spawn(|_req| {
            "HTTP/1.1 403 X\r\nWWW-Authenticate: Bearer error=\"insufficient_scope\", scope=\"mcp:tools\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string()
        })
        .await;
    let transport =
        HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
    let outbound = modern_outbound("tools/list", json!({}));
    let cancel = CancellationToken::new();
    let err = transport.request(outbound, &cancel).await.unwrap_err();
    match err {
        CallError::Transport(msg) => assert!(msg.contains("mcp:tools")),
        other => panic!("expected Transport, got {other:?}"),
    }
}

#[tokio::test]
async fn notify_accepts_202() {
    let server = testserver::spawn(|_req| testserver::status_response(202, "")).await;
    let transport =
        HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
    let outbound = modern_outbound("notifications/initialized", Value::Null);
    transport.notify(outbound).await.unwrap();
}

#[tokio::test]
async fn server_error_is_transport() {
    let server = testserver::spawn(|_req| testserver::status_response(500, "")).await;
    let transport =
        HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
    let outbound = modern_outbound("tools/list", json!({}));
    let cancel = CancellationToken::new();
    let err = transport.request(outbound, &cancel).await.unwrap_err();
    match err {
        CallError::Transport(msg) => assert!(msg.contains("500")),
        other => panic!("expected Transport, got {other:?}"),
    }
}

#[tokio::test]
async fn cancel_during_send_returns_cancelled() {
    let url = testserver::spawn_hanging().await;
    let transport = HttpTransport::new(url, vec![], None, Duration::from_secs(30)).unwrap();
    let outbound = modern_outbound("tools/list", json!({}));
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_clone.cancel();
    });
    let err = transport.request(outbound, &cancel).await.unwrap_err();
    assert_eq!(err, CallError::Cancelled);
}

#[tokio::test]
async fn redirect_not_followed_is_transport_error() {
    let server = testserver::spawn(|_req| {
        testserver::redirect_response(302, "http://127.0.0.1:1/somewhere")
    })
    .await;
    let transport =
        HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5)).unwrap();
    let outbound = modern_outbound("tools/list", json!({}));
    let cancel = CancellationToken::new();
    let err = transport.request(outbound, &cancel).await.unwrap_err();
    match err {
        CallError::Transport(msg) => assert!(msg.contains("302")),
        other => panic!("expected Transport, got {other:?}"),
    }
}

#[tokio::test]
async fn json_body_over_cap_is_rejected() {
    let oversized =
        json!({"jsonrpc": "2.0", "id": 1, "result": {"pad": "x".repeat(200)}}).to_string();
    let server = testserver::spawn(move |_req| testserver::json_response(&oversized)).await;
    let transport = HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5))
        .unwrap()
        .with_max_body_bytes(100);
    let outbound = modern_outbound("tools/list", json!({}));
    let cancel = CancellationToken::new();
    let err = transport.request(outbound, &cancel).await.unwrap_err();
    match err {
        CallError::Transport(msg) => assert_eq!(msg, BODY_TOO_LARGE),
        other => panic!("expected Transport, got {other:?}"),
    }
}

#[tokio::test]
async fn bad_request_body_over_cap_is_rejected() {
    let oversized = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {"code": -32020, "message": "x".repeat(200), "data": null},
    })
    .to_string();
    let server = testserver::spawn(move |_req| testserver::status_response(400, &oversized)).await;
    let transport = HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5))
        .unwrap()
        .with_max_body_bytes(100);
    let outbound = Outbound {
        method: "tools/list".to_string(),
        params: json!({}),
        era: None,
    };
    let cancel = CancellationToken::new();
    let err = transport.request(outbound, &cancel).await.unwrap_err();
    match err {
        CallError::Transport(msg) => assert_eq!(msg, BODY_TOO_LARGE),
        other => panic!("expected Transport, got {other:?}"),
    }
}

#[tokio::test]
async fn sse_stream_over_body_cap_is_rejected() {
    let oversized = format!(
        "data: {}\n\ndata: {{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}\n\n",
        "x".repeat(200)
    );
    let server = testserver::spawn(move |_req| testserver::sse_response(&oversized)).await;
    let transport = HttpTransport::new(server.url.clone(), vec![], None, Duration::from_secs(5))
        .unwrap()
        .with_max_body_bytes(100);
    let outbound = modern_outbound("tools/list", json!({}));
    let cancel = CancellationToken::new();
    let err = transport.request(outbound, &cancel).await.unwrap_err();
    match err {
        CallError::Transport(msg) => assert_eq!(msg, BODY_TOO_LARGE),
        other => panic!("expected Transport, got {other:?}"),
    }
}
