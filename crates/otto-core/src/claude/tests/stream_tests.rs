//! Compatibility and fail-closed completion cases for the Claude assembler.
use super::{decode, event, start, stop};
use crate::claude::stream::{StreamAssembler, StreamError};
use crate::model::{BlockType, FinishReason};
use serde_json::{Value, json};

fn block(index: u64, content: Value) -> String {
    event(json!({"type":"content_block_start","index":index,"content_block":content}))
}
fn tool(index: u64, input: Value) -> String {
    block(
        index,
        json!({"type":"tool_use","id":format!("call_{index}"),"name":"lookup","input":input}),
    )
}
fn input(index: u64, fragment: &str) -> String {
    event(json!({"type":"content_block_delta","index":index,
        "delta":{"type":"input_json_delta","partial_json":fragment}}))
}
fn block_stop(index: u64) -> String {
    event(json!({"type":"content_block_stop","index":index}))
}
fn metadata(delta: Value, usage: Value) -> String {
    event(json!({"type":"message_delta","delta":delta,"usage":usage}))
}
fn message_stop() -> String {
    event(json!({"type":"message_stop"}))
}

#[test]
fn metadata_updates_do_not_close_blocks_and_message_stop_finalizes_all_inputs() {
    let arguments = "{\"id\":900719925474099312345,\"decimal\":0.123456789012345678901}";
    for late_stop in [false, true] {
        let mut body = start()
            + &tool(0, json!({}))
            + &input(0, &arguments[..20])
            + &metadata(json!({"stop_reason":null}), json!({"output_tokens":3}))
            + &input(0, &arguments[20..])
            + &metadata(
                json!({"stop_reason":"tool_use"}),
                json!({"output_tokens":9}),
            );
        if late_stop {
            body += &block_stop(0);
        }
        body += &metadata(json!({}), json!({"input_tokens":null,"output_tokens":null}));
        body += &message_stop();
        for chunk_size in [1, 2, 7, 83, body.len()] {
            let mut assembler = StreamAssembler::new();
            for chunk in body.as_bytes().chunks(chunk_size) {
                assembler.push(chunk, &mut |_| {}).unwrap();
            }
            assert!(assembler.is_done());
            let response = assembler.finish(&mut |_| {}).unwrap();
            assert_eq!(response.message.blocks.len(), 1);
            let call = &response.message.blocks[0];
            assert_eq!(call.block_type, BlockType::ToolCall);
            assert_eq!(call.arguments.as_ref().unwrap().get(), arguments);
            assert_eq!(
                response.message.finish_reason,
                Some(FinishReason::ToolCalls)
            );
            let usage = response.usage().unwrap();
            assert_eq!(usage.input_tokens, 20);
            assert_eq!(usage.output_tokens, 9);
        }
    }
}

#[test]
fn blocks_can_start_and_interleave_after_metadata_and_stop_independently() {
    let body = start()
        + &metadata(json!({}), Value::Null)
        + &tool(0, json!({}))
        + &block(1, json!({"type":"text","text":"你"}))
        + &input(0, "{\"ok\":")
        + &block(2, json!({"type":"future_block","private":"ignored"}))
        + &metadata(json!({"stop_reason":"tool_use"}), Value::Null)
        + &event(
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"好"}}),
        )
        + &block_stop(1)
        + &event(json!({"type":"content_block_delta","index":2,"delta":{"type":"future_delta"}}))
        + &input(0, "true}")
        + &message_stop();
    let response = decode(&body).unwrap();
    assert_eq!(response.message.text(), "你好");
    assert_eq!(response.message.blocks.len(), 2);
    assert_eq!(
        response.message.blocks[0].arguments.as_ref().unwrap().get(),
        "{\"ok\":true}"
    );
}

#[test]
fn empty_input_fragments_preserve_initial_arguments_and_nonempty_fragments_replace_them() {
    for explicit_stop in [false, true] {
        for fragments in [vec![], vec![""], vec!["", "{\"new\":1}", ""]] {
            let mut body = start() + &tool(0, json!({"initial":42}));
            for fragment in &fragments {
                body += &input(0, fragment);
            }
            if explicit_stop {
                body += &block_stop(0);
            }
            body += &stop("tool_use");
            let response = decode(&body).unwrap();
            let raw = response.message.blocks[0].arguments.as_ref().unwrap();
            let expected = if fragments.len() == 3 {
                json!({"new":1})
            } else {
                json!({"initial":42})
            };
            assert_eq!(serde_json::from_str::<Value>(raw.get()).unwrap(), expected);
        }
    }
}

#[test]
fn initial_tool_arguments_preserve_numeric_literals() {
    let raw = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c","name":"f","input":{"id":900719925474099312345,"decimal":0.123456789012345678901}}}"#;
    let body = start() + "data: " + raw + "\n\n" + &stop("tool_use");
    let response = decode(&body).unwrap();
    assert_eq!(
        response.message.blocks[0].arguments.as_ref().unwrap().get(),
        r#"{"id":900719925474099312345,"decimal":0.123456789012345678901}"#
    );
}

#[test]
fn malformed_tool_inputs_fail_at_both_completion_boundaries_without_marking_done() {
    for arguments in [
        "{",
        "{\"x\":",
        "[]",
        "null",
        "1",
        "{}{}",
        "{\"x\":1,\"x\":2}",
        "{\"x\":{\"y\":1,\"y\":2}}",
        " ",
    ] {
        for explicit_stop in [false, true] {
            let mut body = start()
                + &tool(0, json!({"initial":"must not become fallback"}))
                + &input(0, arguments);
            if explicit_stop {
                body += &block_stop(0);
            }
            body += &stop("tool_use");
            let mut assembler = StreamAssembler::new();
            let error = assembler.push(body.as_bytes(), &mut |_| {}).unwrap_err();
            assert_eq!(error, StreamError::MalformedArguments);
            assert!(!assembler.is_done());
            assert_eq!(error.to_string(), "malformed Claude tool input");
        }
    }
}

#[test]
fn eof_and_missing_terminal_metadata_never_complete_a_message() {
    let prefix = start() + &tool(0, json!({})) + &input(0, "{}");
    for ending in [
        String::new(),
        metadata(json!({"stop_reason":"tool_use"}), Value::Null),
    ] {
        assert_eq!(
            decode(&(prefix.clone() + &ending)),
            Err(StreamError::MissingStop)
        );
    }
    for delta in [
        String::new(),
        metadata(json!({}), Value::Null),
        metadata(json!({"stop_reason":null}), Value::Null),
    ] {
        let mut assembler = StreamAssembler::new();
        let body = prefix.clone() + &delta + &message_stop();
        assert_eq!(
            assembler.push(body.as_bytes(), &mut |_| {}),
            Err(StreamError::Sequence("message_stop: missing stop_reason"))
        );
        assert!(!assembler.is_done());
    }
}

#[test]
fn partial_metadata_preserves_values_and_cumulative_usage_replaces_totals() {
    let body = start()
        + &metadata(
            json!({"stop_reason":"end_turn"}),
            json!({"input_tokens":30,"output_tokens":7}),
        )
        + &event(json!({"type":"message_delta","delta":{}}))
        + &metadata(
            json!({"stop_reason":null,"future":"ignored"}),
            json!({"input_tokens":null,"output_tokens":9}),
        )
        + &message_stop();
    let response = decode(&body).unwrap();
    assert_eq!(response.message.finish_reason, Some(FinishReason::Stop));
    let usage = response.usage().unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (30, 9));
    assert_eq!(
        decode(&(start() + &stop("future_reason")))
            .unwrap()
            .message
            .finish_reason,
        Some(FinishReason::Unknown)
    );
}

#[test]
fn malformed_metadata_is_rejected_even_after_a_valid_update() {
    let prefix = start()
        + &metadata(
            json!({"stop_reason":"end_turn"}),
            json!({"output_tokens":7}),
        );
    let mut malformed = vec![
        event(json!({"type":"message_delta"})),
        metadata(Value::Null, Value::Null),
        metadata(json!([]), Value::Null),
        metadata(json!({"stop_reason":42}), Value::Null),
        metadata(json!({"stop_reason":""}), Value::Null),
        metadata(json!({}), json!([])),
    ];
    for key in ["input_tokens", "output_tokens"] {
        for count in [
            json!(-1),
            json!(1.5),
            json!("1"),
            json!(u64::MAX),
            json!(true),
        ] {
            malformed.push(metadata(json!({}), json!({key:count})));
        }
    }
    for update in malformed {
        assert_eq!(
            decode(&(prefix.clone() + &update + &message_stop())),
            Err(StreamError::Decode)
        );
    }
}

#[test]
fn known_content_errors_and_api_errors_remain_fatal_after_metadata() {
    let prefix =
        start() + &tool(0, json!({})) + &metadata(json!({"stop_reason":"tool_use"}), Value::Null);
    let stopped = prefix.clone() + &block_stop(0);
    for body in [
        stopped.clone() + &input(0, "{}"),
        stopped.clone() + &block_stop(0),
        prefix.clone() + &tool(0, json!({})),
        prefix.clone()
            + &event(
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"private-marker"}}),
            ),
    ] {
        let error = decode(&body).unwrap_err();
        assert!(matches!(error, StreamError::Sequence(_)));
        assert!(!error.to_string().contains("private-marker"));
    }
    assert_eq!(
        decode(&(prefix + &event(json!({"type":"error","error":{"message":"private-marker"}})))),
        Err(StreamError::Api)
    );
}

#[test]
fn message_stop_validates_every_open_tool_before_marking_the_response_done() {
    let body = start()
        + &tool(0, json!({}))
        + &input(0, "{\"ok\":true}")
        + &tool(1, json!({}))
        + &input(1, "{\"unfinished\":")
        + &stop("tool_use");
    let mut assembler = StreamAssembler::new();
    assert_eq!(
        assembler.push(body.as_bytes(), &mut |_| {}),
        Err(StreamError::MalformedArguments)
    );
    assert!(!assembler.is_done());
}
