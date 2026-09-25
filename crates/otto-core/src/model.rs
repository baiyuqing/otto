//! Transcript data model: roles, content blocks, messages, tool definitions,
//! and token usage.
//!
//! The serde field names, the omit-empty behavior, and every `validate` rule
//! and error message are pinned by the session JSON format that stored sessions
//! are written in.
//!
//! Ownership: every type here is a plain owned value and derives `Clone`.
//!
//! Concurrency: these types hold no interior mutability and are `Send + Sync`.
//!
//! Errors: `validate` returns [`ValidationError`].

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

/// A rejected value at a trust boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ValidationError(pub &'static str);

/// The timestamp stored sessions carry for an unset time,
/// `0001-01-01T00:00:00Z`.
///
/// The agent loop uses it as the "not set" marker for [`Message::created_at`].
pub fn zero_time() -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(1, 1, 1)
        .expect("0001-01-01 is a valid date")
        .and_time(NaiveTime::MIN)
        .and_utc()
}

/// Who produced a message.
///
/// Unknown wire values decode into [`Role::Other`] instead of failing, so that
/// [`Message::validate`] rejects them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Role {
    User,
    Assistant,
    Tool,
    Context,
    Other(String),
}

impl From<String> for Role {
    fn from(value: String) -> Self {
        match value.as_str() {
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "tool" => Self::Tool,
            "context" => Self::Context,
            _ => Self::Other(value),
        }
    }
}

impl From<Role> for String {
    fn from(value: Role) -> Self {
        match value {
            Role::User => "user".into(),
            Role::Assistant => "assistant".into(),
            Role::Tool => "tool".into(),
            Role::Context => "context".into(),
            Role::Other(other) => other,
        }
    }
}

impl Default for Role {
    /// The empty string, which `validate` rejects.
    fn default() -> Self {
        Self::Other(String::new())
    }
}

/// The shape of one piece of message content.
///
/// Unknown wire values decode into [`BlockType::Other`] so that
/// [`Block::validate`] rejects them rather than the decoder failing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum BlockType {
    Text,
    Image,
    ToolCall,
    ToolResult,
    /// Model reasoning text shown to the user. It is persisted and displayed
    /// but never sent back to a provider.
    Reasoning,
    Other(String),
}

impl From<String> for BlockType {
    fn from(value: String) -> Self {
        match value.as_str() {
            "text" => Self::Text,
            "image" => Self::Image,
            "tool_call" => Self::ToolCall,
            "tool_result" => Self::ToolResult,
            "reasoning" => Self::Reasoning,
            _ => Self::Other(value),
        }
    }
}

impl From<BlockType> for String {
    fn from(value: BlockType) -> Self {
        match value {
            BlockType::Text => "text".into(),
            BlockType::Image => "image".into(),
            BlockType::ToolCall => "tool_call".into(),
            BlockType::ToolResult => "tool_result".into(),
            BlockType::Reasoning => "reasoning".into(),
            BlockType::Other(other) => other,
        }
    }
}

impl Default for BlockType {
    fn default() -> Self {
        Self::Other(String::new())
    }
}

/// Why the provider stopped generating.
///
/// An absent reason is `None`; both it and the empty string are accepted by
/// [`Message::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Unknown,
    Other(String),
}

impl From<String> for FinishReason {
    fn from(value: String) -> Self {
        match value.as_str() {
            "stop" => Self::Stop,
            "tool_calls" => Self::ToolCalls,
            "length" => Self::Length,
            "unknown" => Self::Unknown,
            _ => Self::Other(value),
        }
    }
}

impl From<FinishReason> for String {
    fn from(value: FinishReason) -> Self {
        match value {
            FinishReason::Stop => "stop".into(),
            FinishReason::ToolCalls => "tool_calls".into(),
            FinishReason::Length => "length".into(),
            FinishReason::Unknown => "unknown".into(),
            FinishReason::Other(other) => other,
        }
    }
}

/// One piece of message content.
///
/// `arguments` is provider JSON passed through verbatim. Keeping it as
/// [`RawValue`] preserves number literals exactly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Block {
    #[serde(rename = "type")]
    pub block_type: BlockType,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Box<RawValue>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
}

impl PartialEq for Block {
    /// `arguments` compares by raw JSON text, since [`RawValue`] has no
    /// `PartialEq`.
    fn eq(&self, other: &Self) -> bool {
        self.block_type == other.block_type
            && self.text == other.text
            && self.data == other.data
            && self.mime_type == other.mime_type
            && self.tool_call_id == other.tool_call_id
            && self.tool_name == other.tool_name
            && self.arguments.as_ref().map(|raw| raw.get())
                == other.arguments.as_ref().map(|raw| raw.get())
            && self.is_error == other.is_error
    }
}

impl Eq for Block {}

impl Block {
    /// Convenience constructor for a text block.
    pub fn text(value: impl Into<String>) -> Self {
        Self {
            block_type: BlockType::Text,
            text: value.into(),
            ..Self::default()
        }
    }

    pub fn reasoning(value: impl Into<String>) -> Self {
        Self {
            block_type: BlockType::Reasoning,
            text: value.into(),
            ..Self::default()
        }
    }

    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            block_type: BlockType::Image,
            data: data.into(),
            mime_type: mime_type.into(),
            ..Self::default()
        }
    }

    /// Rejects blocks that carry fields incompatible with their type.
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self.block_type {
            BlockType::Text => {
                if !self.data.is_empty()
                    || !self.mime_type.is_empty()
                    || !self.tool_call_id.is_empty()
                    || !self.tool_name.is_empty()
                    || self.arguments.is_some()
                    || self.is_error
                {
                    return Err(ValidationError("text block contains incompatible fields"));
                }
            }
            BlockType::Image => {
                if !self.text.is_empty()
                    || !self.tool_call_id.is_empty()
                    || !self.tool_name.is_empty()
                    || self.arguments.is_some()
                    || self.is_error
                    || !valid_image(&self.data, &self.mime_type)
                {
                    return Err(ValidationError("image block is malformed"));
                }
            }
            BlockType::ToolCall => {
                if self.tool_call_id.trim().is_empty()
                    || self.tool_name.trim().is_empty()
                    || !self.text.is_empty()
                    || !self.data.is_empty()
                    || !self.mime_type.is_empty()
                    || self.is_error
                    || !is_json_object(self.arguments.as_deref())
                {
                    return Err(ValidationError("tool-call block is malformed"));
                }
            }
            BlockType::ToolResult => {
                if self.tool_call_id.trim().is_empty()
                    || self.tool_name.trim().is_empty()
                    || !self.data.is_empty()
                    || !self.mime_type.is_empty()
                    || self.arguments.is_some()
                {
                    return Err(ValidationError("tool-result block is malformed"));
                }
            }
            BlockType::Reasoning => {
                if self.text.is_empty()
                    || !self.data.is_empty()
                    || !self.mime_type.is_empty()
                    || !self.tool_call_id.is_empty()
                    || !self.tool_name.is_empty()
                    || self.arguments.is_some()
                    || self.is_error
                {
                    return Err(ValidationError("reasoning block is malformed"));
                }
            }
            BlockType::Other(_) => {
                return Err(ValidationError("unsupported message block type"));
            }
        }
        Ok(())
    }
}

/// Extra identity carried by context messages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextMetadata {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task_id: String,
}

impl ContextMetadata {
    /// Requires a generated task id: `t` followed by 1 to 63 ASCII digits.
    pub fn validate(&self) -> Result<(), ValidationError> {
        const INVALID: ValidationError =
            ValidationError("context task id must be a bounded generated id");
        let bytes = self.task_id.as_bytes();
        if bytes.len() < 2 || bytes.len() > 64 || bytes[0] != b't' {
            return Err(INVALID);
        }
        if bytes[1..].iter().any(|byte| !byte.is_ascii_digit()) {
            return Err(INVALID);
        }
        Ok(())
    }
}

/// One transcript entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    #[serde(deserialize_with = "deserialize_blocks")]
    pub blocks: Vec<Block>,
    #[serde(default = "zero_time")]
    pub created_at: DateTime<Utc>,
    #[serde(
        default,
        deserialize_with = "deserialize_finish_reason",
        skip_serializing_if = "Option::is_none"
    )]
    pub finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context_type: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_tokens_before: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_metadata: Option<ContextMetadata>,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            id: String::new(),
            role: Role::default(),
            blocks: Vec::new(),
            created_at: zero_time(),
            finish_reason: None,
            usage: None,
            context_type: String::new(),
            context_tokens_before: 0,
            display: false,
            context_metadata: None,
        }
    }
}

impl Message {
    /// Concatenates the text of every text block, skipping all other blocks.
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter(|block| block.block_type == BlockType::Text)
            .map(|block| block.text.as_str())
            .collect()
    }

    /// Returns true when any block is a tool call.
    pub fn has_tool_call(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| block.block_type == BlockType::ToolCall)
    }

    /// Rejects messages whose role, metadata, and blocks are inconsistent.
    ///
    /// A message with no identifier and no timestamp is accepted: those are
    /// filled in by the agent before the message is persisted.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if matches!(self.role, Role::Other(_)) {
            return Err(ValidationError("unsupported message role"));
        }
        if self.context_tokens_before < 0 {
            return Err(ValidationError("context tokens before must be nonnegative"));
        }
        if self.role != Role::Context
            && (!self.context_type.is_empty()
                || self.context_tokens_before != 0
                || self.display
                || self.context_metadata.is_some())
        {
            return Err(ValidationError(
                "context-only fields require a context message",
            ));
        }
        if let Some(usage) = &self.usage {
            usage.validate()?;
        }
        if let Some(metadata) = &self.context_metadata {
            metadata.validate()?;
        }
        for block in &self.blocks {
            block.validate()?;
        }

        match self.role {
            Role::User => {
                if self.blocks.is_empty() {
                    return Err(ValidationError("user message content is required"));
                }
                if self.finish_reason.is_some() || self.usage.is_some() {
                    return Err(ValidationError(
                        "user message contains assistant-only metadata",
                    ));
                }
                if self
                    .blocks
                    .iter()
                    .any(|block| !matches!(block.block_type, BlockType::Text | BlockType::Image))
                {
                    return Err(ValidationError("user message contains incompatible block"));
                }
            }
            Role::Assistant => {
                if matches!(self.finish_reason, Some(FinishReason::Other(_))) {
                    return Err(ValidationError("unsupported assistant finish reason"));
                }
                if self.blocks.iter().any(|block| {
                    !matches!(
                        block.block_type,
                        BlockType::Text | BlockType::ToolCall | BlockType::Reasoning
                    )
                }) {
                    return Err(ValidationError(
                        "assistant message contains incompatible block",
                    ));
                }
                let has_tool_call = self.has_tool_call();
                let is_tool_calls = self.finish_reason == Some(FinishReason::ToolCalls);
                if has_tool_call && self.finish_reason.is_some() && !is_tool_calls {
                    return Err(ValidationError(
                        "assistant tool calls require tool_calls finish reason",
                    ));
                }
                if !has_tool_call && is_tool_calls {
                    return Err(ValidationError(
                        "tool_calls finish reason requires a tool call",
                    ));
                }
            }
            Role::Tool => {
                if self.finish_reason.is_some() || self.usage.is_some() || self.blocks.is_empty() {
                    return Err(ValidationError("tool result message is malformed"));
                }
                if self
                    .blocks
                    .iter()
                    .any(|block| block.block_type != BlockType::ToolResult)
                {
                    return Err(ValidationError("tool result message is malformed"));
                }
            }
            Role::Context => {
                if self.context_type.trim().is_empty() {
                    return Err(ValidationError("context message type is required"));
                }
                if self.finish_reason.is_some() {
                    return Err(ValidationError(
                        "context message contains assistant-only metadata",
                    ));
                }
                if self.blocks.is_empty() {
                    return Err(ValidationError("context message content is required"));
                }
                if self
                    .blocks
                    .iter()
                    .any(|block| block.block_type != BlockType::Text)
                {
                    return Err(ValidationError(
                        "context message must contain only text blocks",
                    ));
                }
            }
            Role::Other(_) => unreachable!("rejected above"),
        }
        Ok(())
    }
}

pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

fn valid_image(data: &str, mime_type: &str) -> bool {
    if data.is_empty() || data.len() > MAX_IMAGE_BYTES * 4 / 3 + 4 {
        return false;
    }
    let Ok(bytes) = BASE64.decode(data) else {
        return false;
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return false;
    }
    match mime_type {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

/// A tool advertised to the provider.
///
/// `parameters` is a JSON Schema object kept as raw bytes end to end, so large
/// integer constraints are never rounded through a float.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub parameters: Option<Box<RawValue>>,
}

impl PartialEq for ToolDefinition {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.description == other.description
            && self.parameters.as_ref().map(|raw| raw.get())
                == other.parameters.as_ref().map(|raw| raw.get())
    }
}

impl Eq for ToolDefinition {}

/// Token counts reported by the provider for one assistant message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cached_input_tokens: i64,
}

impl Usage {
    /// Rejects negative counts and a cached count larger than the input count.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.input_tokens < 0 || self.output_tokens < 0 || self.cached_input_tokens < 0 {
            return Err(ValidationError("usage token counts must be nonnegative"));
        }
        if self.cached_input_tokens > self.input_tokens {
            return Err(ValidationError(
                "cached input tokens must not exceed input tokens",
            ));
        }
        Ok(())
    }
}

/// Reports whether `raw` is a JSON object. Number literals are not parsed, so
/// values outside `f64` range are accepted.
fn is_json_object(raw: Option<&RawValue>) -> bool {
    let Some(raw) = raw else { return false };
    serde_json::from_str::<std::collections::BTreeMap<String, &RawValue>>(raw.get()).is_ok()
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// Accepts a JSON `null` for `blocks` as an empty list, which is how stored
/// sessions encode an empty block list.
fn deserialize_blocks<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Block>, D::Error> {
    Ok(Option::<Vec<Block>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Maps an empty-string finish reason to `None`.
fn deserialize_finish_reason<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<FinishReason>, D::Error> {
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw
        .filter(|value| !value.is_empty())
        .map(FinishReason::from))
}

#[cfg(test)]
mod tests {
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
}
