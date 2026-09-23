//! The transcript store contract and an in-memory implementation.
//!
//! Phase 0 carries the two operations the agent loop needs; the Pi v3 JSONL
//! store arrives in phase 2.
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
use std::fmt;
use std::sync::Mutex;

use crate::model::{BlockType, Message, Role, ValidationError};

pub mod codec;
pub mod compaction;
pub mod context;
pub mod pi;
pub mod types;

pub use codec::{PiRecord, decode_pi_entry, decode_pi_file, decode_pi_header, encode_pi_record};
pub use context::{
    BRANCH_CONTEXT_TYPE, COMPACTION_CONTEXT_TYPE, ContextEntryIndex, KITE_RUNTIME_CUSTOM_TYPE,
    ResolvedContext, active_context_path, build_context, index_context_entries, is_pi_entry_id,
    new_context_message,
};
pub use pi::{
    MAX_SESSION_ENTRY_BYTES, MAX_SESSION_FILE_BYTES, PI_SESSION_VERSION, PiBranchSummary,
    PiCompaction, PiContentBlock, PiCost, PiCustom, PiCustomMessage, PiEntry, PiFile, PiHeader,
    PiKiteDetails, PiLabel, PiMessage, PiModelChange, PiSessionInfo, PiThinkingLevelChange,
    PiUsage, decode_pi_kite_details, encode_pi_kite_details,
};
pub use types::{
    CURRENT_VERSION, CompactionCheckpoint, CompactionDetails, CompactionMetadata, Header,
    ListResult, RuntimeMetadata, SessionInfo, Snapshot, Warning,
};

/// Which error class a [`PiError`] corresponds to.
///
/// Callers branch on the kind rather than on the message: the kind survives
/// every `context` prefix, so the classification of a failure deep inside a
/// file survives being reported as "session line 7: ...".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiErrorKind {
    /// The record is not a Pi v3 session, or carries an unsupported version.
    UnsupportedFormat,
    /// The record is a Pi session but breaks a validation rule.
    Invalid,
    /// One record exceeds [`MAX_SESSION_ENTRY_BYTES`].
    EntryTooLarge,
    /// The file exceeds [`MAX_SESSION_FILE_BYTES`].
    FileTooLarge,
    /// The record is valid Pi but carries content Kite cannot represent.
    UnsupportedContent,
    /// The session has been closed.
    Closed,
    /// A durable write failed. The store is poisoned and refuses every later
    /// write.
    FatalPersistence,
    /// Anything with no dedicated kind, such as an encoding failure.
    Other,
}

impl PiErrorKind {
    /// The error text this kind prefixes its message with.
    fn text(self) -> &'static str {
        match self {
            Self::UnsupportedFormat => "unsupported session format",
            Self::Invalid => "invalid session",
            Self::EntryTooLarge => "session entry too large",
            Self::FileTooLarge => "session file too large",
            Self::UnsupportedContent => "unsupported session content",
            Self::Closed => "session is closed",
            Self::FatalPersistence => "fatal session persistence failure",
            Self::Other => "",
        }
    }
}

/// A session codec or store failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PiError {
    kind: PiErrorKind,
    message: String,
}

impl PiError {
    /// Builds `"<sentinel>: <detail>"` for one of the sentinel kinds.
    pub fn new(kind: PiErrorKind, detail: impl fmt::Display) -> Self {
        Self {
            message: format!("{}: {detail}", kind.text()),
            kind,
        }
    }

    /// Shorthand for [`PiErrorKind::Invalid`], by far the most common kind.
    pub fn invalid(detail: impl fmt::Display) -> Self {
        Self::new(PiErrorKind::Invalid, detail)
    }

    /// An error with no dedicated kind; the message is used verbatim.
    pub fn other(message: impl fmt::Display) -> Self {
        Self {
            kind: PiErrorKind::Other,
            message: message.to_string(),
        }
    }

    /// The closed-session error. It carries no detail.
    pub fn closed() -> Self {
        Self {
            kind: PiErrorKind::Closed,
            message: PiErrorKind::Closed.text().to_owned(),
        }
    }

    /// Wraps the cause of a failed durable write. Once a store returns this it
    /// returns the same error from every later write.
    pub fn fatal(cause: impl fmt::Display) -> Self {
        Self::new(PiErrorKind::FatalPersistence, cause)
    }

    /// The size-limit message shared by the two too-large kinds.
    pub fn size(kind: PiErrorKind, limit: usize) -> Self {
        Self::new(kind, format!("maximum is {limit} bytes"))
    }

    /// Prefixes the message, keeping the kind.
    #[must_use]
    pub fn context(self, prefix: impl fmt::Display) -> Self {
        Self {
            kind: self.kind,
            message: format!("{prefix}: {}", self.message),
        }
    }

    /// The sentinel this error corresponds to.
    pub fn kind(&self) -> PiErrorKind {
        self.kind
    }
}

/// Why a message could not be appended.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// The message itself is malformed. See [`Message::validate`].
    #[error(transparent)]
    Invalid(#[from] ValidationError),
    /// The message is well formed but breaks the tool-call ordering rule.
    #[error("{0}")]
    Sequence(&'static str),
    /// The message is well formed but could not be persisted. Only a
    /// file-backed session produces this; [`MemorySession`] never does.
    #[error("{0}")]
    Persist(String),
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

    /// The compaction checkpoint currently in force, or `None` when the session
    /// has never been compacted.
    fn latest_compaction(&self) -> Option<CompactionMetadata>;

    /// Records a compaction checkpoint: the transcript becomes the summary
    /// context message followed by the messages from
    /// `checkpoint.first_kept_entry_id` onward. Nothing changes when the call
    /// returns an error.
    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError>;
}

/// Tool calls from the most recent assistant message that have no result yet.
#[derive(Debug, Default)]
struct State {
    messages: Vec<Message>,
    /// Tool-call id to tool name, for calls awaiting a result.
    pending: HashMap<String, String>,
    /// Every tool-call id seen in this session, to reject reuse.
    seen_call_ids: HashSet<String>,
    /// Every message id seen in this session, to reject reuse.
    seen_ids: HashSet<String>,
    latest_compaction: Option<CompactionMetadata>,
    /// Numbers the synthetic checkpoint ids and the ids generated for messages
    /// appended without one. An in-memory session has no file to share ids
    /// with, so a counter is enough and keeps tests deterministic.
    checkpoint_counter: u64,
}

impl State {
    /// The ordering rule, applied incrementally: a tool-result message must
    /// resolve a call from the immediately preceding assistant message, and no
    /// other message may come between the two.
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

    /// Replays `messages` through the sequence rule from an empty state,
    /// validating a whole candidate slice.
    fn replay(messages: &[Message]) -> Result<State, SessionError> {
        let mut state = State::default();
        for message in messages {
            state.check_sequence(message)?;
            state.record(message.clone());
        }
        Ok(state)
    }

    /// A fresh identifier for a message appended without one. The counter is
    /// bumped until the id is unused.
    fn generate_id(&mut self) -> String {
        loop {
            self.checkpoint_counter += 1;
            let id = format!("m-{}", self.checkpoint_counter);
            if !self.seen_ids.contains(&id) {
                return id;
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

    async fn append(&self, mut message: Message) -> Result<(), SessionError> {
        message.validate()?;
        let mut state = self.state.lock().expect("session mutex");
        if message.id.trim().is_empty() {
            message.id = state.generate_id();
        }
        if state.seen_ids.contains(&message.id) {
            return Err(SessionError::Sequence("duplicate message id"));
        }
        state.check_sequence(&message)?;
        if state
            .latest_compaction
            .as_ref()
            .is_some_and(|latest| latest.first_post_checkpoint_message_id.is_empty())
        {
            if message.id.trim().is_empty() {
                return Err(SessionError::Sequence(
                    "first post-checkpoint message id is required",
                ));
            }
            if !matches!(message.role, Role::User | Role::Assistant | Role::Tool) {
                return Err(SessionError::Sequence(
                    "first post-checkpoint message must have a normal role",
                ));
            }
            let id = message.id.clone();
            if let Some(latest) = state.latest_compaction.as_mut() {
                latest.first_post_checkpoint_message_id = id;
            }
        }
        state.seen_ids.insert(message.id.clone());
        state.record(message);
        Ok(())
    }

    fn latest_compaction(&self) -> Option<CompactionMetadata> {
        self.state
            .lock()
            .expect("session mutex")
            .latest_compaction
            .clone()
    }

    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError> {
        compaction::validate_compaction_checkpoint(&checkpoint)
            .map_err(|error| SessionError::Persist(error.to_string()))?;
        let mut state = self.state.lock().expect("session mutex");
        let Some(first_kept) = state
            .messages
            .iter()
            .position(|message| message.id == checkpoint.first_kept_entry_id)
        else {
            return Err(SessionError::Sequence(
                "compaction first-kept message is not in the active context",
            ));
        };
        state.checkpoint_counter += 1;
        let checkpoint_id = format!("compaction-{}", state.checkpoint_counter);

        let mut context = context::new_context_message(
            &checkpoint_id,
            context::COMPACTION_CONTEXT_TYPE,
            true,
            format!("[Compaction summary]\n{}", checkpoint.summary),
            checkpoint.created_at,
            checkpoint.usage,
        );
        context.context_tokens_before = checkpoint.tokens_before;

        let mut candidate = Vec::with_capacity(1 + state.messages.len() - first_kept);
        candidate.push(context);
        candidate.extend_from_slice(&state.messages[first_kept..]);
        let replayed = State::replay(&candidate)?;

        let metadata = CompactionMetadata {
            id: checkpoint_id.clone(),
            summary: checkpoint.summary,
            first_kept_entry_id: checkpoint.first_kept_entry_id,
            tokens_before: checkpoint.tokens_before,
            usage: checkpoint.usage,
            details: checkpoint.details,
            retained_tail_only: false,
            first_post_checkpoint_message_id: String::new(),
        };
        state.messages = replayed.messages;
        state.pending = replayed.pending;
        state.seen_call_ids = replayed.seen_call_ids;
        state.seen_ids.insert(checkpoint_id);
        state.latest_compaction = Some(metadata.clone());
        Ok(metadata)
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
