//! Exercise assembly through the real HTTP adapter with controlled local SSE.
use super::{http, request, trusted_test_client};
use otto_core::model::{BlockType, FinishReason};
use otto_core::provider::{Provider, ProviderError, StreamEvent};
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(super) fn events(arguments: &str, late_stop: bool) -> Vec<String> {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"m","role":"assistant",
            "content":[],"usage":{"input_tokens":2,"output_tokens":0}}}).to_string(),
        json!({"type":"content_block_start","index":0,"content_block":{
            "type":"tool_use","id":"c","name":"lookup","input":{}}}).to_string(),
        json!({"type":"message_delta","delta":{},"usage":{"output_tokens":1}}).to_string(),
        json!({"type":"content_block_delta","index":0,"delta":{
            "type":"input_json_delta","partial_json":arguments}}).to_string(),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}).to_string(),
    ];
    if late_stop {
        events.push(json!({"type":"content_block_stop","index":0}).to_string());
    }
    events.push(json!({"type":"message_stop"}).to_string());
    events
}

#[tokio::test]
async fn late_or_missing_block_stops_complete_without_waiting_for_http_eof() {
    let arguments = r#"{"id":900719925474099312345,"decimal":0.123456789012345678901}"#;
    for late_stop in [false, true] {
        let events = events(arguments, late_stop);
        let events: Vec<_> = events.iter().map(String::as_str).collect();
        // No HTTP terminator or FIN: only message_stop can complete this call.
        let server = http::spawn_scripted(vec![http::Script::stall_after_events(&events)]).await;
        let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
        let mut streamed_arguments = String::new();
        let mut emit = |event| {
            if let StreamEvent::ToolCallDelta { arguments, .. } = event {
                streamed_arguments.push_str(&arguments);
            }
        };
        let request = request();
        let cancel = CancellationToken::new();
        let settlement = tokio::time::timeout(
            Duration::from_secs(5),
            client.complete(&request, &mut emit, &cancel),
        )
        .await
        .expect("message_stop must complete without EOF");
        let response = settlement.result.expect("validated complete response");
        assert_eq!(settlement.attempts, 1);
        assert_eq!(server.count(), 1);
        assert_eq!(streamed_arguments, arguments);
        assert_eq!(response.message.blocks.len(), 1);
        let call = &response.message.blocks[0];
        assert_eq!(call.block_type, BlockType::ToolCall);
        assert_eq!(call.tool_name, "lookup");
        assert_eq!(call.arguments.as_ref().unwrap().get(), arguments);
        assert_eq!(
            response.message.finish_reason,
            Some(FinishReason::ToolCalls)
        );
        assert_eq!(response.usage().unwrap().output_tokens, 7);
    }
}

#[tokio::test]
async fn invalid_inputs_and_eof_return_errors_after_provisional_output_without_retrying() {
    for arguments in ["{\"private-marker\":", "[]", "{\"x\":1,\"x\":2}", "{}"] {
        let mut events = events(arguments, false);
        let expected = if arguments == "{}" {
            events.pop(); // A valid input is still provisional without message_stop.
            "Claude stream ended before message_stop"
        } else {
            "malformed Claude tool input"
        };
        let events: Vec<_> = events.iter().map(String::as_str).collect();
        let server = http::spawn_scripted(vec![http::Script::sse_without_done(&events)]).await;
        let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
        let mut output_started = false;
        let settlement = client
            .complete(
                &request(),
                &mut |event| {
                    if matches!(event, StreamEvent::ToolCallDelta { .. }) {
                        output_started = true;
                    }
                },
                &CancellationToken::new(),
            )
            .await;
        assert!(output_started);
        assert_eq!(settlement.attempts, 1);
        assert_eq!(server.count(), 1);
        let error = settlement.result.unwrap_err();
        assert!(matches!(error, ProviderError::Other(_)));
        assert!(error.to_string().starts_with(expected), "{error}");
        assert!(!error.to_string().contains("private-marker"));
    }
}

#[tokio::test]
async fn unfinished_message_remains_cancellable_after_metadata_and_tool_output() {
    let mut events = events("{}", false);
    events.pop();
    let events: Vec<_> = events.iter().map(String::as_str).collect();
    let server = http::spawn_scripted(vec![http::Script::stall_after_events(&events)]).await;
    let client = trusted_test_client(&server.url, "fixture-key", "2026-10-21", "");
    let cancel = CancellationToken::new();
    let request = request();
    let mut emit = |event| {
        if matches!(event, StreamEvent::ToolCallDelta { .. }) {
            cancel.cancel();
        }
    };
    let settlement = tokio::time::timeout(
        Duration::from_secs(5),
        client.complete(&request, &mut emit, &cancel),
    )
    .await
    .expect("unfinished stream must remain cancellable");
    assert!(matches!(settlement.result, Err(ProviderError::Cancelled)));
    assert_eq!(settlement.attempts, 1);
    assert_eq!(server.count(), 1);
}
