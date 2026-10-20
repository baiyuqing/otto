//! Context messages to Pi entries, stop reasons and assistant tool-finish validation.

use super::*;

// ---------------------------------------------------------------------------
// model -> Pi
// ---------------------------------------------------------------------------

/// Encodes one Otto message as the Pi entry that will be appended, and returns
/// the message as it will be replayed from disk (its id is the entry id).
pub fn model_message_to_pi_entry(
    message: &Message,
    entry_id: &str,
    parent_id: Option<&str>,
    header: &Header,
) -> Result<(PiEntry, Message), PiError> {
    if message.role == Role::Context {
        return model_context_message_to_pi_entry(message, entry_id, parent_id);
    }
    let timestamp = format_persisted_timestamp(message.created_at, "message")?;
    let content = model_blocks_to_pi_content(message.role.clone(), &message.blocks)?;
    let (_, content_blocks) =
        super::super::codec::decode_content(&content, "message content", false)?;
    let mut wire = PiMessage {
        role: role_wire_name(message.role.clone()).to_owned(),
        content: Some(content),
        content_blocks,
        timestamp: message.created_at.timestamp_millis(),
        ..PiMessage::default()
    };

    match message.role {
        Role::User => {
            if message.finish_reason.is_some() || message.usage.is_some() {
                return Err(PiError::invalid(
                    "user message contains assistant-only metadata",
                ));
            }
            if !message.input_id.is_empty() {
                if !crate::model::is_input_id(&message.input_id) {
                    return Err(PiError::invalid("invalid input id"));
                }
                wire.details = Some(encode_pi_otto_details(&PiOttoDetails {
                    input_id: message.input_id.clone(),
                    ..PiOttoDetails::default()
                })?);
            }
        }
        Role::Assistant => {
            let stop_reason = model_finish_reason_to_pi(message.finish_reason.clone())?;
            validate_assistant_tool_finish(&message.blocks, message.finish_reason.clone())?;
            let usage = model_usage_to_pi(message.usage.as_ref())?;
            if message.usage == Some(Usage::default()) {
                wire.details = Some(encode_pi_otto_details(&PiOttoDetails {
                    usage_present: true,
                    ..PiOttoDetails::default()
                })?);
            }
            wire.api = "openai-completions".into();
            wire.provider = header.provider.clone();
            wire.model = header.model.clone();
            wire.usage = usage;
            wire.stop_reason = stop_reason;
            if message.finish_reason == Some(FinishReason::Unknown) {
                wire.error_message =
                    "assistant response ended with an unknown finish reason".into();
            }
        }
        Role::Tool => {
            if message.finish_reason.is_some()
                || message.usage.is_some()
                || message.blocks.len() != 1
            {
                return Err(PiError::invalid("tool result message is malformed"));
            }
            let block = &message.blocks[0];
            if block.block_type != BlockType::ToolResult
                || block.tool_call_id.trim().is_empty()
                || block.tool_name.trim().is_empty()
            {
                return Err(PiError::invalid("tool result id and name are required"));
            }
            wire.role = "toolResult".into();
            wire.tool_call_id = block.tool_call_id.clone();
            wire.tool_name = block.tool_name.clone();
            wire.is_error = Some(block.is_error);
            if let Some(metadata) = block.operation_metadata.as_ref() {
                metadata.validate().map_err(|error| {
                    PiError::invalid(format!("invalid operation metadata: {error}"))
                })?;
                wire.details = Some(encode_pi_otto_details(&PiOttoDetails {
                    operation_id: metadata.operation_id.clone(),
                    disposition: Some(metadata.disposition),
                    effect_certainty: Some(metadata.effect_certainty),
                    stop_reason: metadata.stop_reason,
                    ..PiOttoDetails::default()
                })?);
            }
        }
        _ => return Err(PiError::invalid("unsupported message role")),
    }

    let entry = PiEntry {
        type_name: "message".into(),
        id: entry_id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        timestamp,
        message: Some(Box::new(wire)),
        ..PiEntry::default()
    };
    let mut persisted = message.clone();
    persisted.id = entry_id.to_owned();
    Ok((entry, persisted))
}

/// Encodes a `Role::Context` message as a Pi v3 `custom_message` entry.
/// `compaction` and `branch_summary` are reserved: those context messages are
/// anchored to a checkpoint and must not reach this path.
fn model_context_message_to_pi_entry(
    message: &Message,
    entry_id: &str,
    parent_id: Option<&str>,
) -> Result<(PiEntry, Message), PiError> {
    let timestamp = format_persisted_timestamp(message.created_at, "context message")?;
    if message.context_type.is_empty() {
        return Err(PiError::invalid("context message type is required"));
    }
    if message.context_type == COMPACTION_CONTEXT_TYPE
        || message.context_type == BRANCH_CONTEXT_TYPE
    {
        return Err(PiError::invalid(format!(
            "context type {:?} is reserved",
            message.context_type
        )));
    }
    if message.finish_reason.is_some() {
        return Err(PiError::invalid(
            "context message contains assistant-only metadata",
        ));
    }
    if message.blocks.is_empty() {
        return Err(PiError::invalid("context message content is required"));
    }
    let mut content = String::new();
    for block in &message.blocks {
        if block.block_type != BlockType::Text
            || !block.tool_call_id.is_empty()
            || !block.tool_name.is_empty()
            || block.arguments.is_some()
            || block.is_error
        {
            return Err(PiError::invalid(
                "context message must contain only text blocks",
            ));
        }
        content.push_str(&block.text);
    }
    let content_json = to_raw_value(&content)?;

    let mut custom = PiCustomMessage {
        custom_type: message.context_type.clone(),
        content: Some(content_json),
        display: message.display,
        content_text: Some(content.clone()),
        ..PiCustomMessage::default()
    };
    if !message.input_id.is_empty() && !crate::model::is_input_id(&message.input_id) {
        return Err(PiError::invalid("invalid input id"));
    }
    if message.context_metadata.is_some() || !message.input_id.is_empty() || message.usage.is_some()
    {
        custom.details = Some(encode_pi_otto_details(&PiOttoDetails {
            task_id: message
                .context_metadata
                .as_ref()
                .map(|metadata| metadata.task_id.clone())
                .unwrap_or_default(),
            input_id: message.input_id.clone(),
            // `piCustomMessage` has no usage field in the Pi v3 format, so a
            // notification's token cost (such as a finished sub-agent task's)
            // travels in the Otto extension instead of being dropped.
            usage: message.usage,
            ..PiOttoDetails::default()
        })?);
    }
    let entry = PiEntry {
        type_name: "custom_message".into(),
        id: entry_id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        timestamp,
        custom_message: Some(Box::new(custom)),
        ..PiEntry::default()
    };
    let persisted = Message {
        id: entry_id.to_owned(),
        role: Role::Context,
        blocks: vec![Block::text(content)],
        created_at: message.created_at,
        context_type: message.context_type.clone(),
        display: message.display,
        context_metadata: message.context_metadata.clone(),
        input_id: message.input_id.clone(),
        usage: message.usage,
        ..Message::default()
    };
    Ok((entry, persisted))
}

fn role_wire_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::Context => "context",
        Role::Other(_) => "",
    }
}

fn model_blocks_to_pi_content(role: Role, blocks: &[Block]) -> Result<Box<RawValue>, PiError> {
    if blocks.is_empty() && role != Role::Assistant {
        return Err(PiError::invalid("message content is required"));
    }
    let mut content = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block.block_type {
            BlockType::Text => {
                if role != Role::User && role != Role::Assistant {
                    return Err(PiError::invalid(
                        "text content is incompatible with message role",
                    ));
                }
                if !block.tool_call_id.is_empty()
                    || !block.tool_name.is_empty()
                    || block.arguments.is_some()
                    || block.is_error
                {
                    return Err(PiError::invalid("text block contains incompatible fields"));
                }
                content.push(PiContentBlock {
                    type_name: "text".into(),
                    text: block.text.clone(),
                    ..PiContentBlock::default()
                });
            }
            BlockType::Image => {
                if role != Role::User {
                    return Err(PiError::invalid(
                        "image content is incompatible with message role",
                    ));
                }
                content.push(PiContentBlock {
                    type_name: "image".into(),
                    data: block.data.clone(),
                    mime_type: block.mime_type.clone(),
                    ..PiContentBlock::default()
                });
            }
            BlockType::ToolCall => {
                if role != Role::Assistant
                    || block.tool_call_id.trim().is_empty()
                    || block.tool_name.trim().is_empty()
                {
                    return Err(PiError::invalid(
                        "assistant tool-call id and name are required",
                    ));
                }
                if !valid_tool_arguments(block.arguments.as_deref()) {
                    return Err(PiError::invalid(
                        "tool-call arguments must be a JSON object",
                    ));
                }
                if !block.text.is_empty() || block.is_error {
                    return Err(PiError::invalid(
                        "tool-call block contains incompatible fields",
                    ));
                }
                content.push(PiContentBlock {
                    type_name: "toolCall".into(),
                    id: block.tool_call_id.clone(),
                    name: block.tool_name.clone(),
                    arguments: block.arguments.clone(),
                    ..PiContentBlock::default()
                });
            }
            BlockType::ToolResult => {
                if role != Role::Tool || blocks.len() != 1 {
                    return Err(PiError::invalid(
                        "tool-result block is incompatible with message role",
                    ));
                }
                content.push(PiContentBlock {
                    type_name: "text".into(),
                    text: block.text.clone(),
                    ..PiContentBlock::default()
                });
            }
            BlockType::Reasoning => {
                if role != Role::Assistant {
                    return Err(PiError::invalid(
                        "reasoning content is incompatible with message role",
                    ));
                }
                content.push(PiContentBlock {
                    type_name: "thinking".into(),
                    thinking: block.text.clone(),
                    ..PiContentBlock::default()
                });
            }
            BlockType::Other(_) => {
                return Err(PiError::invalid("unsupported message block type"));
            }
        }
    }
    marshal_pi_content(&content)
}

/// Serializes content blocks for storage. `text` is always written, even when
/// empty, so the strict decoder accepts the record; `skip_serializing_if`
/// alone would drop it and make empty tool output fail the append-time
/// self-check.
fn marshal_pi_content(content: &[PiContentBlock]) -> Result<Box<RawValue>, PiError> {
    let mut wire = String::from("[");
    for (index, block) in content.iter().enumerate() {
        if index > 0 {
            wire.push(',');
        }
        if block.type_name == "text" {
            let text = serde_json::to_string(&block.text)
                .map_err(|error| PiError::other(format!("encode Pi message content: {error}")))?;
            let _ = write!(wire, r#"{{"type":"text","text":{text}}}"#);
            continue;
        }
        let encoded = serde_json::to_string(block)
            .map_err(|error| PiError::other(format!("encode Pi message content: {error}")))?;
        wire.push_str(&encoded);
    }
    wire.push(']');
    RawValue::from_string(wire)
        .map_err(|error| PiError::other(format!("encode Pi message content: {error}")))
}

fn model_finish_reason_to_pi(reason: Option<FinishReason>) -> Result<String, PiError> {
    match reason {
        Some(FinishReason::Stop) => Ok("stop".into()),
        Some(FinishReason::ToolCalls) => Ok("toolUse".into()),
        Some(FinishReason::Length) => Ok("length".into()),
        Some(FinishReason::Unknown) => Ok("error".into()),
        _ => Err(PiError::invalid("unsupported assistant finish reason")),
    }
}

/// Converts Otto usage into the Pi wire shape. `None` becomes an all-zero usage
/// object.
pub fn model_usage_to_pi(usage: Option<&Usage>) -> Result<Option<PiUsage>, PiError> {
    let Some(usage) = usage else {
        return Ok(Some(PiUsage::default()));
    };
    usage
        .validate()
        .map_err(|error| PiError::invalid(error.0))?;
    if usage.output_tokens > i64::MAX - usage.input_tokens {
        return Err(PiError::invalid("usage token total overflows"));
    }
    Ok(Some(PiUsage {
        input: usage.input_tokens - usage.cached_input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cached_input_tokens,
        cache_write: 0,
        total_tokens: usage.input_tokens + usage.output_tokens,
        ..PiUsage::default()
    }))
}

pub(super) fn pi_stop_reason_to_model(reason: &str) -> Result<FinishReason, PiError> {
    match reason {
        "stop" => Ok(FinishReason::Stop),
        "toolUse" => Ok(FinishReason::ToolCalls),
        "length" => Ok(FinishReason::Length),
        "error" | "aborted" => Ok(FinishReason::Unknown),
        "pending" => Err(PiError::invalid(
            "pending assistant message cannot be persisted",
        )),
        _ => Err(PiError::invalid("unsupported Pi assistant stop reason")),
    }
}

/// Normalizes legacy usage: prompt input is `input + cacheRead + cacheWrite`,
/// and an all-zero usage is reported as absent so the explicit-zero marker in
/// `details.otto.usagePresent` can tell the two apart.
pub fn pi_usage_to_model(usage: Option<&PiUsage>) -> Result<Option<Usage>, PiError> {
    let Some(usage) = usage else {
        return Err(PiError::invalid("assistant usage is required"));
    };
    if usage.input < 0
        || usage.output < 0
        || usage.cache_read < 0
        || usage.cache_write < 0
        || usage.total_tokens < 0
    {
        return Err(PiError::invalid(
            "assistant usage is outside the supported range",
        ));
    }
    let prompt_input = usage
        .input
        .checked_add(usage.cache_read)
        .and_then(|sum| sum.checked_add(usage.cache_write))
        .ok_or_else(|| PiError::invalid("assistant usage is outside the supported range"))?;
    if prompt_input == 0 && usage.output == 0 && usage.cache_read == 0 && usage.cache_write == 0 {
        return Ok(None);
    }
    Ok(Some(Usage {
        input_tokens: prompt_input,
        output_tokens: usage.output,
        cached_input_tokens: usage.cache_read,
    }))
}

/// Rejects an assistant message whose tool calls and finish reason disagree.
pub fn validate_assistant_tool_finish(
    blocks: &[Block],
    reason: Option<FinishReason>,
) -> Result<(), PiError> {
    let has_tool_call = blocks
        .iter()
        .any(|block| block.block_type == BlockType::ToolCall);
    if has_tool_call && reason != Some(FinishReason::ToolCalls) {
        return Err(PiError::invalid(
            "assistant tool calls require tool_calls finish reason",
        ));
    }
    if !has_tool_call && reason == Some(FinishReason::ToolCalls) {
        return Err(PiError::invalid(
            "tool_calls finish reason requires a tool call",
        ));
    }
    Ok(())
}
