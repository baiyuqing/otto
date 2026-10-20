use super::*;
use crate::model::{Block, Message, Role, ToolDefinition};
use serde_json::value::RawValue;

fn raw(text: &str) -> Box<RawValue> {
    RawValue::from_string(text.to_owned()).expect("valid JSON")
}

#[test]
fn prompt_cache_key_is_sent_only_when_the_request_has_a_cache_key() {
    let keyed = Request {
        cache_key: "s1".into(),
        ..Request::default()
    };
    let body = serde_json::to_string(&build_request(&keyed)).unwrap();
    assert!(body.contains(r#""prompt_cache_key":"s1""#), "{body}");
    let body = serde_json::to_string(&build_request(&Request::default())).unwrap();
    assert!(!body.contains("prompt_cache_key"), "{body}");
}

#[test]
fn hosted_web_search_is_one_extra_typed_tool_after_the_functions() {
    let request = Request {
        model: "m".into(),
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "d".into(),
            parameters: None,
        }],
        ..Request::default()
    };
    let with = serde_json::to_string(&build_request_with(&request, true)).unwrap();
    assert!(
            with.contains(r#""tools":[{"type":"function","name":"read","description":"d","parameters":null},{"type":"web_search"}]"#),
            "{with}"
        );
    let without = serde_json::to_string(&build_request_with(&request, false)).unwrap();
    assert!(!without.contains("web_search"), "{without}");
    assert_eq!(
        without,
        serde_json::to_string(&build_request(&request)).unwrap()
    );
    // With no function tools the array still carries the hosted one.
    let only = serde_json::to_string(&build_request_with(&Request::default(), true)).unwrap();
    assert!(
        only.contains(r#""tools":[{"type":"web_search"}]"#),
        "{only}"
    );
    assert_eq!(
        serialized_request_size_with(&request, true).unwrap(),
        with.len()
    );
}

fn encode(request: &Request) -> String {
    serde_json::to_string(&build_request(request)).expect("request serializes")
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn compaction_items_wrap_messages_in_the_required_order() {
    let request = Request {
        model: "m".into(),
        remote_compaction: Some(crate::session::RemoteCompaction {
            provider: "openai-compatible".into(),
            id: "cmp_1".into(),
            encrypted_content: "opaque-persisted".into(),
        }),
        messages: vec![Message {
            role: Role::User,
            blocks: vec![Block::text("hello")],
            ..Message::default()
        }],
        request_remote_compaction: true,
        ..Request::default()
    };
    assert_eq!(
        encode(&request),
        concat!(
            r#"{"model":"m","input":["#,
            r#"{"type":"compaction","id":"cmp_1","encrypted_content":"opaque-persisted"},"#,
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},"#,
            r#"{"type":"compaction"}],"stream":true,"store":false}"#,
        )
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn empty_compaction_fields_are_omitted_exactly() {
    let item = WireItem {
        item_type: "compaction".into(),
        ..WireItem::default()
    };
    assert_eq!(
        serde_json::to_string(&item).unwrap(),
        r#"{"type":"compaction"}"#
    );
}

/// The Responses API requires every `function_call_output` item to carry an
/// `output` field, even when the tool produced no text; dropping it yields
/// HTTP 400 "Missing required parameter: 'input[N].output'".
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn empty_tool_result_keeps_the_output_field() {
    let request = Request {
        model: "gpt-5".into(),
        messages: vec![Message {
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                tool_call_id: "call_1".into(),
                text: String::new(),
                ..Block::default()
            }],
            ..Message::default()
        }],
        ..Request::default()
    };
    let item = serde_json::to_string(&build_request(&request).input[0]).expect("item encodes");
    assert_eq!(
        item,
        r#"{"type":"function_call_output","call_id":"call_1","output":""}"#
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn empty_request_encodes_only_the_required_fields() {
    let request = Request {
        model: "m".into(),
        ..Request::default()
    };
    assert_eq!(
        encode(&request),
        r#"{"model":"m","input":null,"stream":true,"store":false}"#
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn full_request_encodes_every_field() {
    let request = Request {
        cache_key: String::new(),
        model: "test-model".into(),
        system_prompt: "sys".into(),
        thinking: "high".into(),
        messages: vec![
            Message {
                role: Role::User,
                blocks: vec![Block::text("hello")],
                ..Message::default()
            },
            Message {
                role: Role::Assistant,
                blocks: vec![
                    Block::reasoning("never sent"),
                    Block::text("sure"),
                    Block {
                        block_type: BlockType::ToolCall,
                        tool_call_id: "call-1".into(),
                        tool_name: "read".into(),
                        arguments: Some(raw(r#"{"path":"README.md"}"#)),
                        ..Block::default()
                    },
                ],
                ..Message::default()
            },
            Message {
                role: Role::Tool,
                blocks: vec![Block {
                    block_type: BlockType::ToolResult,
                    text: "file body".into(),
                    tool_call_id: "call-1".into(),
                    tool_name: "read".into(),
                    ..Block::default()
                }],
                ..Message::default()
            },
        ],
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            parameters: Some(raw(r#"{"alpha":{"a":3,"b":2},"zeta":1}"#)),
        }],
        ..Request::default()
    };
    assert_eq!(
        encode(&request),
        concat!(
            r#"{"model":"test-model","instructions":"sys","input":["#,
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},"#,
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"sure"}]},"#,
            r#"{"type":"function_call","call_id":"call-1","name":"read","arguments":"{\"path\":\"README.md\"}"},"#,
            r#"{"type":"function_call_output","call_id":"call-1","output":"file body"}],"#,
            r#""tools":[{"type":"function","name":"read","description":"read a file","parameters":{"alpha":{"a":3,"b":2},"zeta":1}}],"#,
            r#""reasoning":{"effort":"high","summary":"auto"},"stream":true,"store":false}"#
        )
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn assistant_message_without_text_emits_only_its_tool_calls() {
    let request = Request {
        model: "m".into(),
        messages: vec![Message {
            role: Role::Assistant,
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: "c1".into(),
                tool_name: "read".into(),
                arguments: Some(raw("{}")),
                ..Block::default()
            }],
            ..Message::default()
        }],
        ..Request::default()
    };
    assert_eq!(
        encode(&request),
        concat!(
            r#"{"model":"m","input":[{"type":"function_call","call_id":"c1","name":"read","arguments":"{}"}],"#,
            r#""stream":true,"store":false}"#
        )
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn context_messages_become_user_messages_and_tool_definitions_keep_null_parameters() {
    let request = Request {
        model: "m".into(),
        messages: vec![Message {
            role: Role::Context,
            context_type: "note".into(),
            blocks: vec![Block::text("recalled")],
            ..Message::default()
        }],
        tools: vec![ToolDefinition {
            name: "n".into(),
            description: "d".into(),
            parameters: None,
        }],
        ..Request::default()
    };
    assert_eq!(
        encode(&request),
        concat!(
            r#"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"recalled"}]}],"#,
            r#""tools":[{"type":"function","name":"n","description":"d","parameters":null}],"#,
            r#""stream":true,"store":false}"#
        )
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn user_images_become_response_content_parts() {
    let request = Request {
        model: "m".into(),
        messages: vec![Message {
            role: Role::User,
            blocks: vec![
                Block::text("read it"),
                Block::image("iVBORw0KGgo=", "image/png"),
            ],
            ..Message::default()
        }],
        ..Request::default()
    };
    assert_eq!(
        encode(&request),
        concat!(
            r#"{"model":"m","input":[{"type":"message","role":"user","content":["#,
            r#"{"type":"input_text","text":"read it"},"#,
            r#"{"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo=","detail":"high"}]}],"#,
            r#""stream":true,"store":false}"#
        )
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn non_tool_result_blocks_of_a_tool_message_are_dropped() {
    let request = Request {
        model: "m".into(),
        messages: vec![Message {
            role: Role::Tool,
            blocks: vec![Block::text("ignored")],
            ..Message::default()
        }],
        ..Request::default()
    };
    assert_eq!(
        encode(&request),
        r#"{"model":"m","input":null,"stream":true,"store":false}"#
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn finish_reason_covers_the_three_outcomes() {
    assert_eq!(
        finish_reason(true, "incomplete", "max_output_tokens"),
        FinishReason::ToolCalls
    );
    assert_eq!(
        finish_reason(false, "incomplete", "max_output_tokens"),
        FinishReason::Length
    );
    assert_eq!(
        finish_reason(false, "incomplete", "content_filter"),
        FinishReason::Stop
    );
    assert_eq!(finish_reason(false, "completed", ""), FinishReason::Stop);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn serialized_request_size_is_the_encoded_byte_length() {
    let request = Request {
        model: "m".into(),
        ..Request::default()
    };
    assert_eq!(
        serialized_request_size(&request).expect("size"),
        encode(&request).len()
    );
    assert_eq!(serialized_request_size(&request).expect("size"), 54);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn events_tolerate_null_fields_and_missing_objects() {
    let event: WireEvent = serde_json::from_str(
            r#"{"type":"response.output_text.delta","delta":null,"item_id":null,"item":null,"response":null}"#,
        )
        .expect("event decodes");
    assert_eq!(event.event_type, "response.output_text.delta");
    assert_eq!(event.delta, "");
    assert_eq!(event.item_id, "");
    assert_eq!(event.item, None);
    assert_eq!(event.response, None);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn cached_tokens_default_to_zero_without_a_breakdown() {
    let usage: WireUsage =
        serde_json::from_str(r#"{"input_tokens":5,"output_tokens":7}"#).expect("usage decodes");
    assert_eq!(usage.cached_tokens(), 0);
    assert!(!usage.cached_tokens_present());
    let detailed: WireUsage = serde_json::from_str(
        r#"{"input_tokens":5,"output_tokens":7,"input_tokens_details":{"cached_tokens":4}}"#,
    )
    .expect("usage decodes");
    assert_eq!(detailed.cached_tokens(), 4);
    assert!(detailed.cached_tokens_present());
    let explicit_zero: WireUsage = serde_json::from_str(
        r#"{"input_tokens":5,"output_tokens":7,"input_tokens_details":{"cached_tokens":0}}"#,
    )
    .expect("usage decodes");
    assert!(explicit_zero.cached_tokens_present());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn valid_arguments_matches_json_validity() {
    assert!(valid_arguments("{}"));
    assert!(valid_arguments(r#"{"a":1}"#));
    assert!(!valid_arguments("{"));
    assert!(!valid_arguments(""));
}
