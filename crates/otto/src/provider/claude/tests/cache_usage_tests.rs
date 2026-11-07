use super::*;
use crate::usage::{Collector, Context, Store};
use otto_core::model::{ToolDefinition, Usage};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn cached_requests_preserve_the_transcript_and_record_writes_then_hits() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let server=http::spawn(move |incoming| {
        let body:serde_json::Value=serde_json::from_str(&incoming.body).unwrap();
        assert_eq!(body["cache_control"],serde_json::json!({"type":"ephemeral"}));
        assert_eq!(body["system"][0]["text"],"stable system");
        assert_eq!(body["system"][0]["cache_control"],body["cache_control"]);
        assert_eq!(body["tools"][0]["cache_control"],body["cache_control"]);
        assert_eq!(body["messages"][0]["content"][0]["text"],"hello");
        let first=observed.fetch_add(1,Ordering::SeqCst)==0;
        let usage=serde_json::json!({"input_tokens":2,"output_tokens":0,
            "cache_creation_input_tokens":if first {100} else {0},
            "cache_read_input_tokens":if first {0} else {100},
            "cache_creation":{"ephemeral_5m_input_tokens":if first {100} else {0},"ephemeral_1h_input_tokens":0}});
        let body=STREAM.replace(r#"{"input_tokens":2,"output_tokens":0}"#,&usage.to_string());
        response(200,&body)
    }).await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    let mut request = request();
    request.system_prompt = "stable system".into();
    request.tools = vec![ToolDefinition {
        name: "lookup".into(),
        ..Default::default()
    }];
    let directory = tempfile::tempdir().unwrap();
    let filename = directory.path().join("usage.db");
    let store = Arc::new(Store::open(&filename).unwrap());
    let collector = Collector::new(
        store.clone(),
        Context {
            session_id: "cached-session".into(),
            provider: "claude".into(),
            ..Default::default()
        },
    );
    for (index, expected_read, expected_write) in [(0, 0, 100), (1, 100, 0)] {
        let settled = client
            .complete(&request, &mut |_| {}, &CancellationToken::new())
            .await;
        assert_eq!(settled.attempts, 1);
        let result = settled.result.unwrap();
        assert_eq!(result.message.text(), "hello");
        assert!(result.cached_input_tokens_present);
        let usage = result.usage().unwrap();
        assert_eq!(
            usage,
            Usage {
                input_tokens: 102,
                output_tokens: 1,
                cached_input_tokens: expected_read,
                cache_write_input_tokens: Some(expected_write),
                cache_write_1h_input_tokens: Some(0)
            }
        );
        collector
            .record(&otto_core::agent::Event::ProviderUsage {
                usage,
                present: true,
                cached_input_tokens_present: result.cached_input_tokens_present,
            })
            .unwrap();
        assert_eq!(
            store.summary(Some("cached-session")).unwrap().usage_events,
            index + 1
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    drop(collector);
    drop(store);
    let reopened = Store::open(&filename).unwrap();
    let summary = reopened.summary(Some("cached-session")).unwrap();
    assert_eq!(summary.input_tokens, 204);
    assert_eq!(summary.cached_input_tokens, Some(100));
    assert_eq!(summary.cache_write_input_tokens, Some(100));
    assert_eq!(summary.cache_write_1h_input_tokens, Some(0));
    assert_eq!(summary.cache_hit_rate, Some(100.0 / 204.0));
    let daily = reopened.daily(1, Some("cached-session")).unwrap();
    assert_eq!(daily.summary, summary);
    assert_eq!(daily.daily[0].cache_write_input_tokens, Some(100));
}

#[tokio::test]
async fn cache_rejection_is_visible_without_an_uncached_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let server = http::spawn(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        response(400, r#"{"error":{"message":"unsupported cache_control"}}"#)
    })
    .await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    let settled = client
        .complete(&request(), &mut |_| {}, &CancellationToken::new())
        .await;
    assert!(settled.result.is_err());
    assert_eq!(settled.attempts, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
