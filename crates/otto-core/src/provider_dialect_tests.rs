use serde_json::{json, value::RawValue};

use crate::claude::protocol as claude;
use crate::model::{Block, BlockType, Message, Role};
use crate::openaicompat::protocol as compatible;
use crate::provider::Request;

fn continuation(model: &str) -> Request {
    Request {
        model: model.into(),
        thinking: "high".into(),
        cache_key: "fixture-cache".into(),
        messages: vec![
            Message {
                role: Role::User,
                blocks: vec![Block::text("Read the file")],
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                blocks: vec![
                    Block::reasoning("internal"),
                    Block {
                        block_type: BlockType::ToolCall,
                        tool_call_id: "call-1".into(),
                        tool_name: "read".into(),
                        arguments: Some(RawValue::from_string("{}".into()).unwrap()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            Message {
                role: Role::Tool,
                blocks: vec![Block {
                    block_type: BlockType::ToolResult,
                    text: "fixture content".into(),
                    tool_call_id: "call-1".into(),
                    tool_name: "read".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

#[test]
fn official_deepseek_disables_thinking_on_initial_request_and_tool_continuation() {
    for model in ["deepseek-flash", "deepseek-v4-pro"] {
        let full = continuation(model);
        for messages in [full.messages[..1].to_vec(), full.messages.clone()] {
            let is_continuation = messages.len() == 3;
            let request = Request {
                messages,
                ..full.clone()
            };
            let dialect = compatible::RequestDialect::OfficialDeepSeek;
            let body =
                serde_json::to_vec(&compatible::build_request_for(&request, dialect)).unwrap();
            assert_eq!(
                body.len(),
                compatible::serialized_request_size_for(&request, dialect).unwrap()
            );
            let wire: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(wire["thinking"], json!({"type": "disabled"}));
            assert!(wire.get("reasoning_effort").is_none());
            assert!(wire.get("prompt_cache_key").is_none());
            assert!(wire["messages"][1].get("reasoning_content").is_none());
            if is_continuation {
                assert_eq!(wire["messages"][2]["role"], "tool");
                assert_eq!(wire["messages"][2]["tool_call_id"], "call-1");
            }
            let generic = serde_json::to_value(compatible::build_request(&request)).unwrap();
            assert!(generic.get("thinking").is_none());
            assert_eq!(generic["reasoning_effort"], "high");
            assert_eq!(generic["prompt_cache_key"], "fixture-cache");
        }
    }
}

#[test]
fn official_claude_haiku_disables_thinking_on_initial_request_and_tool_continuation() {
    let full = continuation("claude-haiku-5-5");
    let dialect = claude::RequestDialect::OfficialClaude;
    for messages in [full.messages[..1].to_vec(), full.messages.clone()] {
        let is_continuation = messages.len() == 3;
        let request = Request {
            messages,
            ..full.clone()
        };
        let body =
            serde_json::to_vec(&claude::build_request_for(&request, dialect).unwrap()).unwrap();
        assert_eq!(
            body.len(),
            claude::serialized_request_size_for(&request, dialect).unwrap()
        );
        let wire: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(wire["thinking"], json!({"type": "disabled"}));
        assert!(wire.get("output_config").is_none());
        if is_continuation {
            assert_eq!(wire["messages"][2]["content"][0]["type"], "tool_result");
            assert_eq!(wire["messages"][2]["content"][0]["tool_use_id"], "call-1");
        }
        let generic = serde_json::to_value(claude::build_request(&request).unwrap()).unwrap();
        assert!(generic.get("thinking").is_none());
    }
    for model in [
        "claude-opus-4-8",
        "claude-sonnet-4-6",
        "claude-fable-5-2",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
    ] {
        let wire =
            serde_json::to_value(claude::build_request_for(&continuation(model), dialect).unwrap())
                .unwrap();
        assert!(wire.get("thinking").is_none());
    }
}
