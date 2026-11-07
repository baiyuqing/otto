use super::{
    overflow, protocol,
    stream::{MAX_EVENT_BYTES, StreamAssembler, StreamError},
};
use crate::model::{Block, BlockType, FinishReason, Message, Role, ToolDefinition};
use crate::provider::{Request, StreamEvent};
use serde_json::{Value, json, value::RawValue};

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}
fn start() -> String {
    event(
        json!({"type":"message_start","message":{"id":"msg_1","role":"assistant","content":[],"usage":{"input_tokens":20,"output_tokens":1}}}),
    )
}
fn stop(reason: &str) -> String {
    event(
        json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":7}}),
    ) + &event(json!({"type":"message_stop"}))
}
fn decode(body: &str) -> Result<crate::provider::Response, StreamError> {
    let mut assembler = StreamAssembler::new();
    assembler.push(body.as_bytes(), &mut |_| {})?;
    assembler.finish(&mut |_| {})
}
#[test]
fn completion_is_visible_without_socket_eof() {
    let mut assembler = StreamAssembler::new();
    assert!(!assembler.is_done());
    assembler.push(start().as_bytes(), &mut |_| {}).unwrap();
    assert!(!assembler.is_done());
    let ending = stop("end_turn");
    let stop_offset = ending.find("event: message_stop").unwrap();
    let ending_bytes = ending.as_bytes();
    assembler
        .push(&ending_bytes[..stop_offset], &mut |_| {})
        .unwrap();
    assert!(!assembler.is_done());
    assembler
        .push(&ending_bytes[stop_offset..], &mut |_| {})
        .unwrap();
    assert!(assembler.is_done());
    assembler
        .push(b"invalid trailing bytes", &mut |_| {})
        .unwrap();
    assert!(assembler.is_done());
    assert!(assembler.finish(&mut |_| {}).is_ok());
    let mut invalid = StreamAssembler::new();
    assert_eq!(
        invalid.push(
            event(json!({"type":"message_stop"})).as_bytes(),
            &mut |_| {}
        ),
        Err(StreamError::Sequence("message_stop: before message_start"))
    );
    assert!(!invalid.is_done());
}
#[test]
fn maps_messages_and_tools_without_openai_fields() {
    let call = Block {
        block_type: BlockType::ToolCall,
        tool_call_id: "call_1".into(),
        tool_name: "lookup".into(),
        arguments: Some(RawValue::from_string("{\"id\":9007199254740993}".into()).unwrap()),
        ..Block::default()
    };
    let result = Block {
        block_type: BlockType::ToolResult,
        tool_call_id: "call_1".into(),
        text: "not found".into(),
        is_error: true,
        ..Block::default()
    };
    let request = Request {
        model: "fixture-model".into(),
        system_prompt: "be useful".into(),
        thinking: "ignored".into(),
        cache_key: "ignored".into(),
        tools: vec![ToolDefinition {
            name: "lookup".into(),
            ..ToolDefinition::default()
        }],
        messages: vec![
            Message {
                role: Role::Assistant,
                blocks: vec![call],
                ..Message::default()
            },
            Message {
                role: Role::Tool,
                blocks: vec![result],
                ..Message::default()
            },
        ],
        ..Request::default()
    };
    let encoded = serde_json::to_vec(&protocol::build_request(&request).unwrap()).unwrap();
    assert_eq!(
        encoded.len(),
        protocol::serialized_request_size(&request).unwrap()
    );
    let body: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(body["system"], "be useful");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(
        body["messages"][0]["content"][0]["input"]["id"],
        9007199254740993u64
    );
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"][0]["is_error"], true);
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    for absent in [
        "thinking",
        "cache_key",
        "stream_options",
        "reasoning_effort",
    ] {
        assert!(body.get(absent).is_none());
    }
}
#[test]
fn coalesces_context_and_user_and_rejects_images() {
    let mut request = Request::default();
    for role in [Role::User, Role::Context] {
        request.messages.push(Message {
            role,
            blocks: vec![Block {
                block_type: BlockType::Text,
                text: "hello".into(),
                ..Block::default()
            }],
            ..Message::default()
        });
    }
    let wire = protocol::build_request(&request).unwrap();
    assert_eq!(wire.messages.len(), 1);
    assert_eq!(wire.messages[0].content.len(), 2);
    request.messages[0].blocks[0].block_type = BlockType::Image;
    assert!(protocol::build_request(&request).is_err());
}
#[test]
fn assembles_text_tool_usage_and_arbitrary_byte_splits() {
    let body = start()
        + &event(json!({"type":"ping"}))
        + &event(json!({"type":"future_event","extension":true}))
        + &event(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"你"}}),
        )
        + &event(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"好"}}),
        )
        + &event(json!({"type":"content_block_stop","index":0}))
        + &event(
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"lookup","input":{}}}),
        )
        + &event(
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"key\":"}}),
        )
        + &event(
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"9007199254740993}"}}),
        )
        + &event(json!({"type":"content_block_stop","index":1}))
        + &stop("tool_use");
    for chunk_size in [1, 2, 7, 83, body.len()] {
        let mut assembler = StreamAssembler::new();
        let mut events = Vec::new();
        for chunk in body.as_bytes().chunks(chunk_size) {
            assembler
                .push(chunk, &mut |event| events.push(event))
                .unwrap();
        }
        let response = assembler.finish(&mut |event| events.push(event)).unwrap();
        assert_eq!(response.message.text(), "你好");
        assert_eq!(response.message.blocks.len(), 2);
        assert_eq!(
            response.message.blocks[1].arguments.as_ref().unwrap().get(),
            "{\"key\":9007199254740993}"
        );
        assert_eq!(
            response.message.finish_reason,
            Some(FinishReason::ToolCalls)
        );
        assert_eq!(response.usage().unwrap().input_tokens, 20);
        assert_eq!(response.usage().unwrap().output_tokens, 7);
        assert!(events.iter().any(|event| matches!(event, StreamEvent::ToolCallDelta { tool_name, .. } if tool_name == "lookup")));
    }
}
#[test]
fn unknown_block_delta_and_stop_reason_are_forward_compatible() {
    let body = start()
        + &event(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"future_block"}}),
        )
        + &event(json!({"type":"content_block_delta","index":0,"delta":{"type":"future_delta"}}))
        + &event(json!({"type":"content_block_stop","index":0}))
        + &stop("future_reason");
    let response = decode(&body).unwrap();
    assert!(response.message.blocks.is_empty());
    assert_eq!(response.message.finish_reason, Some(FinishReason::Unknown));
    assert_eq!(protocol::finish_reason("end_turn"), FinishReason::Stop);
    assert_eq!(protocol::finish_reason("stop_sequence"), FinishReason::Stop);
    assert_eq!(protocol::finish_reason("max_tokens"), FinishReason::Length);
}
#[test]
fn rejects_duplicate_malformed_and_out_of_order_input() {
    assert_eq!(
        decode("data: {\"type\":\"ping\",\"type\":\"message_stop\"}\n\n"),
        Err(StreamError::Decode)
    );
    assert_eq!(decode("data: {broken}\n\n"), Err(StreamError::Decode));
    assert_eq!(
        decode(&event(json!({"type":"message_stop"}))),
        Err(StreamError::Sequence("message_stop: before message_start"))
    );
    assert_eq!(decode(&start()), Err(StreamError::MissingStop));
    assert_eq!(
        decode(&(start() + &start())),
        Err(StreamError::Sequence("message_start: duplicate start"))
    );
    let block = event(
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
    );
    assert_eq!(
        decode(&(start() + &block + &block)),
        Err(StreamError::Sequence(
            "content_block_start: duplicate block index"
        ))
    );
    let bad = start()
        + &event(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c","name":"f","input":{}}}),
        )
        + &event(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"[]"}}),
        )
        + &event(json!({"type":"content_block_stop","index":0}));
    assert_eq!(decode(&bad), Err(StreamError::MalformedArguments));
    assert_eq!(
        decode(&event(
            json!({"type":"error","error":{"message":"private"}})
        )),
        Err(StreamError::Api)
    );
}
#[test]
fn supports_crlf_multiline_and_final_unterminated_event() {
    let body = start() + "data: {\"type\":\n" + "data: \"ping\"}\n\n" + &stop("end_turn");
    assert!(decode(body.trim_end()).is_ok());
    assert!(decode(&body.replace('\n', "\r\n")).is_ok());
}
#[test]
fn rejects_oversize_lines_and_sparse_indexes() {
    let mut assembler = StreamAssembler::new();
    assert_eq!(
        assembler.push(&vec![b'a'; MAX_EVENT_BYTES + 1], &mut |_| {}),
        Err(StreamError::Limit)
    );
    let body = start()
        + &event(
            json!({"type":"content_block_start","index":u64::MAX,"content_block":{"type":"text","text":""}}),
        );
    assert_eq!(decode(&body), Err(StreamError::Limit));
}
#[test]
fn rejects_negative_usage_duplicate_tool_ids_and_argument_keys() {
    let negative = start().replace("\"input_tokens\":20", "\"input_tokens\":-1");
    assert_eq!(decode(&negative), Err(StreamError::Decode));
    let tool = |index| {
        event(json!({"type":"content_block_start","index":index,
        "content_block":{"type":"tool_use","id":"same","name":"lookup","input":{}}}))
    };
    assert_eq!(
        decode(&(start() + &tool(0) + &tool(1))),
        Err(StreamError::Sequence(
            "content_block_start: empty tool id/name or duplicate tool id"
        ))
    );
    let body = start()
        + &tool(0)
        + &event(json!({"type":"content_block_delta","index":0,
            "delta":{"type":"input_json_delta","partial_json":"{\"x\":1,\"x\":2}"}}))
        + &event(json!({"type":"content_block_stop","index":0}));
    assert_eq!(decode(&body), Err(StreamError::MalformedArguments));
}
#[test]
fn bounds_multiline_event_and_total_stream_work() {
    let mut assembler = StreamAssembler::new();
    let line = format!("data: {}\n", " ".repeat(MAX_EVENT_BYTES / 2));
    assembler.push(line.as_bytes(), &mut |_| {}).unwrap();
    assert_eq!(
        assembler.push(line.as_bytes(), &mut |_| {}),
        Err(StreamError::Limit)
    );
    let mut assembler = StreamAssembler::new();
    let comment = format!(":{}\n", "a".repeat(MAX_EVENT_BYTES - 2));
    for _ in 0..16 {
        assembler.push(comment.as_bytes(), &mut |_| {}).unwrap();
    }
    assert_eq!(
        assembler.push(comment.as_bytes(), &mut |_| {}),
        Err(StreamError::Limit)
    );
}
#[test]
fn overflow_is_narrow_and_content_free() {
    let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#;
    let error = overflow::classify_overflow(400, body).unwrap();
    assert_eq!(error.current_tokens, 210000);
    assert_eq!(error.maximum_tokens, 200000);
    assert!(!error.to_string().contains("prompt is too long"));
    assert!(overflow::classify_overflow(413, body).is_none());
    for message in [
        "max_tokens is too large",
        "request too large",
        "prompt is too long: private",
        "prompt is too long: 1 tokens > 2 maximum",
    ] {
        let body = json!({"error":{"type":"invalid_request_error","message":message}}).to_string();
        assert!(overflow::classify_overflow(400, body.as_bytes()).is_none());
    }
    assert!(overflow::classify_overflow(400, br#"{"error":{"type":"invalid_request_error","type":"other","message":"prompt is too long: 3 tokens > 2 maximum"}}"#).is_none());
}

#[test]
fn sequence_errors_identify_the_violated_rule_without_response_content() {
    let text_block = event(json!({"type":"content_block_start","index":0,
        "content_block":{"type":"text","text":"private-response-marker"}}));
    let tool_block = |index| {
        event(json!({"type":"content_block_start","index":index,
        "content_block":{"type":"tool_use","id":"private-tool-marker",
        "name":"private-name-marker","input":{}}}))
    };
    let block_stop = event(json!({"type":"content_block_stop","index":0}));
    let message_delta = event(json!({"type":"message_delta",
        "delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}));
    let text_delta = event(json!({"type":"content_block_delta","index":0,
        "delta":{"type":"text_delta","text":"private-response-marker"}}));
    let input_delta = event(json!({"type":"content_block_delta","index":0,
        "delta":{"type":"input_json_delta","partial_json":"private-input-marker"}}));
    let started = start();
    let ended = started.clone() + &message_delta;
    let mut cases = vec![
        (
            started.clone() + &message_delta + &message_delta,
            "message_delta: duplicate delta",
        ),
        (
            started.clone() + &text_block + &message_delta,
            "message_delta: content block still open",
        ),
        (
            started.clone() + &event(json!({"type":"message_stop"})),
            "message_stop: missing message_delta",
        ),
        (
            started.clone() + &text_delta,
            "content_block_delta: block missing or already stopped",
        ),
        (
            started.clone() + &block_stop,
            "content_block_stop: block missing or already stopped",
        ),
        (
            started.clone() + &text_block + &block_stop + &text_delta,
            "content_block_delta: block missing or already stopped",
        ),
        (
            started.clone() + &text_block + &block_stop + &block_stop,
            "content_block_stop: block missing or already stopped",
        ),
        (
            started.clone() + &text_block + &input_delta,
            "content_block_delta: input_json_delta on non-tool block",
        ),
        (
            started.clone() + &tool_block(0) + &text_delta,
            "content_block_delta: text_delta on non-text block",
        ),
        (
            started.clone() + &tool_block(0) + &tool_block(1),
            "content_block_start: empty tool id/name or duplicate tool id",
        ),
        (
            started.clone() + &text_block + &text_block,
            "content_block_start: duplicate block index",
        ),
        (started.clone() + &started, "message_start: duplicate start"),
        (message_delta.clone(), "message_delta: before message_start"),
        (
            event(json!({"type":"message_stop"})),
            "message_stop: before message_start",
        ),
    ];
    for (wire_event, before, after) in [
        (
            text_block,
            "content_block_start: before message_start",
            "content_block_start: after message_delta",
        ),
        (
            text_delta,
            "content_block_delta: before message_start",
            "content_block_delta: after message_delta",
        ),
        (
            block_stop,
            "content_block_stop: before message_start",
            "content_block_stop: after message_delta",
        ),
    ] {
        cases.push((wire_event.clone(), before));
        cases.push((ended.clone() + &wire_event, after));
    }
    for (body, reason) in cases {
        let error = decode(&body).unwrap_err();
        assert_eq!(error, StreamError::Sequence(reason));
        assert_eq!(
            error.to_string(),
            format!("invalid Claude stream sequence: {reason}")
        );
        assert!(!error.to_string().contains("private-"));
    }
}
