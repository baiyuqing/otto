//! Provider contract matrix: one test per injected transport fault, driven
//! through the real `reqwest` transport and each provider's retry loop against
//! `otto::faults::http` scripts. Each test name states its row; the expected
//! values come from the resilience spec (2026-09-28) and its 2026-10-07
//! amendment, and from the module documentation of each provider.
//!
//! Rows that read a response from a live socket run in real time: paused time
//! auto-advances into reqwest's 60 s read timeout while the runtime waits on
//! the socket. The slowest rows wait out the 1 + 2 + 4 s backoff.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use otto::auth::Credentials;
use otto::auth::token::TokenSource;
use otto::deadline::{Control, Deadline};
use otto::faults::http::{self, Script, Server, Step};
use otto::provider::{chatgpt, openaicompat};
use otto_core::model::{EffectCertainty, OperationDisposition, OperationStopReason};
use otto_core::operation::OperationControl;
use otto_core::provider::{Provider, ProviderError, ProviderSettlement, Request, StreamEvent};
use tokio_util::sync::CancellationToken;

fn request() -> Request {
    Request {
        model: "model".into(),
        ..Request::default()
    }
}

fn retries(events: &[StreamEvent]) -> Vec<(u32, Duration, &str)> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Retry {
                attempt,
                delay,
                reason,
                ..
            } => Some((*attempt, *delay, reason.as_str())),
            _ => None,
        })
        .collect()
}

async fn complete(
    provider: &dyn Provider,
    control: &dyn OperationControl,
) -> (ProviderSettlement, Vec<StreamEvent>) {
    let mut events = Vec::new();
    let settlement = provider
        .complete(&request(), &mut |event| events.push(event), control)
        .await;
    (settlement, events)
}

async fn complete_plain(provider: &dyn Provider) -> (ProviderSettlement, Vec<StreamEvent>) {
    complete(provider, &CancellationToken::new()).await
}

/// A control stopped with `Deadline` once `budget` has elapsed, as
/// `runtime::builder::complete_with_timeout` does.
fn deadline_control(budget: Duration) -> Arc<Control> {
    let control = Arc::new(Control::new(Deadline::after(budget)));
    let stopper = Arc::clone(&control);
    tokio::spawn(async move {
        stopper.deadline().expired().await;
        stopper.stop(OperationStopReason::Deadline);
    });
    control
}

// --- openai-compatible ----------------------------------------------------

const STOP: &str = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
const DELTA: &str = r#"{"choices":[{"delta":{"content":"visible"}}]}"#;

fn test_endpoint(
    base_url: &str,
    provider_type: &str,
) -> otto_core::config::endpoint::TrustedEndpoint {
    let origin = url::Url::parse(base_url)
        .unwrap()
        .origin()
        .ascii_serialization();
    otto_core::config::endpoint::TrustedEndpoint::authorize(
        base_url,
        provider_type,
        std::slice::from_ref(&origin),
        std::slice::from_ref(&origin),
    )
    .expect("test-owned HTTP endpoint")
}

fn openai(server: &Server) -> openaicompat::Client {
    openaicompat::Client::new_trusted(&test_endpoint(&server.url, "openai-compatible"), "key")
}

#[tokio::test]
async fn openai_refused_three_times_then_success_takes_four_attempts_and_one_request() {
    // Attempts start at 0, 1, 3 and 7 s; the listener opens between the third
    // and the fourth.
    let server = http::late_listener(
        http::refused_port(),
        Duration::from_secs(5),
        vec![Script::sse_done(&[STOP])],
    );
    let (settlement, events) = complete_plain(&openai(&server)).await;
    assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
    assert_eq!(settlement.attempts, 4);
    assert_eq!(
        retries(&events),
        [
            (2, Duration::from_secs(1), "connection failed"),
            (3, Duration::from_secs(2), "connection failed"),
            (4, Duration::from_secs(4), "connection failed"),
        ]
    );
    assert_eq!(
        server.count(),
        1,
        "refused connections never reach the server"
    );
}

#[tokio::test]
async fn openai_http_500_is_terminal_after_one_request() {
    let server = http::spawn_repeating(Script::status(500, "try again")).await;
    let (settlement, events) = complete_plain(&openai(&server)).await;
    let error = settlement.result.expect_err("an error status is returned");
    assert!(error.to_string().contains("500"), "{error}");
    assert_eq!(settlement.attempts, 1);
    assert_eq!(server.count(), 1);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Completed
    );
    assert!(retries(&events).is_empty());
}

#[tokio::test]
async fn openai_repeating_http_429_is_retried_with_backoff_then_completed() {
    let server = http::spawn_repeating(Script::status(429, "slow down")).await;
    let (settlement, events) = complete_plain(&openai(&server)).await;
    let error = settlement.result.expect_err("the last status is returned");
    assert!(error.to_string().contains("429"), "{error}");
    assert_eq!(settlement.attempts, 4);
    assert_eq!(server.count(), 4);
    assert_eq!(
        retries(&events),
        [
            (2, Duration::from_secs(1), "HTTP 429"),
            (3, Duration::from_secs(2), "HTTP 429"),
            (4, Duration::from_secs(4), "HTTP 429"),
        ]
    );
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Completed
    );
}

#[tokio::test]
async fn openai_reset_before_the_first_event_is_retried_then_stops_as_transport_lost() {
    let server = http::spawn_repeating(Script::sse_cut_after(&[], 0)).await;
    let (settlement, events) = complete_plain(&openai(&server)).await;
    assert_eq!(settlement.attempts, 4);
    assert_eq!(server.count(), 4);
    assert_eq!(
        retries(&events),
        [
            (2, Duration::from_secs(1), "connection interrupted"),
            (3, Duration::from_secs(2), "connection interrupted"),
            (4, Duration::from_secs(4), "connection interrupted"),
        ]
    );
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
}

#[tokio::test]
async fn openai_reset_after_one_content_delta_is_retried_and_marks_partial_output() {
    let server = http::spawn_scripted(vec![
        Script::sse_cut_after(&[DELTA, STOP], 1),
        Script::sse_done(&[DELTA, STOP]),
    ])
    .await;
    let (settlement, events) = complete_plain(&openai(&server)).await;
    assert_one_retry_then_success(&settlement, &events, &server);
    assert_eq!(settlement.result.unwrap().message.text(), "visible");
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Retry {
            interrupted_output: true,
            ..
        }
    )));
}

#[tokio::test]
async fn openai_stream_ending_without_done_is_terminal_after_one_request() {
    let server = http::spawn_repeating(Script::sse_without_done(&[DELTA, STOP])).await;
    let (settlement, events) = complete_plain(&openai(&server)).await;
    assert!(
        settlement.result.is_err(),
        "a stream without [DONE] is an error"
    );
    assert_eq!(settlement.attempts, 1);
    assert_eq!(server.count(), 1);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Completed
    );
    assert!(retries(&events).is_empty());
}

#[tokio::test]
async fn openai_malformed_sse_event_is_terminal_after_one_request() {
    let server = http::spawn_repeating(Script::malformed_event()).await;
    let (settlement, events) = complete_plain(&openai(&server)).await;
    assert!(settlement.result.is_err());
    assert_eq!(settlement.attempts, 1);
    assert_eq!(server.count(), 1);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Completed
    );
    assert!(retries(&events).is_empty());
}

#[tokio::test]
async fn openai_stall_after_head_past_the_deadline_reports_deadline_exceeded() {
    let server = http::spawn_repeating(Script::stall_after_head()).await;
    let control = deadline_control(Duration::from_secs(1));
    let (settlement, events) = complete(&openai(&server), &*control).await;
    assert!(matches!(
        settlement.result,
        Err(ProviderError::DeadlineExceeded)
    ));
    assert_eq!(
        settlement.outcome.disposition,
        OperationDisposition::DeadlineExceeded
    );
    assert_eq!(
        settlement.outcome.stop_reason,
        Some(OperationStopReason::Deadline)
    );
    assert_eq!(settlement.attempts, 1);
    assert_eq!(server.count(), 1);
    assert!(retries(&events).is_empty());
}

#[tokio::test]
async fn openai_cancellation_during_backoff_is_cancelled_and_sends_no_further_request() {
    let server = http::spawn_repeating(Script::sse_cut_after(&[], 0)).await;
    let control = Control::new(Deadline::unlimited());
    let mut events = Vec::new();
    let settlement = openai(&server)
        .complete(
            &request(),
            &mut |event| {
                events.push(event);
                control.stop(OperationStopReason::UserCancellation);
            },
            &control,
        )
        .await;
    assert!(matches!(settlement.result, Err(ProviderError::Cancelled)));
    assert_eq!(
        settlement.outcome.disposition,
        OperationDisposition::Cancelled
    );
    assert_eq!(settlement.attempts, 1);
    assert_eq!(
        retries(&events).len(),
        1,
        "the stop came from the first retry"
    );
    assert_eq!(server.count(), 1);
}

/// With 500 ms left no 1 s backoff fits, so the client stops after the first
/// refusal without waiting. Real time: paused time would advance into the
/// deadline while the connect is pending.
#[tokio::test]
async fn openai_deadline_budget_smaller_than_the_backoff_stops_without_waiting() {
    let endpoint = test_endpoint(
        &format!("http://127.0.0.1:{}", http::refused_port()),
        "openai-compatible",
    );
    let client = openaicompat::Client::new_trusted(&endpoint, "key");
    let control = Control::new(Deadline::after(Duration::from_millis(500)));
    let started = std::time::Instant::now();
    let (settlement, events) = complete(&client, &control).await;
    assert!(settlement.result.is_err());
    assert_eq!(settlement.attempts, 1);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::NotStarted
    );
    assert!(retries(&events).is_empty());
    assert!(started.elapsed() < Duration::from_millis(500));
}

/// An owned, authorized transport with short socket reads for silent-peer faults.
fn openai_short(server: &Server) -> openaicompat::Client {
    openai(server).with_test_read_timeout(Duration::from_millis(200))
}

fn assert_one_retry_then_success(
    settlement: &ProviderSettlement,
    events: &[StreamEvent],
    server: &Server,
) {
    assert!(
        settlement.result.is_ok(),
        "{:?}",
        settlement.result.as_ref().err()
    );
    assert_eq!(settlement.attempts, 2);
    assert_eq!(server.count(), 2);
    assert_eq!(
        retries(events),
        [(2, Duration::from_secs(1), "connection interrupted")]
    );
}

#[tokio::test]
async fn openai_stall_before_the_head_is_retried_and_the_second_request_succeeds() {
    let server =
        http::spawn_scripted(vec![Script::stall_before_head(), Script::sse_done(&[STOP])]).await;
    let (settlement, events) = complete_plain(&openai_short(&server)).await;
    assert_one_retry_then_success(&settlement, &events, &server);
}

#[tokio::test]
async fn openai_stall_after_the_head_before_any_event_is_retried() {
    let server =
        http::spawn_scripted(vec![Script::stall_after_head(), Script::sse_done(&[STOP])]).await;
    let (settlement, events) = complete_plain(&openai_short(&server)).await;
    assert_one_retry_then_success(&settlement, &events, &server);
}

#[tokio::test]
async fn openai_stall_after_one_content_delta_is_retried_and_marks_partial_output() {
    let server = http::spawn_scripted(vec![
        Script::stall_after_events(&[DELTA]),
        Script::sse_done(&[DELTA, STOP]),
    ])
    .await;
    let (settlement, events) = complete_plain(&openai_short(&server)).await;
    assert_one_retry_then_success(&settlement, &events, &server);
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Retry {
            interrupted_output: true,
            ..
        }
    )));
}

mod agent {
    use super::*;
    use otto_core::agent::{Agent, Event, Options};
    use otto_core::model::ToolDefinition;
    use otto_core::session::MemorySession;
    use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};

    struct CountingTool(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl ToolExecutor for CountingTool {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "get_time".to_owned(),
                description: "get the time".to_owned(),
                parameters: serde_json::value::RawValue::from_string(
                    r#"{"type":"object"}"#.to_owned(),
                )
                .ok(),
            }]
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

    /// The request after the tool result is reset before any event and
    /// retried. The retry repeats the model request only: the tool already
    /// ran, and runs once.
    #[tokio::test]
    async fn openai_a_provider_retry_never_re_executes_an_already_executed_tool() {
        let tool_call = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_time","arguments":"{}"}}]}}]}"#;
        let tool_stop = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        let server = http::spawn_scripted(vec![
            Script::sse_done(&[tool_call, tool_stop]),
            Script::sse_cut_after(&[], 0),
            Script::sse_done(&[DELTA, STOP]),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = Agent::new(
            openai(&server),
            CountingTool(Arc::clone(&calls)),
            MemorySession::new(),
            Options {
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
                .any(|event| matches!(event, Event::ProviderRetry { .. }))
        );
        assert!(matches!(events.last(), Some(Event::AgentFinished)));
    }
}

// --- chatgpt --------------------------------------------------------------

const TEXT: &str = r#"{"type":"response.output_text.delta","delta":"a reply long enough to pass the secret redactor and become visible to the caller"}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}"#;

fn tokens(access_token: &str) -> TokenSource {
    TokenSource::new(
        Credentials {
            access_token: access_token.to_owned(),
            ..Credentials::default()
        },
        std::path::PathBuf::from("/nonexistent/chatgpt.json"),
        CancellationToken::new(),
    )
}

fn chatgpt(server: &Server, access_token: &str) -> chatgpt::Client {
    chatgpt::Client::new_trusted(
        &test_endpoint(&server.url, "chatgpt"),
        tokens(access_token),
        "acct-1",
    )
}

#[tokio::test]
async fn chatgpt_transport_failure_then_success_retries_the_same_request() {
    let server = http::spawn_scripted(vec![
        Script::truncated("", 4096),
        Script::sse_without_done(&[TEXT, COMPLETED]),
    ])
    .await;
    let (settlement, events) = complete_plain(&chatgpt(&server, "token")).await;
    assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(
        retries(&events),
        [(2, Duration::from_secs(1), "connection interrupted")]
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|r| r.body == requests[0].body));
}

#[tokio::test]
async fn chatgpt_non_transient_http_statuses_are_terminal_after_one_request() {
    for status in [400, 401, 408, 409, 501] {
        let server = http::spawn_repeating(Script::status(status, "body is not exposed")).await;
        let (settlement, events) = complete_plain(&chatgpt(&server, "token")).await;
        let expected = if status == 401 {
            "chatgpt authorization failed; run 'otto login'".to_owned()
        } else {
            format!("chatgpt responses HTTP {status}")
        };
        assert_eq!(settlement.result.unwrap_err().to_string(), expected);
        assert_eq!(settlement.attempts, 1, "status {status}");
        assert_eq!(server.count(), 1, "status {status}");
        assert!(retries(&events).is_empty(), "status {status}");
    }
}

#[tokio::test]
async fn chatgpt_transient_http_statuses_retry_with_the_same_request() {
    for (status, reason) in [
        (429, "HTTP 429"),
        (500, "HTTP 500"),
        (502, "HTTP 502"),
        (503, "HTTP 503"),
        (504, "HTTP 504"),
    ] {
        let server = http::spawn_scripted(vec![
            Script::status(status, "body is not exposed"),
            Script::sse_without_done(&[TEXT, COMPLETED]),
        ])
        .await;
        let (settlement, events) = complete_plain(&chatgpt(&server, "token")).await;
        assert!(
            settlement.result.is_ok(),
            "status {status}: {:?}",
            settlement.result.err()
        );
        assert_eq!(settlement.attempts, 2, "status {status}");
        assert_eq!(
            retries(&events),
            [(2, Duration::from_secs(1), reason)],
            "status {status}"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "status {status}");
        assert_eq!(requests[0].body, requests[1].body, "status {status}");
    }
}

#[tokio::test]
async fn chatgpt_stream_codec_error_is_terminal_after_one_request() {
    let server = http::spawn_repeating(Script::malformed_event()).await;
    let (settlement, events) = complete_plain(&chatgpt(&server, "token")).await;
    assert!(settlement.result.is_err());
    assert_eq!(settlement.attempts, 1);
    assert_eq!(server.count(), 1);
    assert!(retries(&events).is_empty());
}

#[tokio::test]
async fn chatgpt_token_source_failure_sends_no_request_and_is_not_started() {
    let server = http::spawn_repeating(Script::status(200, "{}")).await;
    let (settlement, events) = complete_plain(&chatgpt(&server, "")).await;
    assert_eq!(
        settlement.result.unwrap_err().to_string(),
        "chatgpt authorization failed; run 'otto login'"
    );
    assert_eq!(server.count(), 0);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::NotStarted
    );
    assert!(retries(&events).is_empty());
}

#[tokio::test]
async fn chatgpt_reset_after_output_recovers_and_marks_partial_output() {
    let server = http::spawn_scripted(vec![
        Script::sse_cut_after(&[TEXT, COMPLETED], 1),
        Script::sse_without_done(&[TEXT, COMPLETED]),
    ])
    .await;
    let (settlement, events) = complete_plain(&chatgpt(&server, "token")).await;
    assert_one_retry_then_success(&settlement, &events, &server);
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Retry {
            interrupted_output: true,
            ..
        }
    )));
    let requests = server.requests();
    assert_eq!(requests[0].body, requests[1].body);
}

fn chatgpt_short(server: &Server) -> chatgpt::Client {
    chatgpt(server, "token").with_test_read_timeout(Duration::from_millis(200))
}

#[tokio::test]
async fn chatgpt_stall_before_the_head_is_retried_and_the_second_request_succeeds() {
    let server = http::spawn_scripted(vec![
        Script::stall_before_head(),
        Script::sse_without_done(&[TEXT, COMPLETED]),
    ])
    .await;
    let (settlement, events) = complete_plain(&chatgpt_short(&server)).await;
    assert!(settlement.result.is_ok());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(server.count(), 2);
    assert_eq!(
        retries(&events),
        [(2, Duration::from_secs(1), "request timeout")]
    );
    let requests = server.requests();
    assert_eq!(requests[0].body, requests[1].body);
}

#[tokio::test]
async fn chatgpt_stall_after_the_head_before_any_event_is_retried() {
    let server = http::spawn_scripted(vec![
        Script::stall_after_head(),
        Script::sse_without_done(&[TEXT, COMPLETED]),
    ])
    .await;
    let (settlement, events) = complete_plain(&chatgpt_short(&server)).await;
    assert!(settlement.result.is_ok());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(server.count(), 2);
    assert_eq!(
        retries(&events),
        [(2, Duration::from_secs(1), "stream read timeout")]
    );
}

#[tokio::test]
async fn chatgpt_stall_after_one_content_delta_recovers() {
    let server = http::spawn_scripted(vec![
        Script::stall_after_events(&[TEXT]),
        Script::sse_without_done(&[TEXT, COMPLETED]),
    ])
    .await;
    let (settlement, events) = complete_plain(&chatgpt_short(&server)).await;
    assert!(settlement.result.is_ok());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(server.count(), 2);
    assert_eq!(
        retries(&events),
        [(2, Duration::from_secs(1), "stream read timeout")]
    );
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Retry {
            interrupted_output: true,
            ..
        }
    )));
}

/// A script that sends the head, then `lead`, then an SSE comment every 50 ms
/// for about 5 s. Every read is far inside the 200 ms read timeout, so only
/// the model-output idle bound can end the attempt.
fn keepalive_script(lead: &[&str]) -> Script {
    let mut script = Script::new(vec![Step::Head {
        status: 200,
        headers: vec![("Content-Type".into(), "text/event-stream".into())],
    }]);
    for event in lead {
        script = script.then(Step::Sse((*event).into()));
    }
    for _ in 0..100 {
        script = script
            .then(Step::Chunk(b": keepalive\n\n".to_vec()))
            .then(Step::Stall(Some(Duration::from_millis(50))));
    }
    script.then(Step::Close)
}

const IDLE: Duration = Duration::from_millis(500);
const IDLE_REASON: &str = "model output idle timeout";

fn chatgpt_idle(server: &Server) -> chatgpt::Client {
    chatgpt_short(server).with_model_idle_timeout(IDLE)
}

fn openai_idle(server: &Server) -> openaicompat::Client {
    openai_short(server).with_model_idle_timeout(IDLE)
}

async fn chatgpt_keepalive_retry(lead: &[&str], interrupted_output: bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let server = http::spawn_scripted(vec![
            keepalive_script(lead),
            Script::sse_without_done(&[TEXT, COMPLETED]),
        ])
        .await;
        let (settlement, events) = complete_plain(&chatgpt_idle(&server)).await;
        assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
        assert_eq!(server.count(), 2);
        assert_eq!(retries(&events), [(2, Duration::from_secs(1), IDLE_REASON)]);
        let marked: Vec<bool> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::Retry {
                    interrupted_output, ..
                } => Some(*interrupted_output),
                _ => None,
            })
            .collect();
        assert_eq!(marked, [interrupted_output]);
    })
    .await
    .expect("the idle bound must end the keepalive-only attempt");
}

#[tokio::test]
async fn chatgpt_keepalive_without_model_output_is_retried() {
    chatgpt_keepalive_retry(&[], false).await;
}

#[tokio::test]
async fn chatgpt_keepalive_after_one_delta_is_retried() {
    chatgpt_keepalive_retry(&[TEXT], true).await;
}

#[tokio::test]
async fn chatgpt_spaced_deltas_do_not_time_out() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut script = Script::new(vec![Step::Head {
            status: 200,
            headers: vec![("Content-Type".into(), "text/event-stream".into())],
        }]);
        // A delta every 300 ms: past the 200 ms read timeout without the
        // keepalive, inside the 500 ms idle bound with it.
        for _ in 0..5 {
            script = script
                .then(Step::Stall(Some(Duration::from_millis(150))))
                .then(Step::Chunk(b": keepalive\n\n".to_vec()))
                .then(Step::Stall(Some(Duration::from_millis(150))))
                .then(Step::Sse(TEXT.into()));
        }
        let script = script.then(Step::Sse(COMPLETED.into())).then(Step::Close);
        let server = http::spawn_scripted(vec![script]).await;
        let (settlement, events) = complete_plain(&chatgpt_idle(&server)).await;
        assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
        assert!(retries(&events).is_empty());
        assert_eq!(server.count(), 1);
    })
    .await
    .expect("spaced deltas must complete");
}

#[tokio::test]
async fn compat_keepalive_before_output_is_retried() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let server = http::spawn_scripted(vec![
            keepalive_script(&[]),
            Script::sse_done(&[DELTA, STOP]),
        ])
        .await;
        let (settlement, events) = complete_plain(&openai_idle(&server)).await;
        assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
        assert_eq!(server.count(), 2);
        assert_eq!(retries(&events), [(2, Duration::from_secs(1), IDLE_REASON)]);
    })
    .await
    .expect("the idle bound must end the keepalive-only attempt");
}

#[tokio::test]
async fn compat_keepalive_after_output_is_retried_and_marks_partial_output() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let server = http::spawn_scripted(vec![
            keepalive_script(&[DELTA]),
            Script::sse_done(&[DELTA, STOP]),
        ])
        .await;
        let (settlement, events) = complete_plain(&openai_idle(&server)).await;
        assert!(settlement.result.is_ok(), "{:?}", settlement.result.err());
        assert_eq!(server.count(), 2);
        assert_eq!(retries(&events), [(2, Duration::from_secs(1), IDLE_REASON)]);
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Retry {
                interrupted_output: true,
                ..
            }
        )));
    })
    .await
    .expect("the idle bound must end the keepalive-only stream");
}
