//! Tests for the request wire format and the model catalog endpoints.

use super::*;

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
    let client = trusted_test_client("https://example.test/v1", "key");
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

    let client = trusted_test_client(&format!("{}/gateway/v1/", server.url), "secret");
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
    let server = crate::faults::http::spawn(|_| {
        crate::faults::http::json_response(concat!(
            r#"{"data":[{"id":"fixture-a","display_name":"Provider name","#,
            r#""description":"catalog-compat-secret"},{"id":"fixture-b"}]}"#
        ))
    })
    .await;
    let client = trusted_test_client(&server.url, "catalog-compat-secret");
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

    let client = trusted_test_client(&format!("{}/gateway/v1/", server.url), "secret");
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

    let client = trusted_test_client(&server.url, "sk-secret-value-123");
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

    let client = trusted_test_client(&server.url, "key");
    let error = client
        .list_models(&CancellationToken::new())
        .await
        .expect_err("the body has no data list");

    let ProviderError::Other(message) = error else {
        panic!("unexpected error {error:?}");
    };
    assert!(message.starts_with("decode model list: "), "{message}");
}
