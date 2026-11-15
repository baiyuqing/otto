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

#[tokio::test]
async fn append_keeps_messages_in_order() {
    let session = MemorySession::new();
    session.append(user("one")).await.expect("append");
    session.append(user("two")).await.expect("append");
    let messages = session.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text(), "one");
    assert_eq!(messages[1].text(), "two");
}

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

fn memory_reference(id: &str, revision: Option<&str>) -> MemoryRecordRef {
    MemoryRecordRef {
        id: id.into(),
        revision: revision.map(str::to_owned),
    }
}

#[tokio::test]
async fn memory_recall_record_is_anchored_to_the_preceding_user_message() {
    let session = MemorySession::new();
    session.append(user("one")).await.expect("append");
    session
        .append_custom(
            MEMORY_RECALL_CUSTOM_TYPE,
            &encode_memory_recall(&[
                memory_reference("rec-b", None),
                memory_reference("rec-a", Some("2")),
            ]),
        )
        .await
        .expect("append recall");
    let user_message_id = session.messages()[0].id.clone();
    assert_eq!(
        session.memory_references(),
        [TurnMemoryReferences {
            user_message_id,
            records: vec![
                memory_reference("rec-a", Some("2")),
                memory_reference("rec-b", None)
            ],
        }]
    );
}

#[tokio::test]
async fn memory_recall_malformed_data_is_rejected_and_stores_nothing() {
    let session = MemorySession::new();
    session.append(user("one")).await.expect("append");
    for data in [
        "not json",
        r#"{"schemaVersion":2,"records":[]}"#,
        r#"{"schemaVersion":1,"records":[{"id":""}]}"#,
        r#"{"schemaVersion":1}"#,
    ] {
        let error = session
            .append_custom(MEMORY_RECALL_CUSTOM_TYPE, data)
            .await
            .expect_err("malformed recall accepted");
        assert!(
            matches!(error, SessionError::Persist(_)),
            "{data}: {error:?}"
        );
    }
    assert!(session.memory_references().is_empty());
}

#[tokio::test]
async fn memory_recall_without_a_user_message_is_a_sequence_error() {
    let session = MemorySession::new();
    let error = session
        .append_custom(MEMORY_RECALL_CUSTOM_TYPE, &encode_memory_recall(&[]))
        .await
        .expect_err("recall without a user message accepted");
    assert!(matches!(error, SessionError::Sequence(_)), "{error:?}");
    assert!(session.memory_references().is_empty());
}

#[tokio::test]
async fn memory_references_list_two_turns_in_order_and_survive_compaction() {
    let session = MemorySession::new();
    session.append(user("one")).await.expect("append");
    session
        .append_custom(
            MEMORY_RECALL_CUSTOM_TYPE,
            &encode_memory_recall(&[memory_reference("rec-a", None)]),
        )
        .await
        .expect("first recall");
    session.append(user("two")).await.expect("append");
    session
        .append_custom(
            MEMORY_RECALL_CUSTOM_TYPE,
            &encode_memory_recall(&[memory_reference("rec-b", Some("1"))]),
        )
        .await
        .expect("second recall");
    let user_ids: Vec<String> = session
        .messages()
        .iter()
        .map(|message| message.id.clone())
        .collect();
    let expected = vec![
        TurnMemoryReferences {
            user_message_id: user_ids[0].clone(),
            records: vec![memory_reference("rec-a", None)],
        },
        TurnMemoryReferences {
            user_message_id: user_ids[1].clone(),
            records: vec![memory_reference("rec-b", Some("1"))],
        },
    ];
    assert_eq!(session.memory_references(), expected);

    session
        .append_compaction(CompactionCheckpoint {
            summary: "summary".into(),
            first_kept_entry_id: user_ids[1].clone(),
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            ..CompactionCheckpoint::default()
        })
        .await
        .expect("checkpoint");
    assert_eq!(session.memory_references(), expected);
}

#[tokio::test]
async fn compaction_details_carry_memory_records_and_coverage() {
    let session = MemorySession::new();
    session.append(user("one")).await.expect("append");
    session.append(user("two")).await.expect("append");
    let first_kept = session.messages()[1].id.clone();
    let details = CompactionDetails {
        memory_records: vec![
            memory_reference("rec-a", Some("1")),
            memory_reference("rec-b", None),
        ],
        memory_coverage: MemoryReferenceCoverage::Complete,
        ..CompactionDetails::default()
    };
    session
        .append_compaction(CompactionCheckpoint {
            summary: "summary".into(),
            first_kept_entry_id: first_kept,
            details: details.clone(),
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            ..CompactionCheckpoint::default()
        })
        .await
        .expect("checkpoint");
    let latest = session.latest_compaction().expect("latest compaction");
    assert_eq!(latest.details.memory_records, details.memory_records);
    assert_eq!(
        latest.details.memory_coverage,
        MemoryReferenceCoverage::Complete
    );
}
