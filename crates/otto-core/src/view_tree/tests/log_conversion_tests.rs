//! Log conversion rules.

use serde_json::value::RawValue;

use crate::model::{Block, BlockType, Message, Role};
use crate::session::HistoryMessage;
use crate::view_tree::*;

fn history(role: Role, blocks: Vec<Block>) -> HistoryMessage {
    HistoryMessage {
        entry_id: "e0000001".into(),
        timestamp: "2026-10-27T01:02:03Z".into(),
        message: Message {
            role,
            blocks,
            ..Message::default()
        },
    }
}

fn text_block(text: &str) -> Block {
    Block {
        block_type: BlockType::Text,
        text: text.into(),
        ..Block::default()
    }
}

#[test]
fn user_assistant_and_tool_messages_become_log_messages() {
    let call = Block {
        block_type: BlockType::ToolCall,
        tool_call_id: "c1".into(),
        tool_name: "read".into(),
        arguments: Some(RawValue::from_string(r#"{"path":"a.rs"}"#.into()).unwrap()),
        ..Block::default()
    };
    let reasoning = Block {
        block_type: BlockType::Reasoning,
        text: "secret thoughts".into(),
        ..Block::default()
    };
    let image = Block {
        block_type: BlockType::Image,
        data: "AAAA".into(),
        mime_type: "image/png".into(),
        ..Block::default()
    };
    let result = Block {
        block_type: BlockType::ToolResult,
        tool_call_id: "c1".into(),
        tool_name: "read".into(),
        text: "fn main() {}".into(),
        is_error: true,
        ..Block::default()
    };
    let log = log_from_history(&[
        history(Role::User, vec![text_block("look"), image]),
        history(
            Role::Assistant,
            vec![reasoning, text_block("reading"), call],
        ),
        history(Role::Tool, vec![result]),
        history(Role::Context, vec![text_block("[Compaction summary]")]),
    ]);
    let rows: Vec<(u64, LogKind, &str)> = log
        .iter()
        .map(|m| (m.id, m.kind, m.text.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            (0, LogKind::User, "look\n[image]"),
            (1, LogKind::Assistant, "reading"),
            (2, LogKind::ToolCall, r#"read {"path":"a.rs"}"#),
            (3, LogKind::ToolResult, "read (error)\nfn main() {}"),
        ]
    );
    assert!(
        log.iter()
            .all(|m| m.entry_id == "e0000001" && m.timestamp == "2026-10-27T01:02:03Z")
    );
    assert!(!log.iter().any(|m| m.text.contains("secret")));
}

#[test]
fn a_long_tool_result_keeps_both_ends_and_names_the_omitted_count() {
    let content = format!(
        "{}{}{}",
        "a".repeat(15_000),
        "b".repeat(10_000),
        "c".repeat(15_000)
    );
    let result = Block {
        block_type: BlockType::ToolResult,
        tool_name: "bash".into(),
        text: content,
        ..Block::default()
    };
    let log = log_from_history(&[history(Role::Tool, vec![result])]);
    assert_eq!(log.len(), 1, "a tool result is clipped, never split");
    let text = &log[0].text;
    assert!(text.starts_with(&format!(
        "bash\n{}\n[... 10000 characters omitted ...]\n",
        "a".repeat(15_000)
    )));
    assert!(text.ends_with(&"c".repeat(15_000)));
    assert_eq!(text.matches('b').count(), 1, "only the b of bash remains");
}

#[test]
fn text_over_30000_characters_is_split_into_consecutive_messages() {
    let long: String = "é".repeat(65_000);
    let log = log_from_history(&[history(Role::Assistant, vec![text_block(&long)])]);
    let sizes: Vec<usize> = log.iter().map(|m| m.text.chars().count()).collect();
    assert_eq!(sizes, [30_000, 30_000, 5_000]);
    assert_eq!(log.iter().map(|m| m.id).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(
        log.iter().map(|m| m.text.as_str()).collect::<String>(),
        long
    );
}
