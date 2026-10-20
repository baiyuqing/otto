
use super::*;
use crate::model::{Block, BlockType, FinishReason, Message, Role};
use serde_json::value::RawValue;

fn tool_call(id: &str, name: &str) -> Block {
    Block {
        block_type: BlockType::ToolCall,
        tool_call_id: id.into(),
        tool_name: name.into(),
        arguments: Some(RawValue::from_string("{}".into()).expect("valid JSON")),
        ..Block::default()
    }
}

fn assistant_with_call(id: &str, name: &str) -> Message {
    Message {
        role: Role::Assistant,
        finish_reason: Some(FinishReason::ToolCalls),
        blocks: vec![tool_call(id, name)],
        ..Message::default()
    }
}

fn tool_result(id: &str, name: &str) -> Message {
    Message {
        role: Role::Tool,
        blocks: vec![Block {
            block_type: BlockType::ToolResult,
            tool_call_id: id.into(),
            tool_name: name.into(),
            text: "output".into(),
            ..Block::default()
        }],
        ..Message::default()
    }
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn append_keeps_messages_in_order() {
    let session = MemorySession::new();
    session.append(user("one")).await.expect("append");
    session.append(user("two")).await.expect("append");
    let messages = session.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text(), "one");
    assert_eq!(messages[1].text(), "two");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn append_rejects_an_invalid_message_and_stores_nothing() {
    let session = MemorySession::new();
    let error = session
        .append(Message {
            role: Role::User,
            ..Message::default()
        })
        .await
        .expect_err("invalid message accepted");
    assert_eq!(
        error,
        SessionError::Invalid(ValidationError("user message content is required"))
    );
    assert!(session.messages().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn tool_result_must_resolve_a_call_from_the_preceding_assistant_message() {
    let session = MemorySession::new();
    session
        .append(assistant_with_call("c1", "read"))
        .await
        .expect("append");
    session
        .append(tool_result("c1", "read"))
        .await
        .expect("append");
    assert_eq!(session.messages().len(), 2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn operation_custom_is_stored_without_interrupting_tool_sequence() {
    let session = MemorySession::new();
    session
        .append(assistant_with_call("c1", "read"))
        .await
        .expect("append call");
    let json = r#"{"event":"attempt","schemaVersion":1,"operationId":"op_1","attempt":1,"kind":"tool_call","toolCallId":"c1","toolName":"read"}"#;
    session
        .append_custom(OPERATION_CUSTOM_TYPE, json)
        .await
        .expect("append fact");
    session
        .append(tool_result("c1", "read"))
        .await
        .expect("custom must not interrupt sequence");
    assert_eq!(session.messages().len(), 2);
    assert_eq!(session.operation_history().len(), 1);
    assert!(
        session
            .operation_ledger()
            .operation_for_tool_call("c1")
            .is_some()
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_local_compaction_clears_an_older_remote_checkpoint() {
    let session = MemorySession::new();
    session.append(user("before")).await.expect("append");
    session
        .append_remote_compaction(RemoteCompaction {
            provider: "chatgpt".into(),
            id: "cmp_1".into(),
            encrypted_content: "opaque".into(),
        })
        .await
        .expect("remote checkpoint");
    session.append(user("after")).await.expect("append");
    let first_kept = session.messages()[1].id.clone();

    session
        .append_compaction(CompactionCheckpoint {
            summary: "local summary".into(),
            first_kept_entry_id: first_kept,
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            ..CompactionCheckpoint::default()
        })
        .await
        .expect("local checkpoint");

    assert_eq!(session.remote_compaction(), None);
    assert_eq!(
        session.post_remote_compaction_messages(),
        session.model_messages()
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn operation_custom_known_malformed_is_rejected_atomically() {
    let session = MemorySession::new();
    assert!(
        session
            .append_custom(
                OPERATION_CUSTOM_TYPE,
                r#"{"schemaVersion":1,"event":"attempt"}"#,
            )
            .await
            .is_err()
    );
    assert!(session.operation_history().is_empty());
    session
        .append_custom(
            OPERATION_CUSTOM_TYPE,
            r#"{"schemaVersion":2,"future":true}"#,
        )
        .await
        .expect("future schema ignored");
    assert!(session.operation_history().is_empty());
    session
        .append_custom("other.custom", "not json")
        .await
        .expect("other custom unchanged");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn append_rejects_out_of_sequence_tool_traffic() {
    let cases: &[(&str, Vec<Message>, SessionError)] = &[
        (
            "result without a call",
            vec![tool_result("c1", "read")],
            SessionError::Sequence("tool result has no pending call"),
        ),
        (
            "result names a different tool",
            vec![
                assistant_with_call("c1", "read"),
                tool_result("c1", "write"),
            ],
            SessionError::Sequence("tool result name does not match pending call"),
        ),
        (
            "assistant before the result",
            vec![
                assistant_with_call("c1", "read"),
                assistant_with_call("c2", "read"),
            ],
            SessionError::Sequence("unresolved tool calls must be followed by tool results"),
        ),
        (
            "user message before the result",
            vec![assistant_with_call("c1", "read"), user("hi")],
            SessionError::Sequence("unresolved tool calls must be followed by tool results"),
        ),
        (
            "duplicate tool-call id",
            vec![
                assistant_with_call("c1", "read"),
                tool_result("c1", "read"),
                assistant_with_call("c1", "read"),
            ],
            SessionError::Sequence("duplicate tool-call id"),
        ),
    ];
    for (name, messages, want) in cases {
        let session = MemorySession::new();
        let last = messages.len() - 1;
        for message in &messages[..last] {
            session.append(message.clone()).await.expect("setup append");
        }
        let error = session
            .append(messages[last].clone())
            .await
            .expect_err("case {name} accepted");
        assert_eq!(&error, want, "case {name}");
        assert_eq!(
            session.messages().len(),
            last,
            "case {name} stored the rejected message"
        );
    }
}
