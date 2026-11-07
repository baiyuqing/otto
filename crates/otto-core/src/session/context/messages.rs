//! Pi entries to context messages, usage conversion, capabilities and runtime metadata.

use super::*;

/// Converts one active-path entry into the messages it contributes. Entry
/// types that carry no message (labels, runtime changes) contribute none.
pub fn pi_entry_to_context_messages(entry: &PiEntry) -> Result<Vec<Message>, PiError> {
    let created_at = parse_rfc3339(&entry.timestamp)
        .ok_or_else(|| PiError::invalid("entry timestamp is invalid"))?;
    match entry.type_name.as_str() {
        "message" => {
            let wire = entry
                .message
                .as_ref()
                .ok_or_else(|| PiError::invalid("message payload is required"))?;
            Ok(vec![pi_message_to_context_message(
                wire, &entry.id, created_at,
            )?])
        }
        "compaction" => {
            let compaction = entry
                .compaction
                .as_ref()
                .ok_or_else(|| PiError::invalid("compaction payload is required"))?;
            let usage = optional_pi_usage_to_model(
                compaction.usage.as_ref(),
                compaction.details.as_deref(),
            )?;
            let mut summary = new_context_message(
                &entry.id,
                COMPACTION_CONTEXT_TYPE,
                true,
                format!("[Compaction summary]\n{}", compaction.summary),
                created_at,
                usage,
            );
            summary.context_tokens_before = safe_context_token_count(compaction.tokens_before);
            let mut messages = vec![summary];
            if compaction_has_retained_tail(compaction) {
                for (index, wire) in compaction
                    .retained_tail
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                {
                    messages.push(pi_message_to_context_message(
                        wire,
                        &format!("{}-tail-{index}", entry.id),
                        DateTime::from_timestamp_millis(wire.timestamp).unwrap_or_else(zero_time),
                    )?);
                }
            }
            Ok(messages)
        }
        "branch_summary" => {
            let branch = entry
                .branch_summary
                .as_ref()
                .ok_or_else(|| PiError::invalid("branch_summary payload is required"))?;
            let usage =
                optional_pi_usage_to_model(branch.usage.as_ref(), branch.details.as_deref())?;
            Ok(vec![new_context_message(
                &entry.id,
                BRANCH_CONTEXT_TYPE,
                true,
                format!("[Branch summary]\n{}", branch.summary),
                created_at,
                usage,
            )])
        }
        "custom_message" => {
            let custom = entry
                .custom_message
                .as_ref()
                .ok_or_else(|| PiError::invalid("custom_message payload is required"))?;
            let text = pi_context_text(custom.content_text.as_deref(), &custom.content_blocks)?;
            let mut message = new_context_message(
                &entry.id,
                &custom.custom_type,
                custom.display,
                custom_context_text(&custom.custom_type, &text),
                created_at,
                None,
            );
            if let Some(details) = decode_pi_otto_details(custom.details.as_deref())? {
                message.input_id = details.input_id;
                message.usage = details.usage;
                if !details.task_id.is_empty() {
                    message.context_metadata = Some(ContextMetadata {
                        task_id: details.task_id,
                    });
                }
            }
            Ok(vec![message])
        }
        _ => Ok(Vec::new()),
    }
}

fn pi_message_to_context_message(
    wire: &PiMessage,
    id: &str,
    created_at: DateTime<Utc>,
) -> Result<Message, PiError> {
    let mut message = Message {
        id: id.to_owned(),
        created_at,
        ..Message::default()
    };
    match wire.role.as_str() {
        "user" => {
            let blocks = pi_context_text_and_tool_blocks(wire, Role::User)?;
            if blocks.is_empty() {
                return Err(PiError::invalid("user message content is required"));
            }
            message.role = Role::User;
            message.blocks = blocks;
            if let Some(details) = decode_pi_otto_details(wire.details.as_deref())? {
                message.input_id = details.input_id;
            }
        }
        "assistant" => {
            let blocks = pi_context_text_and_tool_blocks(wire, Role::Assistant)?;
            let finish_reason = pi_stop_reason_to_model(&wire.stop_reason)?;
            validate_assistant_tool_finish(&blocks, Some(finish_reason.clone()))?;
            message.usage = pi_assistant_usage_to_model(wire)?;
            message.role = Role::Assistant;
            message.blocks = blocks;
            message.finish_reason = Some(finish_reason);
        }
        "toolResult" => {
            let text = pi_context_text(wire.content_text.as_deref(), &wire.content_blocks)?;
            if wire.tool_call_id.trim().is_empty()
                || wire.tool_name.trim().is_empty()
                || wire.is_error.is_none()
            {
                return Err(PiError::invalid("tool-result message is malformed"));
            }
            let operation_metadata =
                decode_pi_otto_details(wire.details.as_deref())?.and_then(|details| {
                    match (details.disposition, details.effect_certainty) {
                        (Some(disposition), Some(effect_certainty)) => Some(ToolResultMetadata {
                            operation_id: details.operation_id,
                            disposition,
                            effect_certainty,
                            stop_reason: details.stop_reason,
                        }),
                        _ => None,
                    }
                });
            message.role = Role::Tool;
            message.blocks = vec![Block {
                block_type: BlockType::ToolResult,
                text,
                data: String::new(),
                mime_type: String::new(),
                tool_call_id: wire.tool_call_id.clone(),
                tool_name: wire.tool_name.clone(),
                arguments: None,
                is_error: wire.is_error.unwrap_or(false),
                operation_metadata,
                ..Block::default()
            }];
        }
        "custom" => {
            let text = pi_context_text(wire.content_text.as_deref(), &wire.content_blocks)?;
            return Ok(new_context_message(
                id,
                &wire.custom_type,
                wire.display.unwrap_or(false),
                custom_context_text(&wire.custom_type, &text),
                created_at,
                None,
            ));
        }
        "branchSummary" => {
            return Ok(new_context_message(
                id,
                BRANCH_CONTEXT_TYPE,
                true,
                format!("[Branch summary]\n{}", wire.summary),
                created_at,
                None,
            ));
        }
        "compactionSummary" => {
            let mut summary = new_context_message(
                id,
                COMPACTION_CONTEXT_TYPE,
                true,
                format!("[Compaction summary]\n{}", wire.summary),
                created_at,
                None,
            );
            if let Some(tokens_before) = wire.tokens_before {
                summary.context_tokens_before = safe_context_token_count(tokens_before);
            }
            return Ok(summary);
        }
        _ => {
            return Err(PiError::new(
                PiErrorKind::UnsupportedContent,
                "Pi message role is not supported by Otto",
            ));
        }
    }
    Ok(message)
}

fn pi_context_text_and_tool_blocks(message: &PiMessage, role: Role) -> Result<Vec<Block>, PiError> {
    if let Some(text) = message.content_text.as_ref() {
        return Ok(vec![Block::text(text.clone())]);
    }
    let mut blocks = Vec::with_capacity(message.content_blocks.len());
    for block in &message.content_blocks {
        match block.type_name.as_str() {
            "text" => {
                if !block.text_signature.is_empty() {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "provider-specific text signatures are not supported",
                    ));
                }
                blocks.push(Block::text(block.text.clone()));
            }
            "toolCall" => {
                if role != Role::Assistant {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "tool call is incompatible with message role",
                    ));
                }
                if block.id.trim().is_empty()
                    || block.name.trim().is_empty()
                    || !valid_tool_arguments(block.arguments.as_deref())
                {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "tool-call shape cannot be represented",
                    ));
                }
                if !block.thought_signature.is_empty() || !block.namespace.is_empty() {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "provider-specific tool-call content is not supported",
                    ));
                }
                blocks.push(Block {
                    block_type: BlockType::ToolCall,
                    text: String::new(),
                    data: String::new(),
                    mime_type: String::new(),
                    tool_call_id: block.id.clone(),
                    tool_name: block.name.clone(),
                    arguments: block.arguments.clone(),
                    is_error: false,
                    operation_metadata: None,
                    ..Block::default()
                });
            }
            "image" => {
                if role != Role::User {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "image content is incompatible with message role",
                    ));
                }
                let image = Block::image(block.data.clone(), block.mime_type.clone());
                image
                    .validate()
                    .map_err(|_| PiError::invalid("image content is malformed"))?;
                blocks.push(image);
            }
            // Otto writes unsigned assistant reasoning. Signed or redacted
            // thinking from another writer cannot be represented.
            "thinking" => {
                if role != Role::Assistant
                    || block.thinking.is_empty()
                    || !block.thinking_signature.is_empty()
                    || block.redacted.is_some()
                {
                    return Err(PiError::new(
                        PiErrorKind::UnsupportedContent,
                        "Pi message content is not supported by Otto",
                    ));
                }
                blocks.push(Block::reasoning(block.thinking.clone()));
            }
            _ => {
                return Err(PiError::new(
                    PiErrorKind::UnsupportedContent,
                    "Pi message content type is unsupported",
                ));
            }
        }
    }
    Ok(blocks)
}

fn pi_context_text(
    content_text: Option<&str>,
    blocks: &[PiContentBlock],
) -> Result<String, PiError> {
    if let Some(text) = content_text {
        return Ok(text.to_owned());
    }
    let mut text = String::new();
    for block in blocks {
        if block.type_name != "text" || !block.text_signature.is_empty() {
            return Err(PiError::new(
                PiErrorKind::UnsupportedContent,
                "context content must be plain text",
            ));
        }
        text.push_str(&block.text);
    }
    Ok(text)
}

/// Builds the `Role::Context` message every summary and notification uses.
pub fn new_context_message(
    id: &str,
    context_type: &str,
    display: bool,
    text: String,
    created_at: DateTime<Utc>,
    usage: Option<Usage>,
) -> Message {
    Message {
        id: id.to_owned(),
        role: Role::Context,
        blocks: vec![Block::text(text)],
        created_at,
        usage,
        context_type: context_type.to_owned(),
        display,
        ..Message::default()
    }
}

fn custom_context_text(custom_type: &str, text: &str) -> String {
    if custom_type == TASK_NOTIFICATION_CONTEXT_TYPE
        || custom_type == PARENT_MESSAGE_CONTEXT_TYPE
        || custom_type == CAPABILITY_UPDATE_CONTEXT_TYPE
    {
        return text.to_owned();
    }
    format!("[Custom context: {custom_type}]\n{text}")
}

pub(crate) fn optional_pi_usage_to_model(
    usage: Option<&PiUsage>,
    details: Option<&RawValue>,
) -> Result<Option<Usage>, PiError> {
    let Some(usage) = usage else {
        return Ok(None);
    };
    let converted = pi_usage_to_model(Some(usage))?.or(Some(Usage::default()));
    restore_usage_details(converted, usage, details)
}

pub(super) fn pi_assistant_usage_to_model(wire: &PiMessage) -> Result<Option<Usage>, PiError> {
    let converted = pi_usage_to_model(wire.usage.as_ref())?;
    let wire_usage = wire
        .usage
        .as_ref()
        .ok_or_else(|| PiError::invalid("assistant usage is required"))?;
    if let Some(usage) = restore_usage_details(converted, wire_usage, wire.details.as_deref())? {
        return Ok(Some(usage));
    }
    if let Some(details) = decode_pi_otto_details(wire.details.as_deref())?
        && details.usage_present
    {
        return Ok(Some(Usage::default()));
    }
    Ok(None)
}

fn restore_usage_details(
    converted: Option<Usage>,
    wire: &PiUsage,
    details: Option<&RawValue>,
) -> Result<Option<Usage>, PiError> {
    let Some(details) = decode_pi_otto_details(details)? else {
        return Ok(converted);
    };
    let Some(usage) = details.usage else {
        return Ok(converted);
    };
    let encoded = model_usage_to_pi(Some(&usage))?.expect("present usage");
    if (
        encoded.input,
        encoded.output,
        encoded.cache_read,
        encoded.cache_write,
        encoded.cache_write_1h,
    ) != (
        wire.input,
        wire.output,
        wire.cache_read,
        wire.cache_write,
        wire.cache_write_1h,
    ) {
        return Err(PiError::invalid(
            "Otto usage details disagree with Pi usage",
        ));
    }
    Ok(Some(usage))
}

/// Adds `usage` into `total`, saturating instead of overflowing.
pub fn add_resolved_usage(total: Usage, usage: Option<&Usage>) -> Usage {
    let Some(usage) = usage else {
        return total;
    };
    total.saturating_add(*usage)
}

pub(super) fn known_pi_entry_type(entry_type: &str) -> bool {
    matches!(
        entry_type,
        "message"
            | "model_change"
            | "thinking_level_change"
            | "compaction"
            | "branch_summary"
            | "custom"
            | "custom_message"
            | "context_edit"
            | "label"
            | "session_info"
    )
}

pub(super) fn sanitize_warning_type(entry_type: &str) -> String {
    let mut sanitized = String::new();
    for character in entry_type.chars() {
        if sanitized.len() >= MAX_WARNING_TYPE_BYTES {
            break;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            sanitized.push(character);
        } else {
            sanitized.push('?');
        }
    }
    if sanitized.is_empty() {
        return "unknown".into();
    }
    sanitized
}

/// The capability state recorded in an `otto.capabilities` custom entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    /// Sorted, deduplicated tool names.
    pub tools: Vec<String>,
    /// Digest per tool name of its description and parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definitions: Option<BTreeMap<String, String>>,
    /// The Otto package version that wrote the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Decodes `otto.capabilities` data, sorting and deduplicating the tool
/// names; `None` when the data has another shape. Fields absent from older
/// records decode as `None` and unknown fields are ignored.
pub fn decode_capabilities(data: &str) -> Option<Capabilities> {
    let mut capabilities = serde_json::from_str::<Capabilities>(data).ok()?;
    capabilities.tools.sort();
    capabilities.tools.dedup();
    Some(capabilities)
}

/// Encodes a [`Capabilities`] value as `otto.capabilities` data.
pub fn encode_capabilities(capabilities: &Capabilities) -> String {
    serde_json::to_string(capabilities).expect("capabilities serialize")
}

/// Decodes an `otto.runtime` custom entry's `data` field.
pub fn decode_runtime_metadata(raw: Option<&RawValue>) -> Result<RuntimeMetadata, PiError> {
    let bytes = raw.map_or(&b""[..], |value| value.get().as_bytes());
    let object = super::super::codec::decode_object(bytes, "otto.runtime data")?;
    let provider =
        super::super::codec::required_string(&object, "provider", "otto.runtime data.provider")
            .unwrap_or_default();
    if provider.trim().is_empty() {
        return Err(PiError::invalid("otto.runtime provider is required"));
    }
    let model = super::super::codec::required_string(&object, "model", "otto.runtime data.model")
        .unwrap_or_default();
    if model.trim().is_empty() {
        return Err(PiError::invalid("otto.runtime model is required"));
    }
    let profile = super::super::codec::optional_string(
        &object,
        "profile",
        "otto.runtime data.profile",
        false,
    )?;
    Ok(RuntimeMetadata {
        profile: profile.unwrap_or_default(),
        provider,
        model,
    })
}
