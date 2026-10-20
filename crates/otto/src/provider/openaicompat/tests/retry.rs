//! Tests for retry classification of connection, status and stream failures.

use super::*;

#[tokio::test]
async fn a_deadline_stopped_call_reports_deadline_exceeded() {
    let server = spawn_server(|_head, _body| {
        http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
    })
    .await;
    let client = Client::new(&server.url, "key");
    let mut emit = |_: StreamEvent| panic!("no event is emitted after deadline");
    let settlement = client
        .complete(&model_request(), &mut emit, &StoppedControl::deadline())
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
}

// --- transport retry -------------------------------------------------

#[tokio::test]
async fn rate_limits_and_server_errors_are_not_retried() {
    #[derive(Default)]
    struct Statuses(std::sync::Mutex<Vec<u16>>);
    impl otto_core::diagnostics::Observer for Statuses {
        fn activity(&self, _: otto_core::diagnostics::Phase, _: usize, status: Option<u16>) {
            if let Some(status) = status {
                self.0.lock().unwrap().push(status);
            }
        }
    }
    for status in [429u16, 503] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn_server(move |_head, _body| {
            counter.fetch_add(1, Ordering::SeqCst);
            http_response(status, &[], "try again")
        })
        .await;

        let client = Client::new(&server.url, "key");
        let mut events = Vec::new();
        let mut emit = |event| events.push(event);
        let statuses = Statuses::default();
        let settlement = client
            .complete_observed(
                &model_request(),
                &mut emit,
                &CancellationToken::new(),
                &statuses,
            )
            .await;
        assert_eq!(*statuses.0.lock().unwrap(), vec![status]);
        assert_eq!(settlement.attempts, 1);
        assert_eq!(
            settlement.outcome.effect_certainty,
            EffectCertainty::Completed
        );
        let error = settlement.result.expect_err("the first status is returned");
        assert!(error.to_string().contains(&status.to_string()));
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "status {status}");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Retry { .. })),
            "status {status} emitted a retry"
        );
    }
}

/// A connection that is refused never reached the endpoint: it is tried
/// again 1, 2 and 4 s later, then fails as not started. Paused time skips
/// the backoff.
#[tokio::test(start_paused = true)]
async fn a_refused_connection_is_retried_three_times_and_not_started() {
    let client = Client::new("http://127.0.0.1:1", "key");
    let mut events = Vec::new();
    let settlement = client
        .complete(
            &model_request(),
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(settlement.attempts, 4);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::NotStarted
    );
    assert!(settlement.result.is_err());
    assert_eq!(
        events,
        (2..=4)
            .map(|attempt| StreamEvent::Retry {
                attempt,
                max_attempts: 4,
                delay: Duration::from_secs(1 << (attempt - 2)),
                reason: "connection failed".into(),
                interrupted_output: false,
            })
            .collect::<Vec<_>>()
    );
}

/// The endpoint starts listening after the first refusal; the retry
/// sends the same request and succeeds.
#[tokio::test]
async fn a_connection_retry_recovers_once_the_endpoint_listens() {
    let server = http::late_listener(
        http::refused_port(),
        Duration::from_millis(300),
        vec![http::Script::sse_done(&[
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ])],
    );
    let client = Client::new(&server.url, "key");
    let mut retries = 0;
    let settlement = client
        .complete(
            &model_request(),
            &mut |event| {
                if matches!(event, StreamEvent::Retry { .. }) {
                    retries += 1;
                }
            },
            &CancellationToken::new(),
        )
        .await;
    assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(retries, 1);
}

/// The endpoint accepts the request and closes before any response. The
/// request may already be at the endpoint, so the retry is reported and
/// the settlement would keep unknown effect certainty.
#[tokio::test]
async fn a_connection_closed_before_the_response_is_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let server = spawn_server(move |_head, _body| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            return Vec::new();
        }
        http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
    })
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await;
    assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(retry_reasons(&events), [TRANSPORT_RETRY_REASON]);
}

/// The response stream is cut before any delta; nothing was visible, so
/// the retry sends the same request again and returns the whole response.
#[tokio::test]
async fn a_stream_cut_before_any_delta_is_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let server = spawn_server(move |_head, _body| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            return truncated_stream("");
        }
        http_response(200, &[("Content-Type", "text/event-stream")], DONE_STREAM)
    })
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await;
    assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(retry_reasons(&events), [TRANSPORT_RETRY_REASON]);
}

/// A connection reset that keeps repeating is tried 1, 2 and 4 seconds
/// later (real time: paused time would trip the read timeout on the
/// loopback socket), then reported as a lost transport whose effect is
/// unknown.
#[tokio::test]
async fn a_repeated_stream_cut_stops_as_transport_lost() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let server = spawn_server(move |_head, _body| {
        counter.fetch_add(1, Ordering::SeqCst);
        truncated_stream("")
    })
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await;
    assert_eq!(settlement.attempts, 4);
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    assert_eq!(retry_reasons(&events), [TRANSPORT_RETRY_REASON; 3]);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Unknown
    );
    assert_eq!(
        settlement.outcome.disposition,
        OperationDisposition::Interrupted
    );
    assert_eq!(
        settlement.outcome.stop_reason,
        Some(OperationStopReason::TransportLost)
    );
    let error = settlement.result.unwrap_err().to_string();
    assert!(
        error.starts_with("read chat completion stream:"),
        "error = {error}"
    );
}

/// A stream cut is not retried when the remaining budget cannot hold the
/// backoff and another attempt.
#[tokio::test]
async fn a_stream_cut_without_retry_budget_is_not_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let server = spawn_server(move |_head, _body| {
        counter.fetch_add(1, Ordering::SeqCst);
        truncated_stream("")
    })
    .await;

    let control = BudgetControl {
        token: CancellationToken::new(),
        remaining: Duration::from_secs(1),
    };
    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    let settlement = Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &control)
        .await;
    assert_eq!(settlement.attempts, 1);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(events.is_empty());
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Unknown
    );
    assert_eq!(
        settlement.outcome.stop_reason,
        Some(OperationStopReason::TransportLost)
    );
}

/// A delta already reached the caller, so the output cannot be replayed
/// and the failure stays single-attempt.
#[tokio::test]
async fn stream_cut_after_a_delta_is_not_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let server = spawn_server(move |_head, _body| {
        counter.fetch_add(1, Ordering::SeqCst);
        truncated_stream("data: {\"choices\":[{\"delta\":{\"content\":\"visible\"}}]}\n\n")
    })
    .await;

    let mut events = Vec::new();
    let mut emit = |event| events.push(event);
    Client::new(&server.url, "key")
        .complete(&model_request(), &mut emit, &CancellationToken::new())
        .await
        .result
        .expect_err("the first interrupted stream is returned");
    assert!(!events.is_empty());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Retry { .. }))
    );
}

/// The `Retry` event reason of every retry in `events`.
fn retry_reasons(events: &[StreamEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Retry { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect()
}
