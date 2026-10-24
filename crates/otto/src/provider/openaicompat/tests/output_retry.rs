//! Tests for retry of a dispatched request after stream output was emitted.

use super::*;

const FIRST_DELTA: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n";

/// A complete stream whose only text is `second`.
fn second_attempt_stream() -> Vec<u8> {
    let body = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"second\"}}}}]}}\n\n{DONE_STREAM}"
    );
    http_response(200, &[("Content-Type", "text/event-stream")], &body)
}

/// Each `Retry` event as `(reason, interrupted_output)`.
fn retry_flags(events: &[StreamEvent]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Retry {
                reason,
                interrupted_output,
                ..
            } => Some((reason.clone(), *interrupted_output)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn stream_cut_after_a_delta_is_retried_and_only_the_second_attempt_is_returned() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let server = spawn_server(move |_head, _body| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            truncated_stream(FIRST_DELTA)
        } else {
            second_attempt_stream()
        }
    })
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await;
    let response = settlement.result.expect("the second attempt succeeds");
    assert_eq!(response.message.text(), "second");
    assert_eq!(settlement.attempts, 2);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(
        retry_flags(&events),
        [(TRANSPORT_RETRY_REASON.to_owned(), true)]
    );
}

#[tokio::test]
async fn stream_cut_before_output_reports_no_interrupted_output() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let server = spawn_server(move |_head, _body| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            truncated_stream("")
        } else {
            second_attempt_stream()
        }
    })
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await;
    assert!(settlement.result.is_ok());
    assert_eq!(
        retry_flags(&events),
        [(TRANSPORT_RETRY_REASON.to_owned(), false)]
    );
}

/// The flag describes the failed attempt, not the earlier ones.
#[tokio::test]
async fn interrupted_output_flag_is_per_attempt() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let server = spawn_server(
        move |_head, _body| match counter.fetch_add(1, Ordering::SeqCst) {
            0 => truncated_stream(FIRST_DELTA),
            1 => truncated_stream(""),
            _ => second_attempt_stream(),
        },
    )
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await;
    assert_eq!(settlement.result.unwrap().message.text(), "second");
    assert_eq!(
        retry_flags(&events),
        [
            (TRANSPORT_RETRY_REASON.to_owned(), true),
            (TRANSPORT_RETRY_REASON.to_owned(), false),
        ]
    );
}

/// A codec error after a delta is not a transport failure.
#[tokio::test]
async fn malformed_chunk_after_a_delta_is_terminal_after_one_request() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let server = spawn_server(move |_head, _body| {
        counter.fetch_add(1, Ordering::SeqCst);
        let body = format!("{FIRST_DELTA}data: {{not json\n\n");
        http_response(200, &[("Content-Type", "text/event-stream")], &body)
    })
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await;
    settlement
        .result
        .expect_err("a malformed chunk fails the call");
    assert_eq!(settlement.attempts, 1);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(retry_flags(&events).is_empty());
}

#[tokio::test]
async fn repeated_stream_cuts_after_a_delta_exhaust_four_attempts_with_unknown_effect() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let server = spawn_server(move |_head, _body| {
        counter.fetch_add(1, Ordering::SeqCst);
        truncated_stream(FIRST_DELTA)
    })
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await;
    assert_eq!(settlement.attempts, 4);
    assert_eq!(hits.load(Ordering::SeqCst), 4);
    assert_eq!(
        retry_flags(&events),
        vec![(TRANSPORT_RETRY_REASON.to_owned(), true); 3]
    );
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Unknown
    );
    let error = settlement.result.unwrap_err().to_string();
    assert!(error.starts_with("read chat completion stream:"), "{error}");
}
