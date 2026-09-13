//! The transcript prefix a `context: inherit` child receives. Port of
//! `internal/subagent/inherit.go`.

use otto_core::model::{Message, Role};

/// The prefix of `messages` a child with `context: "inherit"` receives:
/// everything before the last assistant message, which is the one carrying the
/// pending `agent` tool calls. Tool results appended after it for sibling
/// calls are cut too. `None` when there is no assistant message at all, which
/// Go signals by returning a nil slice; an empty `Vec` means "an assistant
/// message was found and nothing precedes it".
pub fn inherit_snapshot(messages: &[Message]) -> Option<Vec<Message>> {
    let last_assistant = messages
        .iter()
        .rposition(|message| message.role == Role::Assistant)?;
    Some(messages[..last_assistant].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::model::{Block, BlockType};

    fn message(id: &str, role: Role, blocks: Vec<Block>) -> Message {
        Message {
            id: id.to_string(),
            role,
            blocks,
            ..Message::default()
        }
    }

    fn user(id: &str, text: &str) -> Message {
        message(
            id,
            Role::User,
            vec![Block {
                block_type: BlockType::Text,
                text: text.to_string(),
                ..Block::default()
            }],
        )
    }

    fn assistant_text(id: &str, text: &str) -> Message {
        message(
            id,
            Role::Assistant,
            vec![Block {
                block_type: BlockType::Text,
                text: text.to_string(),
                ..Block::default()
            }],
        )
    }

    fn assistant_tool_call(id: &str, tool_name: &str) -> Message {
        message(
            id,
            Role::Assistant,
            vec![Block {
                block_type: BlockType::ToolCall,
                tool_name: tool_name.to_string(),
                tool_call_id: "call-1".to_string(),
                ..Block::default()
            }],
        )
    }

    fn tool_result(id: &str) -> Message {
        message(
            id,
            Role::Tool,
            vec![Block {
                block_type: BlockType::ToolResult,
                tool_call_id: "call-0".to_string(),
                ..Block::default()
            }],
        )
    }

    fn ids(messages: &[Message]) -> Vec<&str> {
        messages.iter().map(|message| message.id.as_str()).collect()
    }

    /// Go's `TestInheritSnapshot`. The copy `TestInheritSnapshotReturnsACopy`
    /// checks is unconditional here: the slice is cloned into a new `Vec`.
    #[test]
    fn the_snapshot_ends_before_the_last_assistant_message() {
        assert!(inherit_snapshot(&[]).is_none());
        assert!(inherit_snapshot(&[user("u1", "hello")]).is_none());
        assert_eq!(
            inherit_snapshot(&[assistant_tool_call("a1", "agent")]),
            Some(Vec::new())
        );

        let siblings = [
            user("u1", "hello"),
            assistant_tool_call("a1", "agent"),
            tool_result("tr1"),
        ];
        assert_eq!(
            ids(&inherit_snapshot(&siblings).expect("an assistant message")),
            ["u1"]
        );

        let exchanges = [
            user("u1", "first"),
            assistant_text("a1", "reply one"),
            user("u2", "second"),
            assistant_tool_call("a2", "agent"),
        ];
        assert_eq!(
            ids(&inherit_snapshot(&exchanges).expect("an assistant message")),
            ["u1", "a1", "u2"]
        );
    }
}
