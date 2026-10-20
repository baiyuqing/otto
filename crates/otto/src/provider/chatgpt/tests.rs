
use super::*;
use crate::auth::Credentials;
use crate::faults::http::{self as testserver, sse_response};
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

#[tokio::test]
async fn catalog_uses_subscription_auth_and_official_models_shape() {
    let server = testserver::spawn(|_| testserver::json_response( r#"{"models":[{"slug":"fixture-text","visibility":"list","input_modalities":["text"],"supported_in_api":false,"description":"catalog-access-secret"}]}"#)).await;
    let client = Client::with_base_url(
        &server.url,
        static_tokens("catalog-access-secret"),
        "catalog-account-secret",
    );
    let catalog = client.catalog(&CancellationToken::new()).await.unwrap();
    assert_eq!(catalog.models[0].id, "fixture-text");
    assert_eq!(catalog.models[0].metadata["description"], "[REDACTED]");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].target, "/models?client_version=0.163.0");
    assert_eq!(
        requests[0].header("authorization"),
        "Bearer catalog-access-secret"
    );
    assert_eq!(
        requests[0].header("chatgpt-account-id"),
        "catalog-account-secret"
    );
}

#[tokio::test]
async fn catalog_cache_survives_client_restart_and_rejects_stale_or_other_accounts() {
    let server =
        testserver::spawn(|_| testserver::json_response(r#"{"models":[{"slug":"fixture-text"}]}"#))
            .await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache/models.json");
    let client = |account: &str| {
        Client::with_base_url(&server.url, static_tokens("access"), account)
            .with_catalog_cache(path.clone())
    };
    assert_eq!(
        client("account-a")
            .catalog(&CancellationToken::new())
            .await
            .unwrap()
            .models[0]
            .id,
        "fixture-text"
    );
    assert_eq!(server.count(), 1);
    client("account-a")
        .catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(server.count(), 1, "fresh JSON cache should skip HTTP");

    let mut entry: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    entry["fetched_at"] = 0.into();
    std::fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();
    client("account-a")
        .catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(server.count(), 2, "expired JSON should refresh");

    client("account-b")
        .catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(server.count(), 3, "another account must refresh");
}

#[tokio::test]
async fn empty_catalog_is_not_persisted_and_rejection_invalidates_cache() {
    let empty = testserver::spawn(|_| testserver::json_response(r#"{"models":[]}"#)).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache/models.json");
    let client = Client::with_base_url(&empty.url, static_tokens("access"), "account")
        .with_catalog_cache(path.clone());
    client.catalog(&CancellationToken::new()).await.unwrap();
    client.catalog(&CancellationToken::new()).await.unwrap();
    assert_eq!(empty.count(), 2);
    assert!(!path.exists());

    let server = testserver::spawn(|request| {
        if request.target.starts_with("/models") {
            testserver::json_response(r#"{"models":[{"slug":"fixture-text"}]}"#)
        } else {
            testserver::status_response(404, "unavailable")
        }
    })
    .await;
    let client = Client::with_base_url(&server.url, static_tokens("access"), "account")
        .with_catalog_cache(path.clone());
    client.catalog(&CancellationToken::new()).await.unwrap();
    assert!(path.exists());
    client
        .complete(&model_request(), &mut |_| {}, &CancellationToken::new())
        .await;
    assert!(!path.exists(), "rejected model should invalidate cache");
}

#[tokio::test]
async fn catalog_missing_auth_and_pre_cancel_are_safe() {
    let client = Client::with_base_url("http://127.0.0.1:1", static_tokens(""), "acct");
    assert!(
        client
            .catalog(&CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("otto login")
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        client.catalog(&cancel).await,
        Err(ProviderError::Cancelled)
    ));
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
            parameters: serde_json::value::RawValue::from_string(r#"{"type":"object"}"#.to_owned())
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
    assert!(response.cached_input_tokens_present);
    assert!(!events.is_empty());
}

/// A requested remote compaction item reaches the response, tagged with
/// this provider.
#[tokio::test]
async fn returns_the_requested_remote_compaction() {
    const STREAM: &str = concat!(
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"id\":\"cmp_1\",\"encrypted_content\":\"opaque\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    let server = testserver::spawn(|_| sse_response(STREAM)).await;
    let client = Client::with_base_url(&server.url, static_tokens("test-token"), "acct-1");
    let request = Request {
        request_remote_compaction: true,
        ..model_request()
    };

    let (settlement, _) = complete(&client, &request).await;

    assert_eq!(
        settlement.result.unwrap().remote_compaction,
        Some(otto_core::session::RemoteCompaction {
            provider: "chatgpt".into(),
            id: "cmp_1".into(),
            encrypted_content: "opaque".into(),
        })
    );
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

    #[derive(Default)]
    struct Phases(std::sync::Mutex<Vec<(otto_core::diagnostics::Phase, Option<u16>)>>);
    impl otto_core::diagnostics::Observer for Phases {
        fn activity(&self, phase: otto_core::diagnostics::Phase, _: usize, status: Option<u16>) {
            self.0.lock().unwrap().push((phase, status));
        }
    }
    let phases = Phases::default();
    let settlement = client
        .complete_observed(
            &Request::default(),
            &mut |_| {},
            &CancellationToken::new(),
            &phases,
        )
        .await;
    assert!(
        phases
            .0
            .lock()
            .unwrap()
            .iter()
            .any(
                |(phase, code)| matches!(phase, otto_core::diagnostics::Phase::Headers)
                    && *code == Some(401)
            )
    );

    let error = settlement.result.unwrap_err();
    assert_eq!(error.to_string(), "chatgpt responses HTTP 401");
}

#[tokio::test]
async fn a_redirect_is_reported_as_its_status_without_forwarding_the_request() {
    let target = testserver::spawn(|_| testserver::status_response(500, "unreachable")).await;
    let location = format!("{}/responses", target.url);
    let source = testserver::spawn(move |_| testserver::redirect_response(307, &location)).await;
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

#[test]
fn transient_http_statuses_and_retry_after_are_classified_without_a_body() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, "45".parse().unwrap());
    for (status, reason) in [
        (429, "HTTP 429"),
        (500, "HTTP 500"),
        (502, "HTTP 502"),
        (503, "HTTP 503"),
        (504, "HTTP 504"),
    ] {
        let retry = transient_http_retry(status, &headers).expect("retryable status");
        assert_eq!(retry.reason, reason);
        assert_eq!(retry.retry_after, Some(Duration::from_secs(45)));
    }
    let retry = transient_http_retry(503, &headers).expect("retryable status");
    assert_eq!(
        crate::retry::next_delay(
            RETRY_POLICY,
            1,
            retry.retry_after,
            Duration::MAX,
            Duration::from_secs(1),
            &mut Backoff,
        ),
        Some(Duration::from_secs(30))
    );
    for status in [400, 401, 408, 409, 501] {
        assert!(
            transient_http_retry(status, &headers).is_none(),
            "status {status}"
        );
    }
}

#[tokio::test]
async fn non_transient_http_statuses_are_not_retried() {
    for status in [400, 401, 408, 409, 501] {
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
/// Real time keeps socket failures distinct from artificial read timeouts.
#[tokio::test]
async fn a_transport_failure_stops_after_six_retries() {
    let client = Client::with_base_url("http://127.0.0.1:1", static_tokens("token"), "acct-1");
    let (settlement, events) = complete(&client, &Request::default()).await;
    assert_eq!(settlement.attempts, 7);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Unknown
    );
    assert_eq!(
        settlement.result.unwrap_err().to_string(),
        "chatgpt request failed (connection interrupted; 7 attempts; retry budget exhausted)"
    );
    assert_eq!(
        events,
        vec![
            StreamEvent::Retry {
                attempt: 2,
                max_attempts: 7,
                delay: Duration::from_secs(1),
                reason: "connection interrupted".into(),
                interrupted_output: false,
            },
            StreamEvent::Retry {
                attempt: 3,
                max_attempts: 7,
                delay: Duration::from_secs(2),
                reason: "connection interrupted".into(),
                interrupted_output: false,
            },
            StreamEvent::Retry {
                attempt: 4,
                max_attempts: 7,
                delay: Duration::from_secs(4),
                reason: "connection interrupted".into(),
                interrupted_output: false,
            },
            StreamEvent::Retry {
                attempt: 5,
                max_attempts: 7,
                delay: Duration::from_secs(8),
                reason: "connection interrupted".into(),
                interrupted_output: false,
            },
            StreamEvent::Retry {
                attempt: 6,
                max_attempts: 7,
                delay: Duration::from_secs(16),
                reason: "connection interrupted".into(),
                interrupted_output: false,
            },
            StreamEvent::Retry {
                attempt: 7,
                max_attempts: 7,
                delay: Duration::from_secs(30),
                reason: "connection interrupted".into(),
                interrupted_output: false,
            },
        ]
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

const OVERLOADED_STREAM: &str = concat!(
    "event: error\n",
    "data: {\"type\":\"error\",\"code\":\"server_is_overloaded\",\"message\":\"Our servers are currently overloaded.\"}\n",
    "\n",
);

const OVERLOADED_STREAM_WITHOUT_SEPARATOR: &str = concat!(
    "event: error\n",
    "data: {\"type\":\"error\",\"code\":\"server_is_overloaded\",\"message\":\"Our servers are currently overloaded.\"}",
);

#[tokio::test]
async fn an_overload_dispatched_at_clean_eof_is_retried() {
    let count = std::sync::atomic::AtomicUsize::new(0);
    let server = testserver::spawn(move |_| {
        if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            sse_response(OVERLOADED_STREAM_WITHOUT_SEPARATOR)
        } else {
            sse_response(CANNED_STREAM)
        }
    })
    .await;
    let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
    let (settlement, events) = complete(&client, &model_request()).await;
    assert!(settlement.result.is_ok());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(retry_reasons(&events), ["server_is_overloaded"]);
}

#[tokio::test]
async fn a_clean_eof_without_output_is_retried() {
    let count = std::sync::atomic::AtomicUsize::new(0);
    let server = testserver::spawn(move |_| {
        if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            sse_response(": keepalive\n\n")
        } else {
            sse_response(CANNED_STREAM)
        }
    })
    .await;
    let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
    let (settlement, events) = complete(&client, &model_request()).await;
    assert!(settlement.result.is_ok());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(server.count(), 2);
    assert_eq!(retry_reasons(&events), [TRANSPORT_RETRY_REASON]);
}

#[tokio::test]
async fn interrupted_text_reasoning_and_tool_streams_are_retried() {
    for prefix in [
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial answer long enough to be visible\"}\n\n",
        "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"partial reasoning long enough to be visible\"}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"item1\",\"call_id\":\"unfinished\",\"name\":\"get_time\",\"arguments\":\"\"}}\n\n",
    ] {
        let count = std::sync::atomic::AtomicUsize::new(0);
        let server = testserver::spawn(move |_| {
            if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                sse_response(prefix)
            } else {
                sse_response(CANNED_STREAM)
            }
        })
        .await;
        let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
        let (settlement, events) = complete(&client, &model_request()).await;
        assert!(settlement.result.is_ok(), "{events:?}");
        assert_eq!(settlement.attempts, 2);
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::Retry {
                    interrupted_output: true,
                    ..
                }
            )),
            "{events:?}"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body, requests[1].body);
    }
}

fn retry_reasons(events: &[StreamEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Retry { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_reported_overload_is_retried_with_the_same_request() {
    let count = std::sync::atomic::AtomicUsize::new(0);
    let server = testserver::spawn(move |_| {
        if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            sse_response(OVERLOADED_STREAM)
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
        Some(StreamEvent::Retry { attempt: 2, reason, .. }) if reason == "server_is_overloaded"
    ));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
}

#[tokio::test]
async fn response_error_variants_are_retried() {
    for failed in [
        concat!(
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"boom\"}}}\n",
            "\n",
        ),
        concat!(
            "event: error\n",
            "data: {\"error\":{\"type\":\"server_error\",\"message\":\"boom\"}}\n",
            "\n",
        ),
        concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{},\"code\":\"server_error\",\"message\":\"boom\"}\n",
            "\n",
        ),
    ] {
        let count = std::sync::atomic::AtomicUsize::new(0);
        let server = testserver::spawn(move |_| {
            if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                sse_response(failed)
            } else {
                sse_response(CANNED_STREAM)
            }
        })
        .await;
        let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
        let (settlement, events) = complete(&client, &model_request()).await;
        assert!(settlement.result.is_ok(), "variant: {failed}");
        assert_eq!(
            retry_reasons(&events),
            ["server_error"],
            "variant: {failed}"
        );
    }
}

#[tokio::test]
async fn other_reported_codes_are_not_retried() {
    for code in ["rate_limit_exceeded", "context_length_exceeded", ""] {
        let body = format!(
            "event: error\ndata: {{\"type\":\"error\",\"code\":\"{code}\",\"message\":\"m\"}}\n\n"
        );
        let server = testserver::spawn(move |_| sse_response(&body)).await;
        let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
        let (settlement, events) = complete(&client, &model_request()).await;
        assert!(settlement.result.is_err());
        assert_eq!(server.count(), 1, "code {code:?}");
        assert!(retry_reasons(&events).is_empty(), "code {code:?}");
    }
}

#[tokio::test]
async fn a_reported_overload_after_streamed_output_recovers() {
    let body = format!(
        "{}{OVERLOADED_STREAM}",
        concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n",
            "\n",
        )
    );
    let count = std::sync::atomic::AtomicUsize::new(0);
    let server = testserver::spawn(move |_| {
        if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            sse_response(&body)
        } else {
            sse_response(CANNED_STREAM)
        }
    })
    .await;
    let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
    let (settlement, events) = complete(&client, &model_request()).await;
    assert!(settlement.result.is_ok());
    assert_eq!(server.count(), 2);
    assert_eq!(retry_reasons(&events), ["server_is_overloaded"]);
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Retry {
            interrupted_output: true,
            ..
        }
    )));
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
        1 => {
            let prefix = &CANNED_STREAM[..CANNED_STREAM.find("event: response.completed").unwrap()];
            testserver::truncated_sse_response(prefix, prefix.len() + 4096)
        }
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
        let server = testserver::spawn(|_| {
            let prefix = &CANNED_STREAM[..CANNED_STREAM.find("event: response.completed").unwrap()];
            testserver::truncated_sse_response(prefix, prefix.len() + 4096)
        })
        .await;
        let client = Client::with_base_url(&server.url, static_tokens("token"), "acct-1");
        let control = crate::deadline::Control::new(crate::deadline::Deadline::unlimited());
        let mut retries = 0;
        let settlement = client
            .complete(
                &model_request(),
                &mut |event| {
                    if let StreamEvent::Retry {
                        interrupted_output, ..
                    } = event
                    {
                        assert!(interrupted_output);
                        retries += 1;
                        control.stop(reason);
                    }
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
    assert!(
        settlement
            .result
            .unwrap_err()
            .to_string()
            .contains("1 attempts; retry budget exhausted")
    );
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
    let server =
        testserver::spawn(move |_| testserver::truncated_sse_response(prefix, prefix.len() + 4096))
            .await;
    let client = Client::with_base_url(
        &server.url,
        static_tokens("stream-token-secret"),
        "stream-account-secret",
    );
    let (settlement, events) = complete(&client, &Request::default()).await;
    assert_eq!(settlement.attempts, 7);
    assert_eq!(server.count(), 7);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta { .. }))
    );
    assert_eq!(retry_reasons(&events).len(), 6);
    assert!(
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::Retry {
                    interrupted_output, ..
                } => Some(*interrupted_output),
                _ => None,
            })
            .all(|interrupted| interrupted)
    );
    let error = settlement.result.unwrap_err().to_string();
    assert_eq!(
        error,
        "chatgpt request failed (connection interrupted; 7 attempts; retry budget exhausted)"
    );
    assert!(!format!("{events:?} {error}").contains("stream-token-secret"));
    assert!(!format!("{events:?} {error}").contains("stream-account-secret"));
}

/// A malformed stream remains terminal.
#[tokio::test]
async fn a_protocol_failure_is_not_retried() {
    let server = testserver::spawn(|_| sse_response("data: {invalid}\n\n")).await;
    let client = Client::with_base_url(&server.url, static_tokens("secret"), "acct-1");
    let (settlement, events) = complete(&client, &Request::default()).await;
    assert!(settlement.result.is_err());
    assert_eq!(server.count(), 1);
    assert!(retry_reasons(&events).is_empty());
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

/// A stream in which the backend ran its own web search: the hosted call's
/// output items and a citation annotation must not disturb the text.
const HOSTED_SEARCH_STREAM: &str = concat!(
    "event: response.output_item.added\n",
    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"in_progress\"}}\n",
    "\n",
    "event: response.web_search_call.searching\n",
    "data: {\"type\":\"response.web_search_call.searching\",\"item_id\":\"ws_1\"}\n",
    "\n",
    "event: response.output_item.done\n",
    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"query\":\"rust\"}}}\n",
    "\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Rust is a language.\"}\n",
    "\n",
    "event: response.output_text.annotation.added\n",
    "data: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\",\"url\":\"https://www.rust-lang.org/\"}}\n",
    "\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n",
    "\n",
);

#[tokio::test]
async fn hosted_web_search_is_advertised_only_when_enabled_and_its_items_are_ignored() {
    let server = testserver::spawn(|_| sse_response(HOSTED_SEARCH_STREAM)).await;
    let off = Client::with_base_url(&server.url, static_tokens("test-token"), "acct-1");
    let on = Client::with_base_url(&server.url, static_tokens("test-token"), "acct-1")
        .with_hosted_web_search(true);

    let (settlement, _) = complete(&off, &model_request()).await;
    settlement.result.unwrap();
    let (settlement, _) = complete(&on, &model_request()).await;
    let response = settlement.result.unwrap();

    let sent = server.requests();
    let tools = |index: usize| -> Vec<serde_json::Value> {
        let body: serde_json::Value = serde_json::from_str(&sent[index].body).unwrap();
        body["tools"].as_array().unwrap().clone()
    };
    assert!(tools(0).iter().all(|tool| tool["type"] == "function"));
    let enabled = tools(1);
    assert_eq!(
        enabled.last().unwrap(),
        &serde_json::json!({"type": "web_search"})
    );
    assert_eq!(enabled.len(), tools(0).len() + 1);

    assert_eq!(
        response.message.blocks.len(),
        1,
        "no tool call for a hosted search"
    );
    assert_eq!(response.message.blocks[0].block_type, BlockType::Text);
    assert_eq!(response.message.blocks[0].text, "Rust is a language.");
    assert_eq!(response.message.finish_reason, Some(FinishReason::Stop));
    // The size estimate counts the extra tool.
    assert_eq!(
        on.serialized_request_size(&model_request()).unwrap(),
        sent[1].body.len()
    );
}

fn redaction_marker(values: &[String]) -> String {
    let mut collector = otto_core::safetext::SecretCollector::new();
    for value in values {
        assert!(collector.add(value));
    }
    otto_core::safetext::dynamic_redaction_marker(&collector.values()).unwrap()
}
