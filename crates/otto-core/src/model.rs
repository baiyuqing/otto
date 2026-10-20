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

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

/// A rejected value at a trust boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ValidationError(pub &'static str);

/// Opaque identity shared by every attempt of one logical operation.
///
/// The wire value is 1 to 128 ASCII bytes and may contain only alphanumeric
/// characters plus `.`, `_`, `:`, and `-`. Consumers must not parse meaning
/// from the identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(String);

impl OperationId {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        let bytes = value.as_bytes();
        if bytes.is_empty()
            || bytes.len() > 128
            || bytes.iter().any(|byte| {
                !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'_' | b':' | b'-')
            })
        {
            return Err(ValidationError(
                "operation id must be 1 to 128 permitted ASCII bytes",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for OperationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for OperationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// What the operation's caller observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationDisposition {
    Succeeded,
    Error,
    Cancelled,
    DeadlineExceeded,
    Interrupted,
}

/// What Otto can prove about externally visible effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectCertainty {
    NotStarted,
    KnownNoEffect,
    Completed,
    Unknown,
}

/// Why execution stopped when that reason must cross an API boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStopReason {
    UserCancellation,
    Deadline,
    Shutdown,
    Migration,
    TransportLost,
    ProcessLost,
}

/// Typed operation outcome attached to a tool result.
///
/// `operation_id` is absent for legacy history whose logical operation was
/// never recorded. The remaining fields still make the interruption outcome
/// machine-readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    pub disposition: OperationDisposition,
    pub effect_certainty: EffectCertainty,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<OperationStopReason>,
}

impl ToolResultMetadata {
    pub fn validate(&self) -> Result<(), ValidationError> {
        OperationOutcome {
            disposition: self.disposition,
            effect_certainty: self.effect_certainty,
            stop_reason: self.stop_reason,
        }
        .validate()
    }
}

/// The implementation-owned safety of repeating an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrySafety {
    ReadOnly,
    Idempotent,
    IdempotentWithKey,
    NonIdempotent,
}

/// Durable terminal classification, intentionally excluding output and errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationOutcome {
    pub disposition: OperationDisposition,
    pub effect_certainty: EffectCertainty,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<OperationStopReason>,
}

impl OperationOutcome {
    /// Rejects combinations whose fields make contradictory claims.
    pub fn validate(&self) -> Result<(), ValidationError> {
        use EffectCertainty::{Completed, NotStarted};
        use OperationDisposition::{Cancelled, DeadlineExceeded, Error, Interrupted, Succeeded};
        use OperationStopReason::{
            Deadline, Migration, ProcessLost, Shutdown, TransportLost, UserCancellation,
        };

        if self.disposition == Succeeded && self.effect_certainty != Completed {
            return Err(ValidationError(
                "successful operation must have completed effects",
            ));
        }
        if self.effect_certainty == Completed && !matches!(self.disposition, Succeeded | Error) {
            return Err(ValidationError(
                "completed effects require a definitive disposition",
            ));
        }
        if self.effect_certainty == NotStarted && self.disposition == Succeeded {
            return Err(ValidationError("an unstarted operation cannot succeed"));
        }
        let valid_reason = match self.stop_reason {
            None => true,
            Some(UserCancellation) => self.disposition == Cancelled,
            Some(Deadline) => self.disposition == DeadlineExceeded,
            Some(Shutdown | Migration | TransportLost | ProcessLost) => {
                self.disposition == Interrupted
            }
        };
        if !valid_reason {
            return Err(ValidationError(
                "operation disposition and stop reason are inconsistent",
            ));
        }
        Ok(())
    }
}

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
#[derive(Debug, Clone, Default, Serialize)]
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
    /// Read-only transcript projection from persisted operation facts.
    /// Not provider identity; absent in legacy stored blocks.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub invocation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Box<RawValue>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub operation_metadata: Option<ToolResultMetadata>,
}

impl<'de> Deserialize<'de> for Block {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireBlock {
            #[serde(rename = "type")]
            block_type: BlockType,
            #[serde(default)]
            text: String,
            #[serde(default)]
            data: String,
            #[serde(default)]
            mime_type: String,
            #[serde(default)]
            tool_call_id: String,
            #[serde(default)]
            tool_name: String,
            #[serde(default)]
            invocation_id: String,
            #[serde(default)]
            invocation_attempt: Option<u32>,
            #[serde(default)]
            arguments: Option<Box<RawValue>>,
            #[serde(default)]
            is_error: bool,
            #[serde(default)]
            operation_id: Option<OperationId>,
            #[serde(default)]
            disposition: Option<OperationDisposition>,
            #[serde(default)]
            effect_certainty: Option<EffectCertainty>,
            #[serde(default)]
            stop_reason: Option<OperationStopReason>,
        }

        let wire = WireBlock::deserialize(deserializer)?;
        let has_metadata = wire.operation_id.is_some()
            || wire.disposition.is_some()
            || wire.effect_certainty.is_some()
            || wire.stop_reason.is_some();
        let operation_metadata = if has_metadata {
            let metadata = ToolResultMetadata {
                operation_id: wire.operation_id,
                disposition: wire
                    .disposition
                    .ok_or_else(|| serde::de::Error::custom("operation disposition is required"))?,
                effect_certainty: wire.effect_certainty.ok_or_else(|| {
                    serde::de::Error::custom("operation effect certainty is required")
                })?,
                stop_reason: wire.stop_reason,
            };
            metadata.validate().map_err(serde::de::Error::custom)?;
            Some(metadata)
        } else {
            None
        };
        if operation_metadata.is_some() && wire.block_type != BlockType::ToolResult {
            return Err(serde::de::Error::custom(
                "operation metadata is only valid on tool-result blocks",
            ));
        }
        Ok(Self {
            block_type: wire.block_type,
            text: wire.text,
            data: wire.data,
            mime_type: wire.mime_type,
            tool_call_id: wire.tool_call_id,
            tool_name: wire.tool_name,
            invocation_id: wire.invocation_id,
            invocation_attempt: wire.invocation_attempt,
            arguments: wire.arguments,
            is_error: wire.is_error,
            operation_metadata,
        })
    }
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
            && self.invocation_id == other.invocation_id
            && self.invocation_attempt == other.invocation_attempt
            && self.arguments.as_ref().map(|raw| raw.get())
                == other.arguments.as_ref().map(|raw| raw.get())
            && self.is_error == other.is_error
            && self.operation_metadata == other.operation_metadata
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
                    || self.operation_metadata.is_some()
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
                    || self.operation_metadata.is_some()
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
                    || self.operation_metadata.is_some()
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
                if let Some(metadata) = self.operation_metadata.as_ref() {
                    metadata.validate()?;
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
                    || self.operation_metadata.is_some()
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
    /// The stable id of the input this message delivers (a mailbox entry or
    /// a client `request_id`); empty for every other message. Persisted as
    /// the Otto detail `inputId` on user and context messages.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub input_id: String,
}

/// Whether `id` is a well-formed input id: 1 to 128 bytes of ASCII letters,
/// digits, `.`, `_`, `:` and `-`.
pub fn is_input_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
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
            input_id: String::new(),
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
        if !self.input_id.is_empty() {
            if !is_input_id(&self.input_id) {
                return Err(ValidationError("input id is malformed"));
            }
            if !matches!(self.role, Role::User | Role::Context) {
                return Err(ValidationError(
                    "only user and context messages carry an input id",
                ));
            }
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
mod tests;
