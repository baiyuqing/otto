//! The transcript store contract and an in-memory implementation.
//!
//! Port of the append-only `Session` contract in `internal/session`. Phase 0
//! carries the two operations the agent loop needs; the Pi v3 JSONL store
//! arrives in phase 2.
//!
//! Ownership: `append` takes the message by value and the session owns it
//! afterwards. `messages` returns an independent copy of the transcript.
//!
//! Concurrency: implementations take `&self` and must be safe for concurrent
//! use. [`MemorySession`] is guarded by a mutex.
//!
//! Errors: history is append-only, so a rejected message is not stored and the
//! call returns a [`SessionError`]. A rejection never leaves partial state.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::model::{BlockType, Message, Role, ValidationError};

/// Why a message could not be appended.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// The message itself is malformed. See [`Message::validate`].
    #[error(transparent)]
    Invalid(#[from] ValidationError),
    /// The message is well formed but breaks the tool-call ordering rule.
    #[error("{0}")]
    Sequence(&'static str),
}

/// An append-only transcript.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait Session {
    /// A copy of the transcript in append order.
    fn messages(&self) -> Vec<Message>;

    /// Appends one message after validating it and the tool-call ordering
    /// rule. Nothing is stored when the call returns an error.
    async fn append(&self, message: Message) -> Result<(), SessionError>;
}

/// Tool calls from the most recent assistant message that have no result yet.
#[derive(Debug, Default)]
struct State {
    messages: Vec<Message>,
    /// Tool-call id to tool name, for calls awaiting a result.
    pending: HashMap<String, String>,
    /// Every tool-call id seen in this session, to reject reuse.
    seen_call_ids: HashSet<String>,
}

impl State {
    /// The ordering rule ported from `pendingToolCalls` in
    /// `internal/session/store.go`, applied incrementally: a tool-result
    /// message must resolve a call from the immediately preceding assistant
    /// message, and no other message may come between the two.
    fn check_sequence(&self, message: &Message) -> Result<(), SessionError> {
        const UNRESOLVED: &str = "unresolved tool calls must be followed by tool results";
        match message.role {
            Role::Assistant => {
                if !self.pending.is_empty() {
                    return Err(SessionError::Sequence(UNRESOLVED));
                }
                let mut seen_here = HashSet::new();
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolCall {
                        continue;
                    }
                    if self.seen_call_ids.contains(&block.tool_call_id)
                        || !seen_here.insert(&block.tool_call_id)
                    {
                        return Err(SessionError::Sequence("duplicate tool-call id"));
                    }
                }
                Ok(())
            }
            Role::Tool => {
                let mut resolved = HashSet::new();
                for block in &message.blocks {
                    let Some(name) = self.pending.get(&block.tool_call_id) else {
                        return Err(SessionError::Sequence("tool result has no pending call"));
                    };
                    if !resolved.insert(&block.tool_call_id) {
                        return Err(SessionError::Sequence("duplicate tool-call id"));
                    }
                    if *name != block.tool_name {
                        return Err(SessionError::Sequence(
                            "tool result name does not match pending call",
                        ));
                    }
                }
                Ok(())
            }
            _ => {
                if self.pending.is_empty() {
                    Ok(())
                } else {
                    Err(SessionError::Sequence(UNRESOLVED))
                }
            }
        }
    }

    fn record(&mut self, message: Message) {
        match message.role {
            Role::Assistant => {
                for block in &message.blocks {
                    if block.block_type == BlockType::ToolCall {
                        self.seen_call_ids.insert(block.tool_call_id.clone());
                        self.pending
                            .insert(block.tool_call_id.clone(), block.tool_name.clone());
                    }
                }
            }
            Role::Tool => {
                for block in &message.blocks {
                    self.pending.remove(&block.tool_call_id);
                }
            }
            _ => {}
        }
        self.messages.push(message);
    }
}

/// An in-memory transcript. Used by tests and by sub-agents that must not
/// write to disk.
#[derive(Debug, Default)]
pub struct MemorySession {
    state: Mutex<State>,
}

impl MemorySession {
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Session for MemorySession {
    fn messages(&self) -> Vec<Message> {
        self.state.lock().expect("session mutex").messages.clone()
    }

    async fn append(&self, message: Message) -> Result<(), SessionError> {
        message.validate()?;
        let mut state = self.state.lock().expect("session mutex");
        state.check_sequence(&message)?;
        state.record(message);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
}
