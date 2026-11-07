use super::{http, request, response, stream_tests::events, trusted_test_client};
use otto_core::{
    model::EffectCertainty,
    operation::OperationControl,
    provider::{Provider, ProviderError, StreamEvent},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

fn body(arguments: &str, reason: &str, output_tokens: u64) -> String {
    let mut events = events(arguments, true);
    // Close the malformed block BEFORE terminal metadata, as on the wire.
    let block_stop = events.remove(events.len() - 2);
    events.insert(4, block_stop);
    events
        .into_iter()
        .map(|event| {
            let mut event: Value = serde_json::from_str(&event).unwrap();
            if event["type"] == "message_delta" && event["delta"]["stop_reason"].is_string() {
                event["delta"]["stop_reason"] = json!(reason);
                event["usage"]["output_tokens"] = json!(output_tokens);
            }
            format!("data: {event}\n\n")
        })
        .collect()
}
fn budgets(server: &http::Server) -> Vec<u64> {
    server
        .requests()
        .iter()
        .map(|request| {
            serde_json::from_str::<Value>(&request.body).unwrap()["max_tokens"]
                .as_u64()
                .unwrap()
        })
        .collect()
}

#[tokio::test]
async fn confirmed_truncation_grows_budget_and_invalidates_provisional_arguments() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let server = http::spawn(move |request| {
        let budget = serde_json::from_str::<Value>(&request.body).unwrap()["max_tokens"]
            .as_u64()
            .unwrap();
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            response(200, &body("{\"private-value\":", "max_tokens", budget))
        } else {
            response(200, &body("{\"ok\":true}", "tool_use", 10))
        }
    })
    .await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    let mut request = request();
    request.model = "claude-opus-5-5".into();
    let mut pending = String::new();
    let mut retries = Vec::new();
    let settlement = client
        .complete(
            &request,
            &mut |event| match event {
                StreamEvent::ToolCallDelta { arguments, .. } => pending.push_str(&arguments),
                StreamEvent::Retry {
                    interrupted_output,
                    attempt,
                    max_attempts,
                    reason,
                    ..
                } => {
                    assert!(interrupted_output);
                    assert_eq!((attempt, max_attempts), (2, 4));
                    assert_eq!(reason, "Claude tool input reached output token limit");
                    pending.clear();
                    retries.push(attempt);
                }
                _ => {}
            },
            &CancellationToken::new(),
        )
        .await;
    let response = settlement.result.unwrap();
    assert_eq!(settlement.attempts, 2);
    assert_eq!(budgets(&server), [16_384, 32_768]);
    assert_eq!(retries, [2]);
    assert_eq!(pending, "{\"ok\":true}");
    assert_eq!(
        response.message.blocks[0].arguments.as_ref().unwrap().get(),
        pending
    );
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Completed
    );
}

#[tokio::test]
async fn malformed_inputs_unknown_models_and_model_ceilings_never_retry() {
    for (model, arguments, reason, category, budget) in [
        ("claude-opus-5-5", "[]", "max_tokens", "non_object", 16_384),
        (
            "claude-opus-5-5",
            "{\"x\":1,\"x\":2}",
            "max_tokens",
            "duplicate_key",
            16_384,
        ),
        (
            "claude-opus-5-5",
            "{\"private\":]",
            "max_tokens",
            "invalid_json",
            16_384,
        ),
        (
            "claude-opus-5-5",
            "{",
            "tool_use",
            "incomplete_json",
            16_384,
        ),
        ("unknown-model", "{", "max_tokens", "incomplete_json", 4096),
        ("claude-3-haiku", "{", "max_tokens", "incomplete_json", 4096),
    ] {
        let server = http::spawn(move |_| response(200, &body(arguments, reason, budget))).await;
        let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
        let mut request = request();
        request.model = model.into();
        let mut retries = 0;
        let settlement = client
            .complete(
                &request,
                &mut |event| {
                    if matches!(event, StreamEvent::Retry { .. }) {
                        retries += 1;
                    }
                },
                &CancellationToken::new(),
            )
            .await;
        let error = settlement.result.unwrap_err().to_string();
        assert!(
            error.starts_with(&format!("malformed Claude tool input: {category}")),
            "{error}"
        );
        assert!(error.contains(&format!("stop_reason={reason}")));
        assert!(error.contains(&format!("output_tokens={budget}")));
        assert!(error.contains(&format!("max_tokens={budget}")));
        assert!(!error.contains("private"));
        assert_eq!(settlement.attempts, 1);
        assert_eq!(retries, 0);
        assert_eq!(budgets(&server), [budget]);
    }
}

#[tokio::test]
async fn recovery_attempts_stop_at_the_model_ceiling() {
    let server = http::spawn(|request| {
        let budget = serde_json::from_str::<Value>(&request.body).unwrap()["max_tokens"]
            .as_u64()
            .unwrap();
        response(200, &body("{", "max_tokens", budget))
    })
    .await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    let mut request = request();
    request.model = "claude-opus-5-5".into();
    let settlement = client
        .complete(&request, &mut |_| {}, &CancellationToken::new())
        .await;
    assert_eq!(settlement.attempts, 4);
    assert_eq!(budgets(&server), [16_384, 32_768, 65_536, 128_000]);
    assert!(
        settlement
            .result
            .unwrap_err()
            .to_string()
            .contains("max_tokens=128000")
    );
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Completed
    );
}

#[tokio::test]
async fn output_recovery_and_http_retries_share_the_attempt_limit() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let server = http::spawn(move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            b"HTTP/1.1 503 Retry\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        } else { response(200, &body("{", "max_tokens", 100)) }
    }).await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    let mut request = request();
    request.model = "claude-opus-5-5".into();
    let settlement = client
        .complete(&request, &mut |_| {}, &CancellationToken::new())
        .await;
    assert!(settlement.result.is_err());
    assert_eq!(settlement.attempts, 4);
    assert_eq!(budgets(&server), [16_384, 16_384, 32_768, 65_536]);
}

#[tokio::test]
async fn recovery_wait_remains_cancellable() {
    let server = http::spawn(|_| response(200, &body("{", "max_tokens", 16_384))).await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    let cancel = CancellationToken::new();
    let mut request = request();
    request.model = "claude-opus-5-5".into();
    let settlement = client
        .complete(
            &request,
            &mut |event| {
                if matches!(event, StreamEvent::Retry { .. }) {
                    cancel.cancel();
                }
            },
            &cancel,
        )
        .await;
    assert!(matches!(settlement.result, Err(ProviderError::Cancelled)));
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Completed
    );
    assert_eq!(settlement.attempts, 1);
    assert_eq!(server.count(), 1);
}

struct ShortBudget(CancellationToken);
impl OperationControl for ShortBudget {
    fn cancellation_token(&self) -> &CancellationToken {
        &self.0
    }
    fn remaining(&self) -> Option<Duration> {
        Some(Duration::from_millis(100))
    }
    fn stop_reason(&self) -> Option<otto_core::model::OperationStopReason> {
        None
    }
}
#[tokio::test]
async fn insufficient_operation_budget_prevents_a_recovery_retry() {
    let server = http::spawn(|_| response(200, &body("{", "max_tokens", 16_384))).await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    let mut request = request();
    request.model = "claude-opus-5-5".into();
    let mut retries = 0;
    let settlement = client
        .complete(
            &request,
            &mut |event| {
                if matches!(event, StreamEvent::Retry { .. }) {
                    retries += 1;
                }
            },
            &ShortBudget(CancellationToken::new()),
        )
        .await;
    assert!(
        settlement
            .result
            .unwrap_err()
            .to_string()
            .contains("max_tokens=16384")
    );
    assert_eq!(settlement.attempts, 1);
    assert_eq!(server.count(), 1);
    assert_eq!(retries, 0);
}
