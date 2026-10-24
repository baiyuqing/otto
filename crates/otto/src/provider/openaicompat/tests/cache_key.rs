//! Tests for `prompt_cache_key` and the fallback for endpoints that reject it.

use super::*;

const REJECTION_BODY: &str =
    r#"{"error":{"message":"Unrecognized request argument supplied: prompt_cache_key"}}"#;

fn keyed_request(cache_key: &str) -> Request {
    Request {
        cache_key: cache_key.to_string(),
        ..model_request()
    }
}

/// Answers `rejection_status` with `rejection_body` to the first request and
/// a complete stream to every later one.
async fn reject_first(rejection_status: u16, rejection_body: &'static str) -> http::Server {
    let hits = Arc::new(AtomicUsize::new(0));
    spawn_server(move |_head, _body| {
        if hits.fetch_add(1, Ordering::SeqCst) == 0 {
            http_response(rejection_status, &[], rejection_body)
        } else {
            http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
        }
    })
    .await
}

fn bodies(server: &http::Server) -> Vec<String> {
    server
        .requests()
        .into_iter()
        .map(|request| request.body)
        .collect()
}

#[tokio::test]
async fn a_non_empty_cache_key_is_sent_as_prompt_cache_key() {
    let server = reject_first(500, "unused").await;
    let client = Client::new(&server.url, "sk-test-secret");
    let _ = complete(&client, &keyed_request("session-1")).await;
    let bodies = bodies(&server);
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].contains(r#""prompt_cache_key":"session-1""#),
        "{}",
        bodies[0]
    );
}

#[tokio::test]
async fn a_400_or_422_naming_the_field_is_resent_once_without_it() {
    for status in [400u16, 422] {
        let server = reject_first(status, REJECTION_BODY).await;
        let client = Client::new(&server.url, "sk-test-secret");
        let mut events = Vec::new();
        let mut emit = |event: StreamEvent| events.push(event);
        let settlement = client
            .complete(
                &keyed_request("session-1"),
                &mut emit,
                &CancellationToken::new(),
            )
            .await;
        assert!(
            settlement.result.is_ok(),
            "{status}: {:?}",
            settlement.result
        );
        assert_eq!(settlement.attempts, 2, "{status}");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Retry { .. })),
            "{status}: no Retry event for the resend"
        );
        let sent = bodies(&server);
        assert_eq!(sent.len(), 2, "{status}");
        assert!(sent[0].contains("prompt_cache_key"), "{status}");
        assert!(
            !sent[1].contains("prompt_cache_key"),
            "{status}: {}",
            sent[1]
        );

        // The client remembers the rejection.
        assert!(complete(&client, &keyed_request("session-1")).await.is_ok());
        let sent = bodies(&server);
        assert_eq!(sent.len(), 3, "{status}");
        assert!(
            !sent[2].contains("prompt_cache_key"),
            "{status}: {}",
            sent[2]
        );
    }
}

#[tokio::test]
async fn the_resend_does_not_use_the_failure_retry_budget() {
    // The rejection, then three HTTP 503 answers, then success: the fourth
    // failure-budget attempt must still be available after the resend.
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let server = spawn_server(
        move |_head, _body| match counter.fetch_add(1, Ordering::SeqCst) {
            0 => http_response(400, &[], REJECTION_BODY),
            1..=3 => http_response(503, &[("Retry-After", "0")], "busy"),
            _ => http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM),
        },
    )
    .await;
    let client = Client::new(&server.url, "sk-test-secret");
    let mut events = Vec::new();
    let mut emit = |event: StreamEvent| events.push(event);
    let settlement = client
        .complete(
            &keyed_request("session-1"),
            &mut emit,
            &CancellationToken::new(),
        )
        .await;
    assert!(settlement.result.is_ok(), "{:?}", settlement.result);
    assert_eq!(settlement.attempts, 5);
    let planned: Vec<u32> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Retry { attempt, .. } => Some(*attempt),
            _ => None,
        })
        .collect();
    assert_eq!(planned, vec![2, 3, 4]);
}

#[tokio::test]
async fn a_400_that_does_not_name_the_field_is_not_resent() {
    let server = reject_first(400, r#"{"error":{"message":"bad request"}}"#).await;
    let client = Client::new(&server.url, "sk-test-secret");
    let error = complete(&client, &keyed_request("session-1"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("HTTP 400"), "{error}");
    assert_eq!(server.requests().len(), 1);
    // The endpoint was not marked as rejecting the field.
    let _ = complete(&client, &keyed_request("session-1")).await;
    assert!(bodies(&server)[1].contains("prompt_cache_key"));
}

#[tokio::test]
async fn an_empty_cache_key_is_never_resent() {
    let server = reject_first(400, REJECTION_BODY).await;
    let client = Client::new(&server.url, "sk-test-secret");
    let error = complete(&client, &keyed_request("")).await.unwrap_err();
    assert!(error.to_string().contains("prompt_cache_key"), "{error}");
    let sent = bodies(&server);
    assert_eq!(sent.len(), 1);
    assert!(!sent[0].contains("prompt_cache_key"));
}
