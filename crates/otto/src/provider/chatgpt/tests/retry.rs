//! Tests for transport, HTTP and stream retry classification and backoff.

use super::*;

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
async fn policy_capacity_http_error_is_typed_fixed_and_not_retried() {
    let server = testserver::spawn(|_| {
        testserver::status_response(
            503,
            r#"{"error":{"code":"policy_capacity_unavailable","message":"secret upstream detail"}}"#,
        )
    })
    .await;
    let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
    let (settlement, events) = complete(&client, &Request::default()).await;
    assert!(matches!(
        settlement.result,
        Err(ProviderError::PolicyCapacityUnavailable)
    ));
    assert_eq!(settlement.attempts, 1);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Completed
    );
    assert!(events.is_empty());
    assert_eq!(server.count(), 1);
    assert_eq!(
        ProviderError::PolicyCapacityUnavailable.to_string(),
        "provider policy capacity unavailable"
    );
}

#[tokio::test]
async fn non_transient_http_statuses_are_not_retried() {
    for status in [400, 401, 408, 409, 501] {
        let server = testserver::spawn(move |_| {
            testserver::status_response(status, "provider body is not exposed")
        })
        .await;
        let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");

        let (settlement, events) = complete(&client, &Request::default()).await;
        let expected = if status == 401 {
            "chatgpt authorization failed; run 'otto login'".to_owned()
        } else {
            format!("chatgpt responses HTTP {status}")
        };
        assert_eq!(settlement.result.unwrap_err().to_string(), expected);
        assert_eq!(server.count(), 1, "status {status}");
        assert!(events.is_empty());
    }
}

/// A connection that is refused carries no provider text into the error.
/// Real time keeps socket failures distinct from artificial read timeouts.
#[tokio::test]
async fn a_transport_failure_stops_after_six_retries() {
    let client = trusted_test_client("http://127.0.0.1:1", static_tokens("token"), "acct-1");
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
    let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
    let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
    let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
        "event: response.output_item.added\ndata: \
            {\"type\":\"response.output_item.added\",\"output_index\":0,\
            \"item\":{\"type\":\"function_call\",\"id\":\"item1\",\"call_id\":\"unfinished\",\
            \"name\":\"get_time\",\"arguments\":\"\"}}\n\n",
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
        let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
    let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
async fn policy_capacity_stream_errors_are_fixed_and_not_retried() {
    for body in [
        concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"code\":\"policy_capacity_unavailable\",\"message\":\"secret upstream detail\"}\n",
            "\n",
        ),
        concat!(
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"policy_capacity_unavailable\",\"message\":\"secret upstream detail\"}}}\n",
            "\n",
        ),
    ] {
        let body = body.to_owned();
        let server = testserver::spawn(move |_| sse_response(&body)).await;
        let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
        let (settlement, events) = complete(&client, &model_request()).await;
        assert_eq!(settlement.attempts, 1);
        assert!(retry_reasons(&events).is_empty());
        assert_eq!(server.count(), 1);
        let error = settlement.result.unwrap_err();
        assert!(matches!(error, ProviderError::PolicyCapacityUnavailable));
        assert_eq!(error.to_string(), "provider policy capacity unavailable");
        assert!(!error.to_string().contains("secret upstream detail"));
    }
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
        let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
        let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
    let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
        trusted_test_client(&server.url, static_tokens("token"), "acct-1"),
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
        let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
    let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
    let client = trusted_test_client("invalid-url", static_tokens("token"), "acct-1");
    let (settlement, events) = complete(&client, &model_request()).await;
    assert_eq!(settlement.attempts, 0);
    assert!(
        settlement
            .result
            .unwrap_err()
            .to_string()
            .contains("invalid provider endpoint")
    );
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
    let client = trusted_test_client(
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
    let client = trusted_test_client(&server.url, static_tokens("secret"), "acct-1");
    let (settlement, events) = complete(&client, &Request::default()).await;
    assert!(settlement.result.is_err());
    assert_eq!(server.count(), 1);
    assert!(retry_reasons(&events).is_empty());
}

#[tokio::test]
async fn a_deadline_stopped_call_reports_deadline_exceeded() {
    let server = testserver::spawn(|_| sse_response(CANNED_STREAM)).await;
    let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
    let client = trusted_test_client(&server.url, static_tokens("token"), "acct-1");
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
