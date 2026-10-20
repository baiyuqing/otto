
use super::*;
use crate::model::{BlockType, Role};
use crate::session::codec::decode_pi_entry;
use crate::session::codec::tests::read_pi_fixture;
use crate::session::pi::{
    PiBranchSummary, PiCompaction, PiContextEdit, PiCustom, PiCustomMessage, PiModelChange,
    PiSessionInfo,
};

macro_rules! test {
        ($name:ident $body:block) => {
            #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
            #[cfg_attr(not(target_arch = "wasm32"), test)]
            fn $name() $body
        };
    }

test!(capabilities_decode_old_records_and_ignore_unknown_fields {
    let old = decode_capabilities(r#"{"tools":["b","a","a"]}"#).expect("old record");
    assert_eq!(
        old,
        Capabilities {
            tools: vec!["a".into(), "b".into()],
            ..Capabilities::default()
        }
    );
    let extra = decode_capabilities(r#"{"tools":["a"],"future":1}"#).expect("extra field");
    assert_eq!(extra.tools, ["a"]);
    assert_eq!(decode_capabilities(r#"{"tools":"a"}"#), None);
});

test!(capabilities_round_trip_all_fields {
    let capabilities = Capabilities {
        tools: vec!["a".into()],
        definitions: Some(BTreeMap::from([("a".into(), "d1".into())])),
        version: Some("0.2.1".into()),
    };
    assert_eq!(
        decode_capabilities(&encode_capabilities(&capabilities)),
        Some(capabilities)
    );
});

fn entry(entry_type: &str, id: &str, parent_id: Option<&str>) -> PiEntry {
    PiEntry::new(
        entry_type,
        id,
        parent_id.map(str::to_owned),
        "2026-08-27T12:00:00Z",
    )
}

fn user_entry(id: &str, parent_id: Option<&str>, text: &str) -> PiEntry {
    let mut built = entry("message", id, parent_id);
    built.message = Some(Box::new(PiMessage {
        role: "user".into(),
        content_text: Some(text.into()),
        timestamp: 1,
        ..PiMessage::default()
    }));
    built
}

fn pi_usage(input: i64, output: i64) -> PiUsage {
    PiUsage {
        input,
        output,
        total_tokens: input + output,
        ..PiUsage::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn assistant_entry(
    id: &str,
    parent_id: Option<&str>,
    text: &str,
    provider: &str,
    model_id: &str,
    input: i64,
    output: i64,
    stop_reason: &str,
) -> PiEntry {
    let mut built = entry("message", id, parent_id);
    built.message = Some(Box::new(PiMessage {
        role: "assistant".into(),
        provider: provider.into(),
        model: model_id.into(),
        stop_reason: stop_reason.into(),
        content_blocks: vec![PiContentBlock {
            type_name: "text".into(),
            text: text.into(),
            ..PiContentBlock::default()
        }],
        usage: Some(pi_usage(input, output)),
        timestamp: 1,
        ..PiMessage::default()
    }));
    built
}

fn context_edit_entry(
    id: &str,
    parent_id: Option<&str>,
    target_id: &str,
    replacement: Option<&str>,
) -> PiEntry {
    let mut built = entry("context_edit", id, parent_id);
    built.context_edit = Some(PiContextEdit {
        target_id: target_id.into(),
        replacement: replacement.map(|text| {
            serde_json::value::to_raw_value(&serde_json::json!({ "content": text }))
                .expect("replacement encodes")
        }),
        replacement_content: replacement
            .map(|text| serde_json::value::to_raw_value(text).expect("content encodes")),
        replacement_text: replacement.map(str::to_owned),
        ..PiContextEdit::default()
    });
    built
}

#[test]
fn tool_result_operation_metadata_round_trips_in_pi_details_and_fails_closed() {
    let metadata = ToolResultMetadata {
        operation_id: Some(crate::model::OperationId::new("op-1").expect("valid id")),
        disposition: OperationDisposition::Interrupted,
        effect_certainty: EffectCertainty::Unknown,
        stop_reason: Some(OperationStopReason::ProcessLost),
    };
    let message = Message {
        role: Role::Tool,
        blocks: vec![Block {
            block_type: BlockType::ToolResult,
            text: "unavailable".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            is_error: true,
            operation_metadata: Some(metadata.clone()),
            ..Block::default()
        }],
        created_at: DateTime::from_timestamp(1, 0).expect("in range"),
        ..Message::default()
    };
    let (entry, _) =
        model_message_to_pi_entry(&message, "e1", None, &Header::default()).expect("encode");
    let details = entry
        .message
        .as_ref()
        .expect("message")
        .details
        .as_ref()
        .expect("details")
        .get();
    assert!(details.contains(r#""operationId":"op-1""#), "{details}");
    assert!(
        details.contains(r#""effectCertainty":"unknown""#),
        "{details}"
    );
    let decoded = pi_entry_to_context_messages(&entry).expect("decode");
    assert_eq!(decoded[0].blocks[0].operation_metadata, Some(metadata));

    let mut malformed = tool_result_entry("e2", None, "call-1", "bash", "bad");
    malformed.message.as_mut().expect("message").details = Some(
        RawValue::from_string(
            r#"{"otto":{"operationId":"op-1","disposition":"interrupted"}}"#.into(),
        )
        .expect("raw details"),
    );
    assert!(pi_entry_to_context_messages(&malformed).is_err());
}

#[test]
fn a_context_message_keeps_its_usage_in_the_otto_details() {
    let usage = Usage {
        input_tokens: 40,
        output_tokens: 5,
        cached_input_tokens: 30,
    };
    let message = Message {
        id: "n1".into(),
        role: Role::Context,
        blocks: vec![Block::text("[task-notification] task t1 succeeded")],
        created_at: DateTime::from_timestamp(1, 0).expect("in range"),
        context_type: TASK_NOTIFICATION_CONTEXT_TYPE.into(),
        display: true,
        context_metadata: Some(ContextMetadata {
            task_id: "t1".into(),
        }),
        usage: Some(usage),
        ..Message::default()
    };

    let (entry, persisted) =
        model_message_to_pi_entry(&message, "e1", None, &Header::default()).expect("encode");

    assert_eq!(
        persisted.usage,
        Some(usage),
        "the replayable message keeps the notification's tokens"
    );
    let details = entry
        .custom_message
        .as_ref()
        .expect("custom_message")
        .details
        .as_ref()
        .expect("details")
        .get();
    assert!(
        details
            .contains(r#""usage":{"input_tokens":40,"output_tokens":5,"cached_input_tokens":30}"#),
        "{details}"
    );
    let decoded = pi_entry_to_context_messages(&entry).expect("decode");
    assert_eq!(decoded[0].usage, Some(usage));
}

#[test]
fn a_notification_without_usage_writes_no_usage_detail() {
    let message = Message {
        id: "n1".into(),
        role: Role::Context,
        blocks: vec![Block::text("[task-notification] task t1 canceled")],
        created_at: DateTime::from_timestamp(1, 0).expect("in range"),
        context_type: TASK_NOTIFICATION_CONTEXT_TYPE.into(),
        display: true,
        ..Message::default()
    };

    let (entry, _) =
        model_message_to_pi_entry(&message, "e1", None, &Header::default()).expect("encode");

    assert!(
        entry
            .custom_message
            .as_ref()
            .expect("custom_message")
            .details
            .is_none(),
        "no details are written for a notification without usage"
    );
}

#[test]
fn assistant_reasoning_round_trips_as_pi_thinking() {
    let message = Message {
        id: "m1".into(),
        role: Role::Assistant,
        blocks: vec![Block::reasoning("check the file"), Block::text("done")],
        created_at: DateTime::from_timestamp(1, 0).expect("in range"),
        finish_reason: Some(FinishReason::Stop),
        ..Message::default()
    };
    let header = Header {
        provider: "openai-compatible".into(),
        model: "m".into(),
        ..Header::default()
    };
    let (entry, _) = model_message_to_pi_entry(&message, "e1", None, &header).expect("encode");
    let wire = entry.message.as_ref().expect("message");
    assert_eq!(wire.content_blocks[0].type_name, "thinking");
    assert_eq!(wire.content_blocks[0].thinking, "check the file");
    let decoded = pi_entry_to_context_messages(&entry).expect("decode");
    assert_eq!(decoded[0].blocks, message.blocks);
}

#[test]
fn pi_thinking_is_rejected_when_signed_or_outside_assistant() {
    let mut signed = assistant_entry("a1", None, "x", "p", "m", 1, 1, "stop");
    signed.message.as_mut().expect("message").content_blocks = vec![PiContentBlock {
        type_name: "thinking".into(),
        thinking: "t".into(),
        thinking_signature: "sig".into(),
        ..PiContentBlock::default()
    }];
    assert!(pi_entry_to_context_messages(&signed).is_err());

    let mut user = entry("message", "u1", None);
    user.message = Some(Box::new(PiMessage {
        role: "user".into(),
        content_blocks: vec![PiContentBlock {
            type_name: "thinking".into(),
            thinking: "t".into(),
            ..PiContentBlock::default()
        }],
        timestamp: 1,
        ..PiMessage::default()
    }));
    assert!(pi_entry_to_context_messages(&user).is_err());
}

fn tool_call_entry(id: &str, parent_id: Option<&str>, calls: &[(&str, &str)]) -> PiEntry {
    let mut built = entry("message", id, parent_id);
    let blocks = calls
        .iter()
        .map(|(call_id, name)| PiContentBlock {
            type_name: "toolCall".into(),
            id: (*call_id).into(),
            name: (*name).into(),
            arguments: Some(RawValue::from_string("{}".into()).expect("valid JSON")),
            ..PiContentBlock::default()
        })
        .collect();
    built.message = Some(Box::new(PiMessage {
        role: "assistant".into(),
        provider: "openai-compatible".into(),
        model: "model".into(),
        stop_reason: "toolUse".into(),
        usage: Some(pi_usage(1, 1)),
        timestamp: 1,
        content_blocks: blocks,
        ..PiMessage::default()
    }));
    built
}

fn tool_result_entry(
    id: &str,
    parent_id: Option<&str>,
    call_id: &str,
    tool_name: &str,
    text: &str,
) -> PiEntry {
    let mut built = entry("message", id, parent_id);
    built.message = Some(Box::new(PiMessage {
        role: "toolResult".into(),
        content_text: Some(text.into()),
        tool_call_id: call_id.into(),
        tool_name: tool_name.into(),
        is_error: Some(false),
        timestamp: 1,
        ..PiMessage::default()
    }));
    built
}

fn single_tool_call(root_id: &str, assistant_id: &str) -> (PiEntry, PiEntry) {
    let root = user_entry(root_id, None, "root");
    let assistant = tool_call_entry(assistant_id, Some(root_id), &[("call-1", "read")]);
    (root, assistant)
}

fn operation_entry(id: &str, parent_id: Option<&str>, data: &str) -> PiEntry {
    let mut built = entry("custom", id, parent_id);
    built.custom = Some(PiCustom {
        custom_type: OPERATION_CUSTOM_TYPE.into(),
        data: Some(RawValue::from_string(data.into()).expect("valid JSON")),
    });
    built
}

fn runtime_entry(id: &str, parent_id: Option<&str>, metadata: &RuntimeMetadata) -> PiEntry {
    let mut built = entry("custom", id, parent_id);
    built.custom = Some(PiCustom {
        custom_type: OTTO_RUNTIME_CUSTOM_TYPE.into(),
        data: Some(to_raw_value(metadata).expect("encode runtime metadata")),
    });
    built
}

fn remote_compaction_entry(
    id: &str,
    parent_id: Option<&str>,
    checkpoint: &RemoteCompaction,
) -> PiEntry {
    let mut built = entry("custom", id, parent_id);
    built.custom = Some(PiCustom {
        custom_type: REMOTE_COMPACTION_CUSTOM_TYPE.into(),
        data: Some(
            RawValue::from_string(
                crate::session::encode_remote_compaction(checkpoint).expect("encode checkpoint"),
            )
            .expect("valid JSON"),
        ),
    });
    built
}

fn decode_entry(raw: &str) -> PiEntry {
    decode_pi_entry(raw.as_bytes()).expect("decode entry")
}

fn texts(messages: &[Message]) -> Vec<String> {
    messages.iter().map(Message::text).collect()
}

fn context_from_fixture(name: &str) -> ResolvedContext {
    let file = read_pi_fixture(name);
    let leaf = file.entries.last().expect("fixture entries").id.clone();
    let (context, warnings) = build_context(&file.entries, &leaf).expect("build context");
    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    context
}

fn assert_warning_contains(warnings: &[Warning], text: &str) {
    assert!(
        warnings
            .iter()
            .any(|warning| warning.message.contains(text)),
        "warnings = {warnings:?}, want text {text:?}"
    );
}

test!(build_context_uses_only_the_active_root_to_leaf_path {
    let file = read_pi_fixture("tree.jsonl");
    let leaf = file.entries.last().expect("entries").id.clone();
    let (context, warnings) = build_context(&file.entries, &leaf).expect("build context");
    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    assert_eq!(
        texts(&context.messages),
        [
            "root",
            "[Branch summary]\ninactive work was abandoned",
            "active branch",
        ]
    );
    for message in &context.messages {
        let text = message.text();
        assert!(
            !text.contains("inactive branch") && !text.contains("inactive custom"),
            "inactive branch leaked into context: {:?}",
            context.messages
        );
    }
});

test!(build_context_uses_the_retained_tail_checkpoint {
    let context = context_from_fixture("compacted.jsonl");
    assert_eq!(
        texts(&context.messages),
        ["[Compaction summary]\nsummary", "retained", "after"]
    );
    let checkpoint = &context.messages[0];
    assert_eq!(checkpoint.role, Role::Context);
    assert!(checkpoint.display);
    assert_eq!(checkpoint.context_type, "compaction");
    assert_eq!(context.usage.input_tokens, 12);
    assert_eq!(context.usage.output_tokens, 4);
});

test!(build_context_prefers_a_real_first_kept_entry_in_the_dual_form {
    let context = context_from_fixture("compaction-dual-form.jsonl");
    assert_eq!(
        texts(&context.messages),
        [
            "[Compaction summary]\ndual summary",
            "real retained path",
            "after dual checkpoint",
        ]
    );
    assert_eq!(context.messages[0].context_tokens_before, 140);
    for message in &context.messages {
        assert!(
            !message.id.contains("-tail-") && !message.text().contains("synthetic must lose"),
            "synthetic retained tail won over the real active path: {:?}",
            context.messages
        );
    }
});

test!(build_context_falls_back_to_the_retained_tail {
    let context = context_from_fixture("compaction-retained-tail-only.jsonl");
    assert_eq!(
        texts(&context.messages),
        [
            "[Compaction summary]\nretained-tail summary",
            "synthetic retained tail",
        ]
    );
    assert_eq!(context.messages[0].context_tokens_before, 120);
});

test!(build_context_carries_tokens_before_from_a_compaction_summary_message {
    let built = decode_entry(
        r#"{"type":"message","id":"cf000001","parentId":null,"timestamp":"2026-08-28T10:00:00Z","message":{"role":"compactionSummary","summary":"message summary","tokensBefore":321,"timestamp":1787911200000}}"#,
    );
    let leaf = built.id.clone();
    let (context, _) = build_context(&[built], &leaf).expect("build context");
    assert_eq!(context.messages.len(), 1);
    assert_eq!(context.messages[0].context_tokens_before, 321);
});

test!(build_context_uses_the_legacy_first_kept_entry_id {
    let file = read_pi_fixture("compacted.jsonl");
    let (context, warnings) = build_context(&file.entries, "c0000003").expect("build context");
    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    assert_eq!(
        texts(&context.messages),
        ["[Compaction summary]\nlegacy summary", "legacy retained"]
    );

    let mut missing = file.entries[2].clone();
    missing
        .compaction
        .as_mut()
        .expect("compaction payload")
        .first_kept_entry_id = Some("ffffffff".into());
    let leaf = missing.id.clone();
    let entries = vec![file.entries[0].clone(), file.entries[1].clone(), missing];
    let error = build_context(&entries, &leaf).expect_err("missing anchor accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
});

test!(build_context_keeps_an_image_on_the_active_branch_only {
    let root = user_entry("10000001", None, "root");
    let image = decode_entry(
        r#"{"type":"message","id":"10000002","parentId":"10000001","timestamp":"2026-08-27T12:00:02Z","message":{"role":"user","content":[{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}],"timestamp":2}}"#,
    );
    let text = user_entry("10000003", Some("10000001"), "text leaf");
    let entries = vec![root, image, text];

    let (context, _) = build_context(&entries, "10000002").expect("build image context");
    assert_eq!(context.messages.len(), 2);
    assert_eq!(context.messages[1].blocks[0].block_type, BlockType::Image);
    assert_eq!(context.messages[1].blocks[0].mime_type, "image/png");
    let (context, _) = build_context(&entries, "10000003").expect("build context");
    assert_eq!(texts(&context.messages), ["root", "text leaf"]);
});

test!(build_context_rejects_an_unsupported_message_on_the_active_branch_only {
    let bash = decode_entry(
        r#"{"type":"message","id":"20000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"bashExecution","command":"pwd","output":"/workspace","exitCode":0,"cancelled":false,"truncated":false,"timestamp":1}}"#,
    );
    let text = user_entry("20000002", None, "supported root");
    let entries = vec![bash, text];

    let error = build_context(&entries, "20000001").expect_err("bash accepted");
    assert_eq!(error.kind(), PiErrorKind::UnsupportedContent);
    build_context(&entries, "20000002").expect("build context");
});

test!(build_context_rejects_provider_required_content_on_the_active_branch_only {
    let provider_specific = decode_entry(
        r#"{"type":"message","id":"21000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"README.md"},"thoughtSignature":"provider-state"}],"api":"provider-api","provider":"provider","model":"model","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":1}}"#,
    );
    let supported = user_entry("21000002", None, "supported root");
    let entries = vec![provider_specific, supported];

    let error = build_context(&entries, "21000001").expect_err("provider content accepted");
    assert_eq!(error.kind(), PiErrorKind::UnsupportedContent);
    build_context(&entries, "21000002").expect("build context");
});

test!(build_context_applies_runtime_precedence {
    let file = read_pi_fixture("tree.jsonl");
    let leaf = file.entries.last().expect("entries").id.clone();
    let (context, _) = build_context(&file.entries, &leaf).expect("build context");
    assert_eq!(
        context.runtime,
        RuntimeMetadata {
            profile: "default".into(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
        }
    );
    assert_eq!(context.thinking_level, "high");
    assert_eq!(context.session_name, "Fixture tree");

    let root = user_entry("30000001", None, "root");
    let assistant = assistant_entry(
        "30000002",
        Some("30000001"),
        "assistant",
        "assistant-provider",
        "assistant-model",
        3,
        2,
        "stop",
    );
    let mut model_change = entry("model_change", "30000003", Some("30000002"));
    model_change.model_change = Some(PiModelChange {
        provider: "changed-provider".into(),
        model_id: "changed-model".into(),
    });
    let latest = assistant_entry(
        "30000004",
        Some("30000003"),
        "latest",
        "later-provider",
        "later-model",
        5,
        4,
        "stop",
    );

    let entries = vec![root.clone(), assistant.clone(), model_change, latest];
    let (context, _) = build_context(&entries, "30000004").expect("build context");
    assert_eq!(
        context.runtime,
        RuntimeMetadata {
            profile: String::new(),
            provider: "changed-provider".into(),
            model: "changed-model".into(),
        }
    );

    let (context, _) =
        build_context(&[root, assistant], "30000002").expect("build context");
    assert_eq!(
        context.runtime,
        RuntimeMetadata {
            profile: String::new(),
            provider: "assistant-provider".into(),
            model: "assistant-model".into(),
        }
    );
});

test!(build_context_uses_the_latest_active_otto_runtime {
    let first = runtime_entry(
        "31000001",
        None,
        &RuntimeMetadata {
            profile: "first".into(),
            provider: "openai-compatible".into(),
            model: "one".into(),
        },
    );
    let second = runtime_entry(
        "31000002",
        Some("31000001"),
        &RuntimeMetadata {
            profile: "second".into(),
            provider: "openai-compatible".into(),
            model: "two".into(),
        },
    );
    let mut change = entry("model_change", "31000003", Some("31000002"));
    change.model_change = Some(PiModelChange {
        provider: "ignored".into(),
        model_id: "ignored".into(),
    });

    let (context, _) =
        build_context(&[first, second, change], "31000003").expect("build context");
    assert_eq!(
        context.runtime,
        RuntimeMetadata {
            profile: "second".into(),
            provider: "openai-compatible".into(),
            model: "two".into(),
        }
    );
});

test!(build_context_replays_latest_remote_compaction_and_post_checkpoint_messages {
    let first = user_entry("32000001", None, "before");
    let first_checkpoint = RemoteCompaction {
        provider: "openai-compatible".into(),
        id: "first".into(),
        encrypted_content: "opaque-one".into(),
    };
    let checkpoint_one =
        remote_compaction_entry("32000002", Some("32000001"), &first_checkpoint);
    let between = user_entry("32000003", Some("32000002"), "between");
    let second_checkpoint = RemoteCompaction {
        provider: "openai-compatible".into(),
        id: "second".into(),
        encrypted_content: "opaque-two".into(),
    };
    let checkpoint_two =
        remote_compaction_entry("32000004", Some("32000003"), &second_checkpoint);
    let after = user_entry("32000005", Some("32000004"), "after");

    let (context, warnings) = build_context(
        &[first, checkpoint_one, between, checkpoint_two, after],
        "32000005",
    )
    .expect("build context");
    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    assert_eq!(context.remote_compaction, Some(second_checkpoint));
    assert_eq!(texts(&context.messages), ["before", "between", "after"]);
    assert_eq!(texts(&context.model_messages), ["before", "between", "after"]);
    assert_eq!(texts(&context.post_remote_compaction_messages), ["after"]);
});

test!(build_context_ignores_unknown_and_invalid_remote_custom_records {
    let first = user_entry("33000001", None, "before");
    let mut unknown = entry("custom", "33000002", Some("33000001"));
    unknown.custom = Some(PiCustom {
        custom_type: "future.fixture".into(),
        data: Some(RawValue::from_string("{\"future\":true}".into()).expect("JSON")),
    });
    let mut invalid = entry("custom", "33000003", Some("33000002"));
    invalid.custom = Some(PiCustom {
        custom_type: REMOTE_COMPACTION_CUSTOM_TYPE.into(),
        data: Some(RawValue::from_string("{\"schemaVersion\":2}".into()).expect("JSON")),
    });
    let after = user_entry("33000004", Some("33000003"), "after");

    let (context, warnings) =
        build_context(&[first, unknown, invalid, after], "33000004").expect("build context");
    assert_eq!(context.remote_compaction, None);
    assert_eq!(texts(&context.model_messages), ["before", "after"]);
    assert_eq!(context.post_remote_compaction_messages, context.model_messages);
    assert_warning_contains(&warnings, "ignored malformed otto.remote_compaction");
});

test!(build_context_frames_branch_and_custom_messages {
    let mut branch = entry("branch_summary", "40000001", None);
    branch.branch_summary = Some(Box::new(PiBranchSummary {
        from_id: "ffffffff".into(),
        summary: "abandoned work".into(),
        usage: Some(PiUsage {
            input: 2,
            output: 1,
            cache_read: 3,
            cache_write: 4,
            total_tokens: 10,
            ..PiUsage::default()
        }),
        ..PiBranchSummary::default()
    }));
    let mut hidden = entry("custom_message", "40000002", Some("40000001"));
    hidden.custom_message = Some(Box::new(PiCustomMessage {
        custom_type: "hidden.fixture".into(),
        content_text: Some("secret context".into()),
        display: false,
        ..PiCustomMessage::default()
    }));
    let mut visible = entry("custom_message", "40000003", Some("40000002"));
    visible.custom_message = Some(Box::new(PiCustomMessage {
        custom_type: "visible.fixture".into(),
        content_text: Some("shown context".into()),
        display: true,
        ..PiCustomMessage::default()
    }));

    let (context, warnings) =
        build_context(&[branch, hidden, visible], "40000003").expect("build context");
    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    assert_eq!(
        texts(&context.messages),
        [
            "[Branch summary]\nabandoned work",
            "[Custom context: hidden.fixture]\nsecret context",
            "[Custom context: visible.fixture]\nshown context",
        ]
    );
    assert_eq!(context.messages[0].context_type, "branch_summary");
    assert!(context.messages[0].display);
    assert!(!context.messages[1].display);
    assert!(context.messages[2].display);
    assert_eq!(
        context.usage,
        Usage {
            input_tokens: 9,
            output_tokens: 1,
            cached_input_tokens: 3,
        }
    );
});

test!(build_context_supports_custom_agent_messages {
    let built = decode_entry(
        r#"{"type":"message","id":"41000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"custom","customType":"agent.fixture","content":"injected","display":false,"timestamp":1}}"#,
    );
    let (context, _) = build_context(&[built], "41000001").expect("build context");
    assert_eq!(
        texts(&context.messages),
        ["[Custom context: agent.fixture]\ninjected"]
    );
    assert!(!context.messages[0].display);
});

test!(build_context_warns_for_an_orphan_root_and_for_multiple_roots {
    let orphan = user_entry("50000001", Some("ffffffff"), "orphan");
    let (context, warnings) = build_context(&[orphan], "50000001").expect("build context");
    assert_eq!(texts(&context.messages), ["orphan"]);
    assert_warning_contains(&warnings, "50000001");

    let first = user_entry("50000002", None, "first root");
    let second = user_entry("50000003", None, "second root");
    let (context, warnings) =
        build_context(&[first, second], "50000003").expect("build context");
    assert_eq!(texts(&context.messages), ["second root"]);
    assert_warning_contains(&warnings, "multiple roots");
});

test!(build_context_warns_and_ignores_an_unknown_active_entry {
    let root = user_entry("60000001", None, "root");
    let unknown = entry(
        "future_entry_with_untrusted_\u{1b}[31m_type",
        "60000002",
        Some("60000001"),
    );
    let leaf = user_entry("60000003", Some("60000002"), "leaf");

    let (context, warnings) =
        build_context(&[root.clone(), unknown, leaf], "60000003").expect("build context");
    assert_eq!(texts(&context.messages), ["root", "leaf"]);
    assert_warning_contains(&warnings, "60000002");
    for warning in &warnings {
        assert!(
            !warning.message.contains('\u{1b}') && warning.message.len() <= 256,
            "unsafe or unbounded warning = {:?}",
            warning.message
        );
    }

    // The two explicit roots produce exactly the multiple-root warning; the
    // inactive unknown entry must not add another.
    let inactive = entry("future_entry", "60000004", None);
    let (_, warnings) =
        build_context(&[inactive, root], "60000001").expect("build context");
    assert_eq!(warnings.len(), 1, "warnings = {warnings:?}");
});

test!(build_context_folds_only_active_operation_facts {
    let root = user_entry("60000101", None, "root");
    let active = operation_entry(
        "60000102",
        Some("60000101"),
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_active","attempt":1,"kind":"tool_call","toolCallId":"call-active","toolName":"read"}"#,
    );
    let leaf = user_entry("60000103", Some("60000102"), "leaf");
    let inactive = operation_entry(
        "60000104",
        Some("60000101"),
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_inactive","attempt":1,"kind":"tool_call","toolCallId":"call-inactive","toolName":"write"}"#,
    );
    let (context, _) = build_context(&[root, active, leaf, inactive], "60000103")
        .expect("build context");
    assert_eq!(texts(&context.messages), ["root", "leaf"]);
    assert!(context
        .operation_ledger
        .operation_for_tool_call("call-active")
        .is_some());
    assert!(context
        .operation_ledger
        .operation_for_tool_call("call-inactive")
        .is_none());
});

test!(build_context_ignores_unknown_operation_schema_but_rejects_malformed_v1 {
    let root = user_entry("60000111", None, "root");
    let future = operation_entry(
        "60000112",
        Some("60000111"),
        r#"{"schemaVersion":2,"future":true}"#,
    );
    build_context(&[root.clone(), future], "60000112").expect("future schema ignored");

    let malformed = operation_entry(
        "60000113",
        Some("60000111"),
        r#"{"schemaVersion":1,"event":"attempt"}"#,
    );
    let error = build_context(&[root, malformed], "60000113")
        .expect_err("malformed known schema accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
});

test!(build_context_warns_and_marks_operation_conflicts_corrupt {
    let root = operation_entry(
        "60000121",
        None,
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_1","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
    );
    let conflict = operation_entry(
        "60000122",
        Some("60000121"),
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_2","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
    );
    let (context, warnings) =
        build_context(&[root, conflict], "60000122").expect("tolerant fold");
    assert_warning_contains(&warnings, "different operation");
    let record = context
        .operation_ledger
        .operation_for_tool_call("call-1")
        .expect("first operation retained");
    assert!(record.corrupt);
});

test!(build_context_preserves_tool_call_result_pairing {
    let context = context_from_fixture("linear.jsonl");
    assert_eq!(context.messages.len(), 4);
    let call = &context.messages[1].blocks[1];
    let result = &context.messages[2].blocks[0];
    assert_eq!(call.block_type, BlockType::ToolCall);
    assert_eq!(result.block_type, BlockType::ToolResult);
    assert_eq!(call.tool_call_id, result.tool_call_id);
    assert_eq!(call.tool_name, result.tool_name);
});

test!(build_context_allows_sibling_tool_results_before_later_context {
    let root = user_entry("61000001", None, "root");
    let assistant = tool_call_entry(
        "61000002",
        Some("61000001"),
        &[("call-1", "read"), ("call-2", "write")],
    );
    let second_result =
        tool_result_entry("61000003", Some("61000002"), "call-2", "write", "second");
    let first_result =
        tool_result_entry("61000004", Some("61000003"), "call-1", "read", "first");
    let mut visible = entry("custom_message", "61000005", Some("61000004"));
    visible.custom_message = Some(Box::new(PiCustomMessage {
        custom_type: "visible.fixture".into(),
        content_text: Some("after tools".into()),
        display: true,
        ..PiCustomMessage::default()
    }));
    let user = user_entry("61000006", Some("61000005"), "continue");

    let entries = vec![
        root,
        assistant,
        second_result,
        first_result,
        visible,
        user,
    ];
    let (context, _) = build_context(&entries, "61000006").expect("build context");
    assert_eq!(
        texts(&context.messages),
        [
            "root",
            "",
            "",
            "",
            "[Custom context: visible.fixture]\nafter tools",
            "continue",
        ]
    );
    assert_eq!(context.messages[2].blocks[0].text, "second");
    assert_eq!(context.messages[3].blocks[0].text, "first");
});

test!(build_context_rejects_invalid_tool_result_ordering {
    let unmatched = {
        let root = user_entry("62000001", None, "root");
        let result =
            tool_result_entry("62000002", Some("62000001"), "missing", "read", "result");
        (vec![root, result], "62000002")
    };
    let duplicate_result = {
        let (root, assistant) = single_tool_call("62000003", "62000004");
        let result =
            tool_result_entry("62000005", Some("62000004"), "call-1", "read", "result");
        let duplicate = tool_result_entry(
            "62000006",
            Some("62000005"),
            "call-1",
            "read",
            "duplicate",
        );
        (vec![root, assistant, result, duplicate], "62000006")
    };
    let mismatched_name = {
        let (root, assistant) = single_tool_call("62000014", "62000015");
        let result =
            tool_result_entry("62000016", Some("62000015"), "call-1", "write", "result");
        (vec![root, assistant, result], "62000016")
    };
    let duplicate_call = {
        let root = user_entry("62000017", None, "root");
        let assistant = tool_call_entry(
            "62000018",
            Some("62000017"),
            &[("call-1", "read"), ("call-1", "read")],
        );
        (vec![root, assistant], "62000018")
    };
    let interposed_context = {
        let root = user_entry("62000007", None, "root");
        let assistant = tool_call_entry(
            "62000008",
            Some("62000007"),
            &[("call-1", "read"), ("call-2", "write")],
        );
        let result =
            tool_result_entry("62000009", Some("62000008"), "call-1", "read", "first");
        let mut interposed = entry("custom_message", "6200000a", Some("62000009"));
        interposed.custom_message = Some(Box::new(PiCustomMessage {
            custom_type: "provider-visible".into(),
            content_text: Some("not yet".into()),
            display: false,
            ..PiCustomMessage::default()
        }));
        let second =
            tool_result_entry("6200000b", Some("6200000a"), "call-2", "write", "second");
        (
            vec![root, assistant, result, interposed, second],
            "6200000b",
        )
    };
    let interposed_user = {
        let (root, assistant) = single_tool_call("6200000c", "6200000d");
        let user = user_entry("6200000e", Some("6200000d"), "too soon");
        let result =
            tool_result_entry("6200000f", Some("6200000e"), "call-1", "read", "result");
        (vec![root, assistant, user, result], "6200000f")
    };
    let interposed_assistant = {
        let (root, assistant) = single_tool_call("62000010", "62000011");
        let interposed = assistant_entry(
            "62000012",
            Some("62000011"),
            "too soon",
            "openai-compatible",
            "model",
            1,
            1,
            "stop",
        );
        let result =
            tool_result_entry("62000013", Some("62000012"), "call-1", "read", "result");
        (vec![root, assistant, interposed, result], "62000013")
    };

    let cases = [
        ("unmatched result", unmatched),
        ("duplicate result", duplicate_result),
        ("mismatched result name", mismatched_name),
        ("duplicate call", duplicate_call),
        (
            "interposed context with sibling outstanding",
            interposed_context,
        ),
        ("interposed user", interposed_user),
        ("interposed assistant", interposed_assistant),
    ];
    assert_eq!(cases.len(), 7);
    for (name, (entries, leaf)) in cases {
        let error = build_context(&entries, leaf).expect_err("case {name} accepted");
        assert_eq!(error.kind(), PiErrorKind::Invalid, "case {name}");
    }
});

test!(build_context_allows_dangling_tool_calls_only_at_the_final_boundary {
    let root = user_entry("63000001", None, "root");
    let assistant = tool_call_entry(
        "63000002",
        Some("63000001"),
        &[("call-1", "read"), ("call-2", "write")],
    );
    let (context, _) =
        build_context(&[root, assistant], "63000002").expect("build context");
    let pending = pending_tool_calls(&context.messages).expect("pending tool calls");
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].tool_call_id, "call-1");
    assert_eq!(pending[1].tool_call_id, "call-2");
});

test!(pending_tool_calls_permits_only_tool_result_blocks {
    let assistant = Message {
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            ..Block::default()
        }],
        ..Message::default()
    };
    let cases = [
        (
            "empty tool message",
            Message {
                role: Role::Tool,
                ..Message::default()
            },
        ),
        (
            "non-result block",
            Message {
                role: Role::Tool,
                blocks: vec![Block {
                    block_type: BlockType::Text,
                    tool_call_id: "call-1".into(),
                    tool_name: "read".into(),
                    text: "not a result".into(),
                    ..Block::default()
                }],
                ..Message::default()
            },
        ),
    ];
    for (name, tool_message) in cases {
        let error = pending_tool_calls(&[assistant.clone(), tool_message])
            .expect_err("case {name} accepted");
        assert_eq!(error.kind(), PiErrorKind::Invalid, "case {name}");
    }
});

test!(build_context_preserves_the_session_info_name_exactly {
    let mut built = entry("session_info", "64000001", None);
    built.session_info = Some(PiSessionInfo {
        name: Some("  exact session name\t ".into()),
    });
    let (context, _) = build_context(&[built], "64000001").expect("build context");
    assert_eq!(context.session_name, "  exact session name\t ");
});

test!(build_context_rejects_a_pending_pi_assistant {
    let pending = assistant_entry(
        "70000001",
        None,
        "partial",
        "openai-compatible",
        "model",
        0,
        0,
        "pending",
    );
    let error = build_context(&[pending], "70000001").expect_err("pending accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
});

test!(build_context_rejects_duplicate_forward_parent_and_cycle {
    let root = user_entry("80000001", None, "root");
    let duplicate = user_entry("80000001", None, "duplicate");
    let error = build_context(&[root, duplicate], "80000001").expect_err("duplicate accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);

    let forward = user_entry("80000002", Some("80000003"), "forward");
    let future = user_entry("80000003", None, "future");
    let error = build_context(&[forward, future], "80000003").expect_err("forward accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);

    let cycle = user_entry("80000004", Some("80000004"), "cycle");
    let error = build_context(&[cycle], "80000004").expect_err("cycle accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
});

test!(build_context_usage_comes_from_the_full_active_path {
    let assistant = assistant_entry(
        "90000001",
        None,
        "answer",
        "openai-compatible",
        "model",
        10,
        3,
        "stop",
    );
    let mut branch = entry("branch_summary", "90000002", Some("90000001"));
    branch.branch_summary = Some(Box::new(PiBranchSummary {
        from_id: "90000001".into(),
        summary: "summary".into(),
        usage: Some(pi_usage(4, 2)),
        ..PiBranchSummary::default()
    }));
    let mut compaction = entry("compaction", "90000003", Some("90000002"));
    compaction.compaction = Some(Box::new(PiCompaction {
        summary: "compact".into(),
        tokens_before: 10,
        retained_tail: Some(Vec::new()),
        usage: Some(pi_usage(6, 1)),
        ..PiCompaction::default()
    }));

    let (context, _) = build_context(&[assistant, branch, compaction], "90000003")
        .expect("build context");
    assert_eq!(
        context.usage,
        Usage {
            input_tokens: 20,
            output_tokens: 6,
            cached_input_tokens: 0,
        }
    );
});

test!(build_context_counts_a_task_notification_usage {
    let assistant = assistant_entry(
        "91000001",
        None,
        "answer",
        "openai-compatible",
        "model",
        10,
        3,
        "stop",
    );
    let task_usage = Usage {
        input_tokens: 40,
        output_tokens: 5,
        cached_input_tokens: 30,
    };
    let mut notification = entry("custom_message", "91000002", Some("91000001"));
    notification.custom_message = Some(Box::new(PiCustomMessage {
        custom_type: TASK_NOTIFICATION_CONTEXT_TYPE.into(),
        content_text: Some("[task-notification] task t1 succeeded".into()),
        display: true,
        details: Some(
            encode_pi_otto_details(&PiOttoDetails {
                task_id: "t1".into(),
                usage: Some(task_usage),
                ..PiOttoDetails::default()
            })
            .expect("encode details"),
        ),
        ..PiCustomMessage::default()
    }));

    let (context, _) =
        build_context(&[assistant, notification], "91000002").expect("build context");

    assert_eq!(
        context.usage,
        Usage {
            input_tokens: 50,
            output_tokens: 8,
            cached_input_tokens: 30,
        },
        "the sub-agent's tokens belong to the session totals"
    );
    assert!(context.usage_present);
    assert_eq!(
        context.messages[1].usage,
        Some(task_usage),
        "the replayed notification also carries its own usage"
    );
});

// Every fixture under `testdata/session/pi-v3` resolves to the message
// sequence below. `unknown-entry.jsonl` has no resolution expectation of
// its own (its codec test covers raw-JSON preservation only), so the roles
// and count are snapshotted here.
test!(every_fixture_resolves_to_the_expected_message_sequence {
    let cases: &[(&str, &[(Role, &str)])] = &[
        (
            "linear.jsonl",
            &[
                (Role::User, "run the check"),
                (Role::Assistant, "I will read it."),
                (Role::Tool, ""),
                (Role::Assistant, "The check passed."),
            ],
        ),
        (
            "tree.jsonl",
            &[
                (Role::User, "root"),
                (Role::Context, "[Branch summary]\ninactive work was abandoned"),
                (Role::User, "active branch"),
            ],
        ),
        (
            "compacted.jsonl",
            &[
                (Role::Context, "[Compaction summary]\nsummary"),
                (Role::User, "retained"),
                (Role::User, "after"),
            ],
        ),
        (
            "compaction-dual-form.jsonl",
            &[
                (Role::Context, "[Compaction summary]\ndual summary"),
                (Role::User, "real retained path"),
                (Role::User, "after dual checkpoint"),
            ],
        ),
        (
            "compaction-retained-tail-only.jsonl",
            &[
                (Role::Context, "[Compaction summary]\nretained-tail summary"),
                (Role::User, "synthetic retained tail"),
            ],
        ),
        ("unknown-entry.jsonl", &[]),
    ];
    assert_eq!(cases.len(), 6, "every fixture must be covered");
    for (name, want) in cases {
        let file = read_pi_fixture(name);
        let leaf = file.entries.last().expect("fixture entries").id.clone();
        let (context, _) = build_context(&file.entries, &leaf)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let got: Vec<(Role, String)> = context
            .messages
            .iter()
            .map(|message| (message.role.clone(), message.text()))
            .collect();
        let want: Vec<(Role, String)> = want
            .iter()
            .map(|(role, text)| (role.clone(), (*text).to_owned()))
            .collect();
        assert_eq!(got, want, "{name}");
    }
});
test!(build_context_repairs_a_tool_call_the_history_left_behind {
    let root = user_entry("63100001", None, "root");
    let assistant = tool_call_entry("63100002", Some("63100001"), &[("call-1", "read")]);
    let next = user_entry("63100003", Some("63100002"), "next");

    let (context, warnings) =
        build_context(&[root, assistant, next], "63100003").expect("build context");

    let roles: Vec<Role> = context.messages.iter().map(|message| message.role.clone()).collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant, Role::Tool, Role::User],
        "the stand-in result belongs next to its call"
    );
    let repaired = &context.messages[2];
    assert_eq!(repaired.blocks.len(), 1);
    assert_eq!(repaired.blocks[0].tool_call_id, "call-1");
    assert_eq!(repaired.blocks[0].tool_name, "read");
    assert!(repaired.blocks[0].is_error);
    assert_eq!(repaired.blocks[0].text, MAY_HAVE_RUN_TOOL_RESULT_TEXT);
    assert_eq!(repaired.created_at, context.messages[1].created_at);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.message.contains("call-1")),
        "the repair must be reported: {warnings:?}"
    );
});

test!(build_context_marks_only_the_first_unanswered_call_as_possibly_run {
    let root = user_entry("63120001", None, "root");
    let assistant = tool_call_entry(
        "63120002",
        Some("63120001"),
        &[("call-1", "read"), ("call-2", "bash"), ("call-3", "write")],
    );
    let result = tool_result_entry("63120003", Some("63120002"), "call-1", "read", "ok");
    let next = user_entry("63120004", Some("63120003"), "next");

    let (context, _) = build_context(&[root, assistant, result, next], "63120004")
        .expect("build context");

    let stand_ins: Vec<(&str, &str)> = context.messages[3..5]
        .iter()
        .map(|message| {
            let block = &message.blocks[0];
            (block.tool_call_id.as_str(), block.text.as_str())
        })
        .collect();
    assert_eq!(
        stand_ins,
        vec![
            ("call-2", MAY_HAVE_RUN_TOOL_RESULT_TEXT),
            ("call-3", NOT_EXECUTED_TOOL_RESULT_TEXT),
        ]
    );
});

test!(build_context_projects_dangling_repairs_from_the_operation_ledger {
    let cases = [
        (
            "terminal",
            vec![
                user_entry("63200001", None, "root"),
                tool_call_entry("63200002", Some("63200001"), &[("call-1", "read")]),
                operation_entry(
                    "63200003",
                    Some("63200002"),
                    r#"{"event":"attempt","schemaVersion":1,"operationId":"op_terminal","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
                ),
                operation_entry(
                    "63200004",
                    Some("63200003"),
                    r#"{"event":"terminal","schemaVersion":1,"operationId":"op_terminal","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read","disposition":"succeeded","effectCertainty":"completed"}"#,
                ),
                user_entry("63200005", Some("63200004"), "next"),
            ],
            "63200005",
            Some("op_terminal"),
            OperationDisposition::Succeeded,
            EffectCertainty::Completed,
            None,
            None,
        ),
        (
            "attempt",
            vec![
                user_entry("63200011", None, "root"),
                tool_call_entry("63200012", Some("63200011"), &[("call-1", "read")]),
                operation_entry(
                    "63200013",
                    Some("63200012"),
                    r#"{"event":"attempt","schemaVersion":1,"operationId":"op_attempt","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
                ),
                user_entry("63200014", Some("63200013"), "next"),
            ],
            "63200014",
            Some("op_attempt"),
            OperationDisposition::Interrupted,
            EffectCertainty::Unknown,
            Some(OperationStopReason::ProcessLost),
            None,
        ),
        (
            "legacy",
            vec![
                user_entry("63200021", None, "root"),
                tool_call_entry(
                    "63200022",
                    Some("63200021"),
                    &[("call-1", "read"), ("call-2", "write")],
                ),
                user_entry("63200023", Some("63200022"), "next"),
            ],
            "63200023",
            None,
            OperationDisposition::Interrupted,
            EffectCertainty::Unknown,
            Some(OperationStopReason::ProcessLost),
            None,
        ),
        (
            "corrupt",
            vec![
                user_entry("63200031", None, "root"),
                tool_call_entry("63200032", Some("63200031"), &[("call-1", "read")]),
                operation_entry(
                    "63200033",
                    Some("63200032"),
                    r#"{"event":"attempt","schemaVersion":1,"operationId":"op_corrupt","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
                ),
                operation_entry(
                    "63200034",
                    Some("63200033"),
                    r#"{"event":"attempt","schemaVersion":1,"operationId":"op_corrupt","attempt":3,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
                ),
                user_entry("63200035", Some("63200034"), "next"),
            ],
            "63200035",
            Some("op_corrupt"),
            OperationDisposition::Interrupted,
            EffectCertainty::Unknown,
            Some(OperationStopReason::ProcessLost),
            Some("corrupt"),
        ),
    ];

    for (
        name,
        entries,
        leaf,
        operation_id,
        disposition,
        effect_certainty,
        stop_reason,
        warning,
    ) in cases
    {
        let (context, warnings) =
            build_context(&entries, leaf).unwrap_or_else(|error| panic!("{name}: {error}"));
        let repairs: Vec<&Block> = context
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .flat_map(|message| &message.blocks)
            .collect();
        let metadata = repairs[0]
            .operation_metadata
            .as_ref()
            .unwrap_or_else(|| panic!("{name}: missing operation metadata"));
        assert_eq!(
            metadata.operation_id.as_ref().map(|id| id.as_str()),
            operation_id,
            "{name}"
        );
        assert_eq!(metadata.disposition, disposition, "{name}");
        assert_eq!(metadata.effect_certainty, effect_certainty, "{name}");
        assert_eq!(metadata.stop_reason, stop_reason, "{name}");
        if name == "legacy" {
            assert_eq!(repairs.len(), 2);
            let second = repairs[1].operation_metadata.as_ref().expect("metadata");
            assert_eq!(second.operation_id, None);
            assert_eq!(second.effect_certainty, EffectCertainty::NotStarted);
        }
        if let Some(warning) = warning {
            assert_warning_contains(&warnings, warning);
        }
    }
});

test!(build_context_correlates_dangling_repairs_across_branch_and_compaction_views {
    let root = user_entry("63300001", None, "root");
    let assistant = tool_call_entry("63300002", Some("63300001"), &[("branch-call", "read")]);
    let attempt = operation_entry(
        "63300003",
        Some("63300002"),
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_branch","attempt":1,"kind":"tool_call","toolCallId":"branch-call","toolName":"read"}"#,
    );
    let mut branch = entry("branch_summary", "63300004", Some("63300003"));
    branch.branch_summary = Some(Box::new(PiBranchSummary {
        from_id: "63300001".into(),
        summary: "branch moved on".into(),
        ..PiBranchSummary::default()
    }));
    let inactive = operation_entry(
        "63300005",
        Some("63300001"),
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_inactive","attempt":1,"kind":"tool_call","toolCallId":"branch-call","toolName":"read"}"#,
    );
    let (branch_context, _) = build_context(
        &[root, assistant, attempt, branch, inactive],
        "63300004",
    )
    .expect("branch context");
    let branch_metadata = branch_context.messages[2].blocks[0]
        .operation_metadata
        .as_ref()
        .expect("branch repair metadata");
    assert_eq!(
        branch_metadata.operation_id.as_ref().map(|id| id.as_str()),
        Some("op_branch")
    );

    let attempt = operation_entry(
        "63300011",
        None,
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_compacted","attempt":1,"kind":"tool_call","toolCallId":"retained-call","toolName":"bash"}"#,
    );
    let retained = tool_call_entry("ignored01", None, &[("retained-call", "bash")])
        .message
        .expect("retained message");
    let mut compaction = entry("compaction", "63300012", Some("63300011"));
    compaction.compaction = Some(Box::new(PiCompaction {
        summary: "summary".into(),
        retained_tail: Some(vec![*retained]),
        ..PiCompaction::default()
    }));
    let next = user_entry("63300013", Some("63300012"), "next");
    let (compacted, _) = build_context(&[attempt, compaction, next], "63300013")
        .expect("compacted context");
    let repair = compacted
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("retained-tail repair");
    let metadata = repair.blocks[0]
        .operation_metadata
        .as_ref()
        .expect("compacted repair metadata");
    assert_eq!(
        metadata.operation_id.as_ref().map(|id| id.as_str()),
        Some("op_compacted")
    );
    assert_eq!(metadata.effect_certainty, EffectCertainty::Unknown);
});

test!(build_context_leaves_a_trailing_tool_call_for_the_durable_repair {
    let (root, assistant) = single_tool_call("63110001", "63110002");

    let (context, warnings) = build_context(&[root, assistant], "63110002").expect("build context");

    assert_eq!(context.messages.len(), 2, "{:?}", context.messages);
    assert!(warnings.is_empty(), "{warnings:?}");
    let pending = pending_tool_calls(&context.messages).expect("pending tool calls");
    assert_eq!(pending.len(), 1);
});

test!(build_context_still_rejects_a_tool_result_without_a_call {
    let root = user_entry("63120001", None, "root");
    let orphan = tool_result_entry("63120002", Some("63120001"), "call-1", "read", "done");

    let error = build_context(&[root, orphan], "63120002").expect_err("must reject");

    assert_eq!(error.kind(), PiErrorKind::Invalid);
});

test!(context_edit_omits_replaces_and_keeps_raw_history {
    let root = user_entry("ce000001", None, "original");
    let replace = context_edit_entry("ce000002", Some("ce000001"), "ce000001", Some("new"));
    let omit = context_edit_entry("ce000003", Some("ce000002"), "ce000001", None);

    let (replaced, _) =
        build_context(&[root.clone(), replace.clone()], "ce000002").expect("replace");
    assert_eq!(replaced.messages.len(), 1);
    assert_eq!(replaced.messages[0].text(), "original");
    assert_eq!(replaced.model_messages[0].text(), "new");
    assert_eq!(root.message.as_ref().unwrap().content_text.as_deref(), Some("original"));

    let (omitted, _) =
        build_context(&[root, replace, omit.clone()], "ce000003").expect("omit");
    assert!(omitted.model_messages.is_empty());
    assert_eq!(omitted.messages[0].text(), "original");
    assert!(omit.context_edit.as_ref().unwrap().replacement.is_none());
});

test!(context_edit_is_latest_wins_and_branch_relative {
    let root = user_entry("ce100001", None, "original");
    let first = context_edit_entry("ce100002", Some("ce100001"), "ce100001", Some("first"));
    let second = context_edit_entry("ce100003", Some("ce100002"), "ce100001", Some("second"));
    let branch = user_entry("ce100004", Some("ce100001"), "branch");

    let (latest, _) = build_context(
        &[root.clone(), first, second],
        "ce100003",
    )
    .expect("latest edit");
    assert_eq!(latest.model_messages[0].text(), "second");

    let (other_branch, _) = build_context(&[root, branch], "ce100004").expect("branch");
    assert_eq!(other_branch.model_messages[0].text(), "original");
    assert_eq!(other_branch.model_messages[1].text(), "branch");
});

test!(context_edit_rejects_a_noneditable_or_inactive_target {
    let root = user_entry("ce200001", None, "root");
    let mut label = entry("label", "ce200002", Some("ce200001"));
    label.label = Some(crate::session::PiLabel {
        target_id: "ce200001".into(),
        label: Some("x".into()),
    });
    let invalid = context_edit_entry("ce200003", Some("ce200002"), "ce200002", None);
    let (_, warnings) = build_context(&[root.clone(), label, invalid], "ce200003")
        .expect("label target is skipped");
    assert_warning_contains(&warnings, "unsupported target");

    let inactive = user_entry("ce200004", Some("ce200001"), "inactive");
    let active = user_entry("ce200005", Some("ce200001"), "active");
    let invalid = context_edit_entry("ce200006", Some("ce200005"), "ce200004", None);
    let (_, warnings) = build_context(&[root, inactive, active, invalid], "ce200006")
        .expect("inactive target is skipped");
    assert_warning_contains(&warnings, "unsupported target");
});

test!(context_edit_skips_tool_call_and_result_targets_without_affecting_recovery {
    let root = user_entry("ce400001", None, "root");
    let assistant = tool_call_entry("ce400002", Some("ce400001"), &[("call-1", "read")]);
    let result = tool_result_entry(
        "ce400003",
        Some("ce400002"),
        "call-1",
        "read",
        "done",
    );
    let edit = context_edit_entry("ce400004", Some("ce400003"), "ce400003", None);
    let (context, warnings) =
        build_context(&[root, assistant, result, edit], "ce400004").expect("build context");
    assert_eq!(context.messages.len(), 3);
    assert_eq!(context.model_messages.len(), 3);
    assert_eq!(context.model_messages[2].blocks[0].text, "done");
    assert!(warnings.iter().any(|warning| warning.message.contains("unsupported target")));
});

test!(context_edit_skips_a_target_removed_by_compaction {
    let root = user_entry("ce500001", None, "old");
    let edit = context_edit_entry("ce500002", Some("ce500001"), "ce500001", None);
    let mut compaction = entry("compaction", "ce500003", Some("ce500002"));
    compaction.compaction = Some(Box::new(PiCompaction {
        summary: "summary".into(),
        first_kept_entry_id: None,
        tokens_before: 1,
        retained_tail: Some(Vec::new()),
        ..PiCompaction::default()
    }));
    let (context, warnings) =
        build_context(&[root, edit, compaction], "ce500003").expect("build context");
    assert_eq!(context.model_messages.len(), 1);
    assert_eq!(context.model_messages[0].context_type, COMPACTION_CONTEXT_TYPE);
    assert!(warnings
        .iter()
        .any(|warning| warning.message.contains("not retained after compaction")));
});

test!(context_edit_uses_pi_replacement_shape_and_round_trips {
    let raw = br#"{"type":"context_edit","id":"ce300001","parentId":null,"timestamp":"2026-08-27T12:00:00Z","targetId":"ce300000","replacement":{"content":"replacement"}}"#;
    let entry = decode_pi_entry(raw).expect("decode");
    let edit = entry.context_edit.as_ref().expect("payload");
    assert_eq!(edit.target_id, "ce300000");
    assert_eq!(edit.replacement_text.as_deref(), Some("replacement"));
    let encoded = crate::session::encode_pi_record(crate::session::PiRecord::Entry(&entry))
        .expect("encode raw");
    assert_eq!(encoded, raw);

    let mut typed = entry;
    typed.raw.clear();
    let encoded = crate::session::encode_pi_record(crate::session::PiRecord::Entry(&typed))
        .expect("encode typed");
    let round_trip = decode_pi_entry(&encoded).expect("decode typed record");
    assert_eq!(
        round_trip
            .context_edit
            .as_ref()
            .expect("typed payload")
            .replacement_text
            .as_deref(),
        Some("replacement")
    );
});
