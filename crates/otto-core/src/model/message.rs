//! Context metadata and the transcript message with its validation and image limits.

use super::*;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_provenance: Option<crate::model::InputProvenance>,
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
            input_provenance: None,
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

pub(super) fn valid_image(data: &str, mime_type: &str) -> bool {
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
