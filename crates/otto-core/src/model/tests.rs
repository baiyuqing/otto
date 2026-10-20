
use super::*;

fn raw(json: &str) -> Option<Box<RawValue>> {
    Some(RawValue::from_string(json.to_owned()).expect("valid JSON"))
}

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("in range")
}

fn tool_call(id: &str, name: &str, arguments: &str) -> Block {
    Block {
        block_type: BlockType::ToolCall,
        tool_call_id: id.into(),
        tool_name: name.into(),
        arguments: raw(arguments),
        ..Block::default()
    }
}

fn operation_metadata() -> ToolResultMetadata {
    ToolResultMetadata {
        operation_id: Some(OperationId::new("op-1").expect("valid operation id")),
        disposition: OperationDisposition::Interrupted,
        effect_certainty: EffectCertainty::Unknown,
        stop_reason: Some(OperationStopReason::ProcessLost),
    }
}

#[test]
fn operation_contract_json_and_validation() {
    assert_eq!(
        serde_json::to_string(&RetrySafety::IdempotentWithKey).expect("encode"),
        r#""idempotent_with_key""#
    );
    for valid in ["a", "op_1.test:part-2", &"x".repeat(128)] {
        assert_eq!(OperationId::new(valid).expect("valid").as_str(), valid);
    }
    for invalid in ["", "has space", "é", &"x".repeat(129)] {
        assert!(OperationId::new(invalid).is_err(), "accepted {invalid:?}");
    }
    let valid = OperationOutcome {
        disposition: OperationDisposition::Cancelled,
        effect_certainty: EffectCertainty::NotStarted,
        stop_reason: Some(OperationStopReason::UserCancellation),
    };
    valid.validate().expect("valid cancellation");
    assert_eq!(
        serde_json::to_string(&valid).expect("encode"),
        r#"{"disposition":"cancelled","effectCertainty":"not_started","stopReason":"user_cancellation"}"#
    );
    assert!(
        OperationOutcome {
            disposition: OperationDisposition::Succeeded,
            effect_certainty: EffectCertainty::Unknown,
            stop_reason: None,
        }
        .validate()
        .is_err()
    );
    assert!(
        OperationOutcome {
            disposition: OperationDisposition::DeadlineExceeded,
            effect_certainty: EffectCertainty::Unknown,
            stop_reason: Some(OperationStopReason::UserCancellation),
        }
        .validate()
        .is_err()
    );
}

#[test]
fn tool_result_operation_metadata_is_flattened_and_type_checked() {
    let result = Block {
        block_type: BlockType::ToolResult,
        text: "unavailable".into(),
        tool_call_id: "call-1".into(),
        tool_name: "bash".into(),
        is_error: true,
        operation_metadata: Some(operation_metadata()),
        ..Block::default()
    };
    result.validate().expect("valid tool result metadata");
    let encoded = serde_json::to_value(&result).expect("encode");
    assert_eq!(encoded["operation_id"], "op-1");
    assert_eq!(encoded["disposition"], "interrupted");
    assert_eq!(encoded["effect_certainty"], "unknown");
    assert_eq!(encoded["stop_reason"], "process_lost");
    assert!(encoded.get("operation_metadata").is_none());
    let decoded: Block = serde_json::from_value(encoded).expect("decode");
    assert_eq!(decoded, result);

    assert!(
        Block {
            operation_metadata: Some(operation_metadata()),
            ..Block::text("not a result")
        }
        .validate()
        .is_err()
    );
    assert!(serde_json::from_str::<Block>(
            r#"{"type":"text","text":"x","disposition":"interrupted","effect_certainty":"unknown"}"#,
        )
        .is_err());
    assert!(serde_json::from_str::<Block>(
            r#"{"type":"tool_result","tool_call_id":"c1","tool_name":"read","disposition":"interrupted"}"#,
        )
        .is_err());
}

#[test]
fn message_json_round_trip() {
    let original = Message {
        id: "msg-1".into(),
        role: Role::Assistant,
        created_at: at(10),
        blocks: vec![
            Block::text("checking"),
            tool_call("call-1", "read", r#"{"path":"README.md"}"#),
        ],
        finish_reason: Some(FinishReason::ToolCalls),
        usage: Some(Usage {
            input_tokens: 11,
            output_tokens: 7,
            cached_input_tokens: 0,
        }),
        ..Message::default()
    };
    let encoded = serde_json::to_string(&original).expect("encode");
    let decoded: Message = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(original, decoded);
}

#[test]
fn assistant_reasoning_block_is_valid_and_excluded_from_text() {
    let message = Message {
        id: "msg-r".into(),
        role: Role::Assistant,
        created_at: at(10),
        blocks: vec![Block::reasoning("plan"), Block::text("done")],
        finish_reason: Some(FinishReason::Stop),
        ..Message::default()
    };
    message.validate().expect("assistant may carry reasoning");
    assert_eq!(message.text(), "done");
    let encoded = serde_json::to_string(&message).expect("encode");
    assert!(
        encoded.contains(r#""type":"reasoning""#),
        "encoded: {encoded}"
    );
    let decoded: Message = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(message, decoded);
}

#[test]
fn reasoning_block_is_rejected_outside_assistant_and_when_malformed() {
    assert!(Block::reasoning("").validate().is_err());
    let with_tool = Block {
        tool_call_id: "c1".into(),
        ..Block::reasoning("x")
    };
    assert!(with_tool.validate().is_err());
    let user = Message {
        id: "u".into(),
        role: Role::User,
        blocks: vec![Block::reasoning("x")],
        ..Message::default()
    };
    assert!(user.validate().is_err());
}

#[test]
fn context_message_json_round_trip() {
    let original = Message {
        id: "context-1".into(),
        role: Role::Context,
        blocks: vec![Block::text("[Custom context: fixture]\ntext")],
        created_at: at(20),
        context_type: "fixture".into(),
        display: true,
        context_metadata: Some(ContextMetadata {
            task_id: "t1".into(),
        }),
        ..Message::default()
    };
    let encoded = serde_json::to_string(&original).expect("encode");
    let decoded: Message = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(original, decoded);
}

#[test]
fn tool_definition_parameters_preserve_large_integers() {
    let source = r#"{"name":"probe","description":"","parameters":{"type":"object","properties":{"id":{"minimum":9007199254740993}}}}"#;
    let decoded: ToolDefinition = serde_json::from_str(source).expect("decode");
    let parameters = decoded.parameters.as_ref().expect("parameters present");
    assert!(parameters.get().contains(r#""minimum":9007199254740993"#));
    let encoded = serde_json::to_string(&decoded).expect("encode");
    assert!(
        encoded.contains(r#""minimum":9007199254740993"#),
        "encoded: {encoded}"
    );
}

#[test]
fn tool_definition_null_parameters_decode_to_none() {
    let decoded: ToolDefinition =
        serde_json::from_str(r#"{"name":"probe","description":"","parameters":null}"#)
            .expect("decode");
    assert_eq!(decoded.parameters.as_ref().map(|raw| raw.get()), None);
    assert_eq!(
        serde_json::to_string(&decoded).expect("encode"),
        r#"{"name":"probe","description":"","parameters":null}"#
    );
}

#[test]
fn message_text_joins_only_text_blocks() {
    let message = Message {
        blocks: vec![
            Block::text("one"),
            Block {
                block_type: BlockType::ToolCall,
                tool_name: "read".into(),
                ..Block::default()
            },
            Block::text("two"),
        ],
        ..Message::default()
    };
    assert_eq!(message.text(), "onetwo");
}

#[test]
fn usage_validate() {
    let cases: &[(&str, Usage, bool)] = &[
        (
            "valid",
            Usage {
                input_tokens: 7,
                output_tokens: 2,
                cached_input_tokens: 3,
            },
            false,
        ),
        (
            "zero",
            Usage {
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
            },
            false,
        ),
        (
            "negative-input",
            Usage {
                input_tokens: -1,
                output_tokens: 1,
                cached_input_tokens: 0,
            },
            true,
        ),
        (
            "negative-output",
            Usage {
                input_tokens: 1,
                output_tokens: -1,
                cached_input_tokens: 0,
            },
            true,
        ),
        (
            "negative-cached",
            Usage {
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: -1,
            },
            true,
        ),
        (
            "cached-exceeds-input",
            Usage {
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: 2,
            },
            true,
        ),
    ];
    for (name, usage, want_err) in cases {
        assert_eq!(usage.validate().is_err(), *want_err, "case {name}");
    }
}

#[test]
fn message_validate_allows_transient_messages_and_rejects_invalid_shapes() {
    let valid = Message {
        role: Role::Assistant,
        ..Message::default()
    };
    assert_eq!(valid.validate(), Ok(()));

    let cases: &[Message] = &[
        Message {
            role: Role::Other("future".into()),
            blocks: vec![Block::text("x")],
            ..Message::default()
        },
        Message {
            role: Role::User,
            blocks: vec![Block {
                block_type: BlockType::Text,
                tool_name: "read".into(),
                ..Block::default()
            }],
            ..Message::default()
        },
        Message {
            role: Role::Assistant,
            finish_reason: Some(FinishReason::ToolCalls),
            blocks: vec![Block::text("x")],
            ..Message::default()
        },
        Message {
            role: Role::Assistant,
            blocks: vec![tool_call("c1", "read", "[]")],
            ..Message::default()
        },
        Message {
            role: Role::Context,
            context_type: "task_notification".into(),
            blocks: vec![Block::text("x")],
            context_metadata: Some(ContextMetadata {
                task_id: "bad".into(),
            }),
            ..Message::default()
        },
    ];
    for (index, message) in cases.iter().enumerate() {
        assert!(message.validate().is_err(), "case {index} was accepted");
    }

    let large_number = Message {
        role: Role::Assistant,
        finish_reason: Some(FinishReason::ToolCalls),
        blocks: vec![tool_call("c1", "read", r#"{"n":1e400}"#)],
        ..Message::default()
    };
    assert_eq!(
        large_number.validate(),
        Ok(()),
        "large JSON number was rejected"
    );
}

#[test]
fn message_validate_allows_context_usage_and_transient_identity() {
    let message = Message {
        role: Role::Context,
        blocks: vec![Block::text("notification")],
        usage: Some(Usage::default()),
        context_type: "task_notification".into(),
        context_metadata: Some(ContextMetadata {
            task_id: "t12".into(),
        }),
        ..Message::default()
    };
    assert_eq!(message.validate(), Ok(()));
}

#[test]
fn message_validate_rejects_unsupported_finish_reason() {
    let message = Message {
        role: Role::Assistant,
        finish_reason: Some(FinishReason::Other("refusal".into())),
        blocks: vec![Block::text("x")],
        ..Message::default()
    };
    assert_eq!(
        message.validate(),
        Err(ValidationError("unsupported assistant finish reason"))
    );
}

#[test]
fn block_validate_covers_each_type() {
    assert_eq!(Block::text("x").validate(), Ok(()));
    assert_eq!(
        Block {
            block_type: BlockType::Text,
            is_error: true,
            ..Block::default()
        }
        .validate(),
        Err(ValidationError("text block contains incompatible fields"))
    );
    assert_eq!(tool_call("c1", "read", r#"{"a":1}"#).validate(), Ok(()));
    assert_eq!(
        tool_call(" ", "read", r#"{"a":1}"#).validate(),
        Err(ValidationError("tool-call block is malformed"))
    );
    assert_eq!(
        Block {
            block_type: BlockType::ToolCall,
            ..Block::default()
        }
        .validate(),
        Err(ValidationError("tool-call block is malformed"))
    );
    assert_eq!(
        Block {
            block_type: BlockType::ToolResult,
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
            text: "output".into(),
            ..Block::default()
        }
        .validate(),
        Ok(())
    );
    assert_eq!(
        Block {
            block_type: BlockType::ToolResult,
            tool_call_id: "c1".into(),
            ..Block::default()
        }
        .validate(),
        Err(ValidationError("tool-result block is malformed"))
    );
    assert_eq!(
        Block {
            block_type: BlockType::Other("image".into()),
            ..Block::default()
        }
        .validate(),
        Err(ValidationError("unsupported message block type"))
    );
}

#[test]
fn image_block_is_valid_user_content_and_checks_its_bytes() {
    let image = Block::image("iVBORw0KGgo=", "image/png");
    assert_eq!(image.validate(), Ok(()));
    assert_eq!(
        Message {
            role: Role::User,
            blocks: vec![image],
            ..Message::default()
        }
        .validate(),
        Ok(())
    );
    assert_eq!(
        Block::image("/9j/", "image/png").validate(),
        Err(ValidationError("image block is malformed"))
    );
}

#[test]
fn context_metadata_validate_bounds_the_task_id() {
    assert_eq!(
        ContextMetadata {
            task_id: "t1".into()
        }
        .validate(),
        Ok(())
    );
    assert_eq!(
        ContextMetadata {
            task_id: format!("t{}", "9".repeat(63))
        }
        .validate(),
        Ok(())
    );
    for invalid in ["", "t", "x1", "t1a", &format!("t{}", "9".repeat(64))] {
        assert!(
            ContextMetadata {
                task_id: invalid.into()
            }
            .validate()
            .is_err(),
            "accepted {invalid:?}"
        );
    }
}

#[test]
fn blocks_decode_from_json_null() {
    let decoded: Message = serde_json::from_str(
        r#"{"id":"m1","role":"assistant","blocks":null,"created_at":"1970-01-01T00:00:10Z"}"#,
    )
    .expect("decode");
    assert!(decoded.blocks.is_empty());
}

#[test]
fn zero_time_encodes_as_the_pi_zero_timestamp() {
    assert_eq!(
        serde_json::to_string(&zero_time()).expect("encode"),
        "\"0001-01-01T00:00:00Z\""
    );
}
#[test]
fn invocation_projection_is_additive_and_roundtrips_without_outcome_metadata() {
    let legacy = r#"{"type":"tool_call","tool_call_id":"provider-1","tool_name":"read"}"#;
    let mut block: Block = serde_json::from_str(legacy).unwrap();
    assert!(block.invocation_id.is_empty());
    assert_eq!(block.invocation_attempt, None);
    let value = serde_json::to_value(&block).unwrap();
    assert!(value.get("invocation_id").is_none());
    assert!(value.get("invocation_attempt").is_none());
    block.invocation_id = "op_identity".into();
    block.invocation_attempt = Some(2);
    let decoded: Block = serde_json::from_str(&serde_json::to_string(&block).unwrap()).unwrap();
    assert_eq!(decoded, block);
    assert_eq!(decoded.tool_call_id, "provider-1");
    assert!(decoded.operation_metadata.is_none());
    let old: Block = serde_json::from_str(legacy).unwrap();
    assert_ne!(decoded, old);
}
