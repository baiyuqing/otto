//! Tests for redirects, redacted error bodies, context overflow and base URLs.

use super::*;

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
