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
    /// Newly cached input, included in input_tokens; absent if not reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<i64>,
    /// One-hour cache writes, a subset of cache_write_input_tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h_input_tokens: Option<i64>,
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
        if let Some(written) = self.cache_write_input_tokens
            && (written < 0 || written > self.input_tokens - self.cached_input_tokens)
        {
            return Err(ValidationError(
                "cache writes must fit within uncached input tokens",
            ));
        }
        if let Some(one_hour) = self.cache_write_1h_input_tokens
            && (one_hour < 0
                || self
                    .cache_write_input_tokens
                    .is_none_or(|written| one_hour > written))
        {
            return Err(ValidationError(
                "one-hour cache writes must fit within cache writes",
            ));
        }
        Ok(())
    }

    /// Adds two reported usages. A missing breakdown on either side makes its
    /// aggregate unknown; callers must handle the first report separately.
    pub fn saturating_add(self, other: Self) -> Self {
        let mut total = Self {
            input_tokens: self
                .input_tokens
                .max(0)
                .saturating_add(other.input_tokens.max(0)),
            output_tokens: self
                .output_tokens
                .max(0)
                .saturating_add(other.output_tokens.max(0)),
            cached_input_tokens: self
                .cached_input_tokens
                .max(0)
                .saturating_add(other.cached_input_tokens.max(0)),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .zip(other.cache_write_input_tokens)
                .map(|(left, right)| left.max(0).saturating_add(right.max(0))),
            cache_write_1h_input_tokens: self
                .cache_write_1h_input_tokens
                .zip(other.cache_write_1h_input_tokens)
                .map(|(left, right)| left.max(0).saturating_add(right.max(0))),
        };
        if total.validate().is_err() {
            total.cache_write_input_tokens = None;
            total.cache_write_1h_input_tokens = None;
        }
        total
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

mod input_provenance;
pub use input_provenance::*;
mod message;
mod operation;

pub use message::*;
pub use operation::*;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "model/input_provenance_tests.rs"]
mod input_provenance_tests;
