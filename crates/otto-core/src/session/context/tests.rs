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
            #[test]
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
        ..Default::default()
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

mod cache_usage;
mod context_edits;
mod history_messages;
mod memory_references;
mod operations_pairing;
mod path_checkpoints;
mod usage_repair;
