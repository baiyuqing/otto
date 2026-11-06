//! Tests for request shape, stream parsing, redaction and non-retried errors.

use super::*;

/// Text and tool call are parsed, and the four request headers are
/// asserted.
#[tokio::test]
async fn sends_the_expected_request_and_parses_text_and_a_tool_call() {
    let server = testserver::spawn(|_| sse_response(CANNED_STREAM)).await;
    let client = trusted_test_client(&server.url, static_tokens("test-token"), "acct-1");

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
    let client = trusted_test_client(&server.url, static_tokens("test-token"), "acct-1");
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
        let client = trusted_test_client(&server.url, static_tokens("test-token"), "acct-1");
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
    let client = trusted_test_client(&server.url, static_tokens(&access_token), account_id);

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
    let client = trusted_test_client(&server.url, static_tokens("token"), account_id);

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

/// An authorization error uses fixed guidance, so no part of the response body
/// can reach it.
#[tokio::test]
async fn an_unauthorized_response_reports_fixed_guidance() {
    let access_token = "secret-abc";
    let account_id = "acct-secret";
    let body = format!(
        "{}{access_token} account {account_id}",
        "x".repeat(32 << 10)
    );
    let server = testserver::spawn(move |_| testserver::status_response(401, &body)).await;
    let client = trusted_test_client(&server.url, static_tokens(access_token), account_id);

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
    let error_message = error.to_string();
    assert_eq!(
        error_message,
        "chatgpt authorization failed; run 'otto login'"
    );
    assert!(!error_message.contains(access_token));
    assert!(!error_message.contains(account_id));
}

#[tokio::test]
async fn a_redirect_is_reported_as_its_status_without_forwarding_the_request() {
    let target = testserver::spawn(|_| testserver::status_response(500, "unreachable")).await;
    let location = format!("{}/responses", target.url);
    let source = testserver::spawn(move |_| testserver::redirect_response(307, &location)).await;
    let client = trusted_test_client(
        &source.url,
        static_tokens("redirect-access-token"),
        "redirect-account-id",
    )
    .with_test_read_timeout(Duration::from_millis(200));

    let (settlement, _) = complete(&client, &model_request()).await;
    assert_eq!(
        settlement.result.unwrap_err().to_string(),
        "chatgpt responses HTTP 307"
    );
    assert_eq!(source.count(), 1);
    assert_eq!(target.count(), 0);
}

#[tokio::test]
async fn test_read_timeout_preserves_endpoint_rejection() {
    let client = Client::with_base_url(
        "http://127.0.0.1:1",
        static_tokens("test-token"),
        "test-account",
    )
    .with_test_read_timeout(Duration::from_millis(200));
    let (settlement, _) = complete(&client, &model_request()).await;
    assert!(
        settlement
            .result
            .unwrap_err()
            .to_string()
            .contains("not trusted")
    );
    assert_eq!(settlement.attempts, 0);
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
        let client = trusted_test_client("http://127.0.0.1:1", tokens, "acct-secret");
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
    let client = trusted_test_client("http://127.0.0.1:1", static_tokens("token"), "  ");
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
    let client = trusted_test_client(&server.url, static_tokens(&oversized), "acct-1");

    let (settlement, events) = complete(&client, &Request::default()).await;
    assert_eq!(
        settlement.result.unwrap_err().to_string(),
        "chatgpt request failed"
    );
    assert_eq!(server.count(), 0);
    assert!(events.is_empty());
}
