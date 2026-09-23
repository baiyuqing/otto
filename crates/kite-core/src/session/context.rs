//! Resolution of a Pi v3 entry tree into Kite's message list, and the
//! model-to-entry direction used when appending.
//!
//! Nothing here touches the filesystem, the clock, or randomness, so the whole
//! module builds for `wasm32-unknown-unknown`.
//!
//! Ownership: every function takes borrowed input and returns owned values.
//! Concurrency: no shared state. Errors: every failure is a [`PiError`],
//! classified by [`PiError::kind`].

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::LazyLock;

use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;
use serde_json::value::RawValue;

use crate::model::{
    Block, BlockType, ContextMetadata, FinishReason, Message, Role, Usage, zero_time,
};

use super::compaction::{
    compaction_aware_path, compaction_has_retained_tail, safe_context_token_count,
};
use super::pi::{
    PiContentBlock, PiCustomMessage, PiEntry, PiMessage, PiKiteDetails, PiUsage,
    decode_pi_kite_details, encode_pi_kite_details,
};
use super::types::{Header, RuntimeMetadata, Snapshot, Warning};
use super::{PiError, PiErrorKind};

/// Context type written for a compaction checkpoint summary.
pub const COMPACTION_CONTEXT_TYPE: &str = "compaction";
/// Context type written for a branch summary.
pub const BRANCH_CONTEXT_TYPE: &str = "branch_summary";
/// Custom entry type carrying Kite's runtime metadata.
pub const KITE_RUNTIME_CUSTOM_TYPE: &str = "kite.runtime";

const MAX_CONTEXT_WARNINGS: usize = 32;
const MAX_WARNING_TYPE_BYTES: usize = 32;

// Kite's own context message types, used for sub-agent notifications. They
// round-trip without the "[Custom context: ...]" decoration applied to custom
// types written by other Pi-compatible tools.
const TASK_NOTIFICATION_CONTEXT_TYPE: &str = "task_notification";
const PARENT_MESSAGE_CONTEXT_TYPE: &str = "parent_message";

static ENTRY_ID_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^[0-9a-f]{8}$").expect("entry id pattern compiles"));

/// True when `id` is a well-formed Pi entry id: eight lowercase hex digits.
pub fn is_pi_entry_id(id: &str) -> bool {
    ENTRY_ID_PATTERN.is_match(id)
}

/// Everything a frontend needs after replaying one session's active branch.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedContext {
    pub messages: Vec<Message>,
    pub runtime: RuntimeMetadata,
    pub usage: Usage,
    pub usage_present: bool,
    pub session_name: String,
    pub thinking_level: String,
}

impl Default for ResolvedContext {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            runtime: RuntimeMetadata::default(),
            usage: Usage::default(),
            usage_present: false,
            session_name: String::new(),
            thinking_level: "off".into(),
        }
    }
}

/// Entry lookup built once per resolution: id to entry and id to file position.
pub struct ContextEntryIndex {
    by_id: HashMap<String, PiEntry>,
    indexes: HashMap<String, usize>,
}

#[derive(Default)]
struct WarningCollector {
    warnings: Vec<Warning>,
    omitted: bool,
}

impl WarningCollector {
    fn add(&mut self, message: String) {
        if self.omitted {
            return;
        }
        if self.warnings.len() < MAX_CONTEXT_WARNINGS {
            self.warnings.push(Warning::new(message));
            return;
        }
        self.warnings[MAX_CONTEXT_WARNINGS - 1] =
            Warning::new("additional session warnings omitted");
        self.omitted = true;
    }
}

/// Replays the entry tree from `leaf_id` back to its root and returns the
/// messages, runtime and token totals in force there.
///
/// Warnings describe entries that were skipped or re-rooted; they are never
/// fatal. An error means the session cannot be represented at all, and no
/// warnings are returned with it.
pub fn build_context(
    entries: &[PiEntry],
    leaf_id: &str,
) -> Result<(ResolvedContext, Vec<Warning>), PiError> {
    let (index, warnings) = index_context_entries(entries)?;
    let mut collector = WarningCollector {
        warnings,
        omitted: false,
    };

    let path = active_context_path(entries, leaf_id, &index)?;

    let mut resolved = ResolvedContext::default();
    let mut latest_runtime: Option<RuntimeMetadata> = None;
    let mut latest_model_change: Option<RuntimeMetadata> = None;
    let mut latest_assistant: Option<RuntimeMetadata> = None;
    for entry in &path {
        match entry.type_name.as_str() {
            "message" => {
                let message = entry
                    .message
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("message payload is required"))?;
                if message.role == "assistant" {
                    latest_assistant = Some(RuntimeMetadata {
                        profile: String::new(),
                        provider: message.provider.clone(),
                        model: message.model.clone(),
                    });
                    if let Some(usage) = pi_assistant_usage_to_model(message)? {
                        resolved.usage = add_resolved_usage(resolved.usage, Some(&usage));
                        resolved.usage_present = true;
                    }
                }
            }
            "model_change" => {
                let change = entry
                    .model_change
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("model_change payload is required"))?;
                latest_model_change = Some(RuntimeMetadata {
                    profile: String::new(),
                    provider: change.provider.clone(),
                    model: change.model_id.clone(),
                });
            }
            "thinking_level_change" => {
                let change = entry
                    .thinking_level_change
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("thinking_level_change payload is required"))?;
                resolved.thinking_level = change.thinking_level.clone();
            }
            "compaction" => {
                let compaction = entry
                    .compaction
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("compaction payload is required"))?;
                let usage = optional_pi_usage_to_model(compaction.usage.as_ref())?;
                resolved.usage = add_resolved_usage(resolved.usage, usage.as_ref());
                if compaction.usage.is_some() {
                    resolved.usage_present = true;
                }
            }
            "branch_summary" => {
                let summary = entry
                    .branch_summary
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("branch_summary payload is required"))?;
                let usage = optional_pi_usage_to_model(summary.usage.as_ref())?;
                resolved.usage = add_resolved_usage(resolved.usage, usage.as_ref());
                if summary.usage.is_some() {
                    resolved.usage_present = true;
                }
            }
            "custom" => {
                let custom = entry
                    .custom
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("custom payload is required"))?;
                if custom.custom_type == KITE_RUNTIME_CUSTOM_TYPE {
                    latest_runtime = Some(decode_runtime_metadata(custom.data.as_deref())?);
                }
            }
            "session_info" => {
                let info = entry
                    .session_info
                    .as_ref()
                    .ok_or_else(|| PiError::invalid("session_info payload is required"))?;
                resolved.session_name = info.name.clone().unwrap_or_default();
            }
            other => {
                if !known_pi_entry_type(other) {
                    collector.add(format!(
                        "ignored unknown active session entry {} of type {}",
                        entry.id,
                        sanitize_warning_type(other)
                    ));
                }
            }
        }
    }

    if let Some(runtime) = latest_runtime.or(latest_model_change).or(latest_assistant) {
        resolved.runtime = runtime;
    }

    for entry in compaction_aware_path(&path)? {
        resolved
            .messages
            .extend(pi_entry_to_context_messages(&entry)?);
    }
    pending_tool_calls(&resolved.messages)?;
    Ok((resolved, collector.warnings))
}

pub fn index_context_entries(
    entries: &[PiEntry],
) -> Result<(ContextEntryIndex, Vec<Warning>), PiError> {
    let mut index = ContextEntryIndex {
        by_id: HashMap::with_capacity(entries.len()),
        indexes: HashMap::with_capacity(entries.len()),
    };
    let mut collector = WarningCollector::default();
    for (position, entry) in entries.iter().enumerate() {
        validate_context_entry_base(entry)
            .map_err(|error| error.context(format!("session entry {}", position + 1)))?;
        if index.by_id.contains_key(&entry.id) {
            return Err(PiError::invalid("duplicate entry id")
                .context(format!("session entry {}", position + 1)));
        }
        index.by_id.insert(entry.id.clone(), entry.clone());
        index.indexes.insert(entry.id.clone(), position);
    }

    let mut root_count = 0usize;
    for (position, entry) in entries.iter().enumerate() {
        let Some(parent_id) = entry.parent_id.as_ref() else {
            root_count += 1;
            continue;
        };
        let Some(parent_position) = index.indexes.get(parent_id).copied() else {
            root_count += 1;
            collector.add(format!(
                "session entry {} has missing parent {parent_id}; treating entry as a root",
                entry.id
            ));
            continue;
        };
        if *parent_id == entry.id {
            return Err(
                PiError::invalid("parent cycle").context(format!("session entry {}", position + 1))
            );
        }
        if parent_position >= position {
            return Err(
                PiError::invalid("parent id does not reference a prior entry")
                    .context(format!("session entry {}", position + 1)),
            );
        }
    }
    if root_count > 1 {
        collector.add("session contains multiple roots".into());
    }
    Ok((index, collector.warnings))
}

fn validate_context_entry_base(entry: &PiEntry) -> Result<(), PiError> {
    if entry.type_name.trim().is_empty() {
        return Err(PiError::invalid("entry type is required"));
    }
    if !is_pi_entry_id(&entry.id) {
        return Err(PiError::invalid(
            "entry id must be eight lowercase hexadecimal characters",
        ));
    }
    if parse_rfc3339(&entry.timestamp).is_none() {
        return Err(PiError::invalid("entry timestamp is invalid"));
    }
    if let Some(parent_id) = entry.parent_id.as_ref()
        && !is_pi_entry_id(parent_id)
    {
        return Err(PiError::invalid(
            "parent id must be eight lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

pub fn active_context_path(
    entries: &[PiEntry],
    leaf_id: &str,
    index: &ContextEntryIndex,
) -> Result<Vec<PiEntry>, PiError> {
    if entries.is_empty() {
        if !leaf_id.is_empty() {
            return Err(PiError::invalid("active leaf does not exist"));
        }
        return Ok(Vec::new());
    }
    let mut current = index
        .by_id
        .get(leaf_id)
        .ok_or_else(|| PiError::invalid("active leaf does not exist"))?;
    let mut visited = HashSet::new();
    let mut path = Vec::new();
    loop {
        if !visited.insert(current.id.clone()) {
            return Err(PiError::invalid("active branch contains a cycle"));
        }
        path.push(current.clone());
        let Some(parent_id) = current.parent_id.as_ref() else {
            break;
        };
        let Some(parent) = index.by_id.get(parent_id) else {
            break;
        };
        current = parent;
    }
    path.reverse();
    Ok(path)
}

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
            let usage = optional_pi_usage_to_model(compaction.usage.as_ref())?;
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
            let usage = optional_pi_usage_to_model(branch.usage.as_ref())?;
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
            if let Some(details) = decode_pi_kite_details(custom.details.as_deref())?
                && !details.task_id.is_empty()
            {
                message.context_metadata = Some(ContextMetadata {
                    task_id: details.task_id,
                });
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
                "Pi message role is not supported by Kite",
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
            "thinking" => {
                return Err(PiError::new(
                    PiErrorKind::UnsupportedContent,
                    "Pi message content is not supported by Kite",
                ));
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
    if custom_type == TASK_NOTIFICATION_CONTEXT_TYPE || custom_type == PARENT_MESSAGE_CONTEXT_TYPE {
        return text.to_owned();
    }
    format!("[Custom context: {custom_type}]\n{text}")
}

pub(crate) fn optional_pi_usage_to_model(
    usage: Option<&PiUsage>,
) -> Result<Option<Usage>, PiError> {
    let Some(usage) = usage else {
        return Ok(None);
    };
    match pi_usage_to_model(Some(usage))? {
        Some(converted) => Ok(Some(converted)),
        None => Ok(Some(Usage::default())),
    }
}

fn pi_assistant_usage_to_model(wire: &PiMessage) -> Result<Option<Usage>, PiError> {
    if let Some(usage) = pi_usage_to_model(wire.usage.as_ref())? {
        return Ok(Some(usage));
    }
    if let Some(details) = decode_pi_kite_details(wire.details.as_deref())?
        && details.usage_present
    {
        return Ok(Some(Usage::default()));
    }
    Ok(None)
}

/// Adds `usage` into `total`, saturating instead of overflowing.
pub fn add_resolved_usage(mut total: Usage, usage: Option<&Usage>) -> Usage {
    let Some(usage) = usage else {
        return total;
    };
    total.input_tokens = saturating_usage_add(total.input_tokens, usage.input_tokens);
    total.output_tokens = saturating_usage_add(total.output_tokens, usage.output_tokens);
    total.cached_input_tokens =
        saturating_usage_add(total.cached_input_tokens, usage.cached_input_tokens);
    total
}

fn saturating_usage_add(total: i64, delta: i64) -> i64 {
    if delta <= 0 {
        return total;
    }
    total.saturating_add(delta)
}

fn known_pi_entry_type(entry_type: &str) -> bool {
    matches!(
        entry_type,
        "message"
            | "model_change"
            | "thinking_level_change"
            | "compaction"
            | "branch_summary"
            | "custom"
            | "custom_message"
            | "label"
            | "session_info"
    )
}

fn sanitize_warning_type(entry_type: &str) -> String {
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

/// Decodes an `kite.runtime` custom entry's `data` field.
pub fn decode_runtime_metadata(raw: Option<&RawValue>) -> Result<RuntimeMetadata, PiError> {
    let bytes = raw.map_or(&b""[..], |value| value.get().as_bytes());
    let object = super::codec::decode_object(bytes, "kite.runtime data")?;
    let provider = super::codec::required_string(&object, "provider", "kite.runtime data.provider")
        .unwrap_or_default();
    if provider.trim().is_empty() {
        return Err(PiError::invalid("kite.runtime provider is required"));
    }
    let model = super::codec::required_string(&object, "model", "kite.runtime data.model")
        .unwrap_or_default();
    if model.trim().is_empty() {
        return Err(PiError::invalid("kite.runtime model is required"));
    }
    let profile =
        super::codec::optional_string(&object, "profile", "kite.runtime data.profile", false)?;
    Ok(RuntimeMetadata {
        profile: profile.unwrap_or_default(),
        provider,
        model,
    })
}

// ---------------------------------------------------------------------------
// model -> Pi
// ---------------------------------------------------------------------------

/// Encodes one Kite message as the Pi entry that will be appended, and returns
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
    let (_, content_blocks) = super::codec::decode_content(&content, "message content", false)?;
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
        }
        Role::Assistant => {
            let stop_reason = model_finish_reason_to_pi(message.finish_reason.clone())?;
            validate_assistant_tool_finish(&message.blocks, message.finish_reason.clone())?;
            let usage = model_usage_to_pi(message.usage.as_ref())?;
            if message.usage == Some(Usage::default()) {
                wire.details = Some(encode_pi_kite_details(&PiKiteDetails {
                    task_id: String::new(),
                    usage_present: true,
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
    if let Some(metadata) = message.context_metadata.as_ref() {
        custom.details = Some(encode_pi_kite_details(&PiKiteDetails {
            task_id: metadata.task_id.clone(),
            usage_present: false,
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
    // Usage is dropped: piCustomMessage has no usage field in the Pi v3
    // format, so a notification's token cost is not persisted.
    let persisted = Message {
        id: entry_id.to_owned(),
        role: Role::Context,
        blocks: vec![Block::text(content)],
        created_at: message.created_at,
        context_type: message.context_type.clone(),
        display: message.display,
        context_metadata: message.context_metadata.clone(),
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

/// Converts Kite usage into the Pi wire shape. `None` becomes an all-zero usage
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

fn pi_stop_reason_to_model(reason: &str) -> Result<FinishReason, PiError> {
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
/// `details.kite.usagePresent` can tell the two apart.
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

/// Walks a message list and returns the tool calls that are still unanswered.
/// Any ordering violation is an error, so this doubles as the sequence check.
pub fn pending_tool_calls(messages: &[Message]) -> Result<Vec<Block>, PiError> {
    let mut pending: HashMap<String, Block> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    for message in messages {
        match message.role {
            Role::Assistant => {
                if !pending.is_empty() {
                    return Err(PiError::invalid(
                        "unresolved tool calls must be followed by tool results",
                    ));
                }
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolCall {
                        continue;
                    }
                    if !seen.insert(block.tool_call_id.clone()) {
                        return Err(PiError::invalid("duplicate tool-call id"));
                    }
                    pending.insert(block.tool_call_id.clone(), block.clone());
                    order.push(block.tool_call_id.clone());
                }
            }
            Role::Tool => {
                if message.blocks.is_empty() {
                    return Err(PiError::invalid("tool message must contain a tool result"));
                }
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolResult {
                        return Err(PiError::invalid("tool message contains a non-result block"));
                    }
                    let call = pending
                        .get(&block.tool_call_id)
                        .ok_or_else(|| PiError::invalid("tool result has no pending call"))?;
                    if call.tool_name != block.tool_name {
                        return Err(PiError::invalid(
                            "tool result name does not match pending call",
                        ));
                    }
                    pending.remove(&block.tool_call_id);
                }
            }
            _ => {
                if !pending.is_empty() {
                    return Err(PiError::invalid(
                        "unresolved tool calls must be followed by tool results",
                    ));
                }
            }
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|id| pending.remove(&id))
        .collect())
}

/// True when `arguments` is a JSON object, the only tool-call argument shape
/// Kite persists.
pub fn valid_tool_arguments(arguments: Option<&RawValue>) -> bool {
    let Some(arguments) = arguments else {
        return false;
    };
    let trimmed = arguments.get().trim();
    trimmed.starts_with('{')
        && trimmed.ends_with('}')
        && serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(trimmed).is_ok()
}

// ---------------------------------------------------------------------------
// timestamps
// ---------------------------------------------------------------------------

/// Parses the RFC 3339 timestamps stored sessions use, with nanosecond
/// precision.
pub fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value).ok().map(Into::into)
}

/// Formats as RFC 3339 with nanosecond precision: fractional seconds are
/// written only when nonzero, with trailing zeros removed.
pub fn format_rfc3339_nano(timestamp: DateTime<Utc>) -> String {
    let formatted = timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let Some((head, rest)) = formatted.split_once('.') else {
        return formatted;
    };
    let (fraction, suffix) = rest.split_at(rest.len() - 1);
    let trimmed = fraction.trim_end_matches('0');
    if trimmed.is_empty() {
        format!("{head}{suffix}")
    } else {
        format!("{head}.{trimmed}{suffix}")
    }
}

/// Formats a timestamp for persistence, rejecting the zero value and anything
/// that does not survive a format/parse round trip.
pub fn format_persisted_timestamp(
    timestamp: DateTime<Utc>,
    subject: &str,
) -> Result<String, PiError> {
    if timestamp == zero_time() {
        return Err(PiError::invalid(format!("{subject} timestamp is required")));
    }
    let formatted = format_rfc3339_nano(timestamp);
    match parse_rfc3339(&formatted) {
        Some(round_tripped)
            if round_tripped == timestamp && format_rfc3339_nano(round_tripped) == formatted =>
        {
            Ok(formatted)
        }
        _ => Err(PiError::invalid(format!(
            "{subject} timestamp is outside the RFC3339Nano range"
        ))),
    }
}

// ---------------------------------------------------------------------------
// snapshot and preview
// ---------------------------------------------------------------------------

/// Builds the display snapshot from a session's resolved state.
pub fn snapshot_from_state(
    aggregate_usage: Usage,
    aggregate_usage_present: bool,
    messages: &[Message],
    latest_compaction: Option<&super::types::CompactionMetadata>,
) -> Snapshot {
    let (context_input_tokens, present, pending) =
        latest_context_input_tokens(messages, latest_compaction);
    Snapshot {
        aggregate_usage,
        aggregate_usage_present,
        context_input_tokens,
        context_input_tokens_present: present,
        context_input_tokens_pending: pending,
    }
}

fn latest_context_input_tokens(
    messages: &[Message],
    latest_compaction: Option<&super::types::CompactionMetadata>,
) -> (i64, bool, bool) {
    let mut start = 0usize;
    if let Some(compaction) = latest_compaction {
        if compaction.first_post_checkpoint_message_id.is_empty() {
            return (0, false, true);
        }
        match messages
            .iter()
            .position(|message| message.id == compaction.first_post_checkpoint_message_id)
        {
            Some(index) => start = index,
            None => return (0, false, true),
        }
    }
    for message in messages[start..].iter().rev() {
        if message.role != Role::Assistant {
            continue;
        }
        return match message.usage {
            Some(usage) if usage.input_tokens > 0 => (usage.input_tokens, true, false),
            _ => (0, false, false),
        };
    }
    if latest_compaction.is_some() {
        return (0, false, true);
    }
    (0, false, false)
}

const MAX_SESSION_PREVIEW_RUNES: usize = 120;

/// The text of the most recent user message, or empty when there is none.
pub fn last_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(Message::text)
        .unwrap_or_default()
}

/// Collapses whitespace, escapes control characters and truncates to 120
/// display units, appending "..." when anything was dropped.
pub fn preview_text(text: &str) -> String {
    let mut builder = String::with_capacity(MAX_SESSION_PREVIEW_RUNES * 6 + 3);
    let mut used = 0usize;
    let mut wrote_content = false;
    let mut pending_space = false;
    let mut truncated = false;

    for character in text.chars() {
        if character.is_whitespace() {
            pending_space = wrote_content;
            continue;
        }
        let width = preview_rune_width(character);
        let needed = width + usize::from(pending_space);
        if used + needed > MAX_SESSION_PREVIEW_RUNES {
            truncated = true;
            break;
        }
        if pending_space {
            builder.push(' ');
            used += 1;
            pending_space = false;
        }
        write_preview_rune(&mut builder, character);
        used += width;
        wrote_content = true;
    }

    if !wrote_content {
        return String::new();
    }
    if truncated {
        builder.push_str("...");
    }
    builder
}

fn preview_rune_width(character: char) -> usize {
    let value = character as u32;
    if is_go_control(character) {
        if value < 0x100 {
            return 4;
        }
        return 2 + hex_width(value).max(4);
    }
    1
}

fn write_preview_rune(builder: &mut String, character: char) {
    let value = character as u32;
    if is_go_control(character) {
        if value < 0x100 {
            let _ = write!(builder, "\\x{value:02x}");
        } else {
            let width = hex_width(value).max(4);
            let _ = write!(builder, "\\u{value:0width$x}");
        }
        return;
    }
    builder.push(character);
}

/// The C0 and C1 control ranges only.
fn is_go_control(character: char) -> bool {
    let value = character as u32;
    value < 0x20 || (0x7f..=0x9f).contains(&value)
}

fn hex_width(mut value: u32) -> usize {
    let mut width = 1;
    while value >= 16 {
        value /= 16;
        width += 1;
    }
    width
}

fn to_raw_value<T: serde::Serialize>(value: &T) -> Result<Box<RawValue>, PiError> {
    let encoded = serde_json::to_string(value)
        .map_err(|error| PiError::other(format!("encode context message content: {error}")))?;
    RawValue::from_string(encoded)
        .map_err(|error| PiError::other(format!("encode context message content: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BlockType, Role};
    use crate::session::codec::decode_pi_entry;
    use crate::session::codec::tests::read_pi_fixture;
    use crate::session::pi::{
        PiBranchSummary, PiCompaction, PiCustom, PiModelChange, PiSessionInfo,
    };

    macro_rules! test {
        ($name:ident $body:block) => {
            #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
            #[cfg_attr(not(target_arch = "wasm32"), test)]
            fn $name() $body
        };
    }

    fn entry(entry_type: &str, id: &str, parent_id: Option<&str>) -> PiEntry {
        PiEntry::new(
            entry_type,
            id,
            parent_id.map(str::to_owned),
            "2026-08-27T12:00:00Z",
        )
    }

    fn user_entry(id: &str, parent_id: Option<&str>, text: &str) -> PiEntry {
        let mut built = entry("message", id, parent_id);
        built.message = Some(Box::new(PiMessage {
            role: "user".into(),
            content_text: Some(text.into()),
            timestamp: 1,
            ..PiMessage::default()
        }));
        built
    }

    fn pi_usage(input: i64, output: i64) -> PiUsage {
        PiUsage {
            input,
            output,
            total_tokens: input + output,
            ..PiUsage::default()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn assistant_entry(
        id: &str,
        parent_id: Option<&str>,
        text: &str,
        provider: &str,
        model_id: &str,
        input: i64,
        output: i64,
        stop_reason: &str,
    ) -> PiEntry {
        let mut built = entry("message", id, parent_id);
        built.message = Some(Box::new(PiMessage {
            role: "assistant".into(),
            provider: provider.into(),
            model: model_id.into(),
            stop_reason: stop_reason.into(),
            content_blocks: vec![PiContentBlock {
                type_name: "text".into(),
                text: text.into(),
                ..PiContentBlock::default()
            }],
            usage: Some(pi_usage(input, output)),
            timestamp: 1,
            ..PiMessage::default()
        }));
        built
    }

    fn tool_call_entry(id: &str, parent_id: Option<&str>, calls: &[(&str, &str)]) -> PiEntry {
        let mut built = entry("message", id, parent_id);
        let blocks = calls
            .iter()
            .map(|(call_id, name)| PiContentBlock {
                type_name: "toolCall".into(),
                id: (*call_id).into(),
                name: (*name).into(),
                arguments: Some(RawValue::from_string("{}".into()).expect("valid JSON")),
                ..PiContentBlock::default()
            })
            .collect();
        built.message = Some(Box::new(PiMessage {
            role: "assistant".into(),
            provider: "openai-compatible".into(),
            model: "model".into(),
            stop_reason: "toolUse".into(),
            usage: Some(pi_usage(1, 1)),
            timestamp: 1,
            content_blocks: blocks,
            ..PiMessage::default()
        }));
        built
    }

    fn tool_result_entry(
        id: &str,
        parent_id: Option<&str>,
        call_id: &str,
        tool_name: &str,
        text: &str,
    ) -> PiEntry {
        let mut built = entry("message", id, parent_id);
        built.message = Some(Box::new(PiMessage {
            role: "toolResult".into(),
            content_text: Some(text.into()),
            tool_call_id: call_id.into(),
            tool_name: tool_name.into(),
            is_error: Some(false),
            timestamp: 1,
            ..PiMessage::default()
        }));
        built
    }

    fn single_tool_call(root_id: &str, assistant_id: &str) -> (PiEntry, PiEntry) {
        let root = user_entry(root_id, None, "root");
        let assistant = tool_call_entry(assistant_id, Some(root_id), &[("call-1", "read")]);
        (root, assistant)
    }

    fn runtime_entry(id: &str, parent_id: Option<&str>, metadata: &RuntimeMetadata) -> PiEntry {
        let mut built = entry("custom", id, parent_id);
        built.custom = Some(PiCustom {
            custom_type: KITE_RUNTIME_CUSTOM_TYPE.into(),
            data: Some(to_raw_value(metadata).expect("encode runtime metadata")),
        });
        built
    }

    fn decode_entry(raw: &str) -> PiEntry {
        decode_pi_entry(raw.as_bytes()).expect("decode entry")
    }

    fn texts(messages: &[Message]) -> Vec<String> {
        messages.iter().map(Message::text).collect()
    }

    fn context_from_fixture(name: &str) -> ResolvedContext {
        let file = read_pi_fixture(name);
        let leaf = file.entries.last().expect("fixture entries").id.clone();
        let (context, warnings) = build_context(&file.entries, &leaf).expect("build context");
        assert!(warnings.is_empty(), "warnings = {warnings:?}");
        context
    }

    fn assert_warning_contains(warnings: &[Warning], text: &str) {
        assert!(
            warnings
                .iter()
                .any(|warning| warning.message.contains(text)),
            "warnings = {warnings:?}, want text {text:?}"
        );
    }

    test!(build_context_uses_only_the_active_root_to_leaf_path {
        let file = read_pi_fixture("tree.jsonl");
        let leaf = file.entries.last().expect("entries").id.clone();
        let (context, warnings) = build_context(&file.entries, &leaf).expect("build context");
        assert!(warnings.is_empty(), "warnings = {warnings:?}");
        assert_eq!(
            texts(&context.messages),
            [
                "root",
                "[Branch summary]\ninactive work was abandoned",
                "active branch",
            ]
        );
        for message in &context.messages {
            let text = message.text();
            assert!(
                !text.contains("inactive branch") && !text.contains("inactive custom"),
                "inactive branch leaked into context: {:?}",
                context.messages
            );
        }
    });

    test!(build_context_uses_the_retained_tail_checkpoint {
        let context = context_from_fixture("compacted.jsonl");
        assert_eq!(
            texts(&context.messages),
            ["[Compaction summary]\nsummary", "retained", "after"]
        );
        let checkpoint = &context.messages[0];
        assert_eq!(checkpoint.role, Role::Context);
        assert!(checkpoint.display);
        assert_eq!(checkpoint.context_type, "compaction");
        assert_eq!(context.usage.input_tokens, 12);
        assert_eq!(context.usage.output_tokens, 4);
    });

    test!(build_context_prefers_a_real_first_kept_entry_in_the_dual_form {
        let context = context_from_fixture("compaction-dual-form.jsonl");
        assert_eq!(
            texts(&context.messages),
            [
                "[Compaction summary]\ndual summary",
                "real retained path",
                "after dual checkpoint",
            ]
        );
        assert_eq!(context.messages[0].context_tokens_before, 140);
        for message in &context.messages {
            assert!(
                !message.id.contains("-tail-") && !message.text().contains("synthetic must lose"),
                "synthetic retained tail won over the real active path: {:?}",
                context.messages
            );
        }
    });

    test!(build_context_falls_back_to_the_retained_tail {
        let context = context_from_fixture("compaction-retained-tail-only.jsonl");
        assert_eq!(
            texts(&context.messages),
            [
                "[Compaction summary]\nretained-tail summary",
                "synthetic retained tail",
            ]
        );
        assert_eq!(context.messages[0].context_tokens_before, 120);
    });

    test!(build_context_carries_tokens_before_from_a_compaction_summary_message {
        let built = decode_entry(
            r#"{"type":"message","id":"cf000001","parentId":null,"timestamp":"2026-08-28T10:00:00Z","message":{"role":"compactionSummary","summary":"message summary","tokensBefore":321,"timestamp":1787911200000}}"#,
        );
        let leaf = built.id.clone();
        let (context, _) = build_context(&[built], &leaf).expect("build context");
        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.messages[0].context_tokens_before, 321);
    });

    test!(build_context_uses_the_legacy_first_kept_entry_id {
        let file = read_pi_fixture("compacted.jsonl");
        let (context, warnings) = build_context(&file.entries, "c0000003").expect("build context");
        assert!(warnings.is_empty(), "warnings = {warnings:?}");
        assert_eq!(
            texts(&context.messages),
            ["[Compaction summary]\nlegacy summary", "legacy retained"]
        );

        let mut missing = file.entries[2].clone();
        missing
            .compaction
            .as_mut()
            .expect("compaction payload")
            .first_kept_entry_id = Some("ffffffff".into());
        let leaf = missing.id.clone();
        let entries = vec![file.entries[0].clone(), file.entries[1].clone(), missing];
        let error = build_context(&entries, &leaf).expect_err("missing anchor accepted");
        assert_eq!(error.kind(), PiErrorKind::Invalid);
    });

    test!(build_context_keeps_an_image_on_the_active_branch_only {
        let root = user_entry("10000001", None, "root");
        let image = decode_entry(
            r#"{"type":"message","id":"10000002","parentId":"10000001","timestamp":"2026-08-27T12:00:02Z","message":{"role":"user","content":[{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}],"timestamp":2}}"#,
        );
        let text = user_entry("10000003", Some("10000001"), "text leaf");
        let entries = vec![root, image, text];

        let (context, _) = build_context(&entries, "10000002").expect("build image context");
        assert_eq!(context.messages.len(), 2);
        assert_eq!(context.messages[1].blocks[0].block_type, BlockType::Image);
        assert_eq!(context.messages[1].blocks[0].mime_type, "image/png");
        let (context, _) = build_context(&entries, "10000003").expect("build context");
        assert_eq!(texts(&context.messages), ["root", "text leaf"]);
    });

    test!(build_context_rejects_an_unsupported_message_on_the_active_branch_only {
        let bash = decode_entry(
            r#"{"type":"message","id":"20000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"bashExecution","command":"pwd","output":"/workspace","exitCode":0,"cancelled":false,"truncated":false,"timestamp":1}}"#,
        );
        let text = user_entry("20000002", None, "supported root");
        let entries = vec![bash, text];

        let error = build_context(&entries, "20000001").expect_err("bash accepted");
        assert_eq!(error.kind(), PiErrorKind::UnsupportedContent);
        build_context(&entries, "20000002").expect("build context");
    });

    test!(build_context_rejects_provider_required_content_on_the_active_branch_only {
        let provider_specific = decode_entry(
            r#"{"type":"message","id":"21000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"README.md"},"thoughtSignature":"provider-state"}],"api":"provider-api","provider":"provider","model":"model","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":1}}"#,
        );
        let supported = user_entry("21000002", None, "supported root");
        let entries = vec![provider_specific, supported];

        let error = build_context(&entries, "21000001").expect_err("provider content accepted");
        assert_eq!(error.kind(), PiErrorKind::UnsupportedContent);
        build_context(&entries, "21000002").expect("build context");
    });

    test!(build_context_applies_runtime_precedence {
        let file = read_pi_fixture("tree.jsonl");
        let leaf = file.entries.last().expect("entries").id.clone();
        let (context, _) = build_context(&file.entries, &leaf).expect("build context");
        assert_eq!(
            context.runtime,
            RuntimeMetadata {
                profile: "default".into(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
            }
        );
        assert_eq!(context.thinking_level, "high");
        assert_eq!(context.session_name, "Fixture tree");

        let root = user_entry("30000001", None, "root");
        let assistant = assistant_entry(
            "30000002",
            Some("30000001"),
            "assistant",
            "assistant-provider",
            "assistant-model",
            3,
            2,
            "stop",
        );
        let mut model_change = entry("model_change", "30000003", Some("30000002"));
        model_change.model_change = Some(PiModelChange {
            provider: "changed-provider".into(),
            model_id: "changed-model".into(),
        });
        let latest = assistant_entry(
            "30000004",
            Some("30000003"),
            "latest",
            "later-provider",
            "later-model",
            5,
            4,
            "stop",
        );

        let entries = vec![root.clone(), assistant.clone(), model_change, latest];
        let (context, _) = build_context(&entries, "30000004").expect("build context");
        assert_eq!(
            context.runtime,
            RuntimeMetadata {
                profile: String::new(),
                provider: "changed-provider".into(),
                model: "changed-model".into(),
            }
        );

        let (context, _) =
            build_context(&[root, assistant], "30000002").expect("build context");
        assert_eq!(
            context.runtime,
            RuntimeMetadata {
                profile: String::new(),
                provider: "assistant-provider".into(),
                model: "assistant-model".into(),
            }
        );
    });

    test!(build_context_uses_the_latest_active_kite_runtime {
        let first = runtime_entry(
            "31000001",
            None,
            &RuntimeMetadata {
                profile: "first".into(),
                provider: "openai-compatible".into(),
                model: "one".into(),
            },
        );
        let second = runtime_entry(
            "31000002",
            Some("31000001"),
            &RuntimeMetadata {
                profile: "second".into(),
                provider: "openai-compatible".into(),
                model: "two".into(),
            },
        );
        let mut change = entry("model_change", "31000003", Some("31000002"));
        change.model_change = Some(PiModelChange {
            provider: "ignored".into(),
            model_id: "ignored".into(),
        });

        let (context, _) =
            build_context(&[first, second, change], "31000003").expect("build context");
        assert_eq!(
            context.runtime,
            RuntimeMetadata {
                profile: "second".into(),
                provider: "openai-compatible".into(),
                model: "two".into(),
            }
        );
    });

    test!(build_context_frames_branch_and_custom_messages {
        let mut branch = entry("branch_summary", "40000001", None);
        branch.branch_summary = Some(Box::new(PiBranchSummary {
            from_id: "ffffffff".into(),
            summary: "abandoned work".into(),
            usage: Some(PiUsage {
                input: 2,
                output: 1,
                cache_read: 3,
                cache_write: 4,
                total_tokens: 10,
                ..PiUsage::default()
            }),
            ..PiBranchSummary::default()
        }));
        let mut hidden = entry("custom_message", "40000002", Some("40000001"));
        hidden.custom_message = Some(Box::new(PiCustomMessage {
            custom_type: "hidden.fixture".into(),
            content_text: Some("secret context".into()),
            display: false,
            ..PiCustomMessage::default()
        }));
        let mut visible = entry("custom_message", "40000003", Some("40000002"));
        visible.custom_message = Some(Box::new(PiCustomMessage {
            custom_type: "visible.fixture".into(),
            content_text: Some("shown context".into()),
            display: true,
            ..PiCustomMessage::default()
        }));

        let (context, warnings) =
            build_context(&[branch, hidden, visible], "40000003").expect("build context");
        assert!(warnings.is_empty(), "warnings = {warnings:?}");
        assert_eq!(
            texts(&context.messages),
            [
                "[Branch summary]\nabandoned work",
                "[Custom context: hidden.fixture]\nsecret context",
                "[Custom context: visible.fixture]\nshown context",
            ]
        );
        assert_eq!(context.messages[0].context_type, "branch_summary");
        assert!(context.messages[0].display);
        assert!(!context.messages[1].display);
        assert!(context.messages[2].display);
        assert_eq!(
            context.usage,
            Usage {
                input_tokens: 9,
                output_tokens: 1,
                cached_input_tokens: 3,
            }
        );
    });

    test!(build_context_supports_custom_agent_messages {
        let built = decode_entry(
            r#"{"type":"message","id":"41000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"custom","customType":"agent.fixture","content":"injected","display":false,"timestamp":1}}"#,
        );
        let (context, _) = build_context(&[built], "41000001").expect("build context");
        assert_eq!(
            texts(&context.messages),
            ["[Custom context: agent.fixture]\ninjected"]
        );
        assert!(!context.messages[0].display);
    });

    test!(build_context_warns_for_an_orphan_root_and_for_multiple_roots {
        let orphan = user_entry("50000001", Some("ffffffff"), "orphan");
        let (context, warnings) = build_context(&[orphan], "50000001").expect("build context");
        assert_eq!(texts(&context.messages), ["orphan"]);
        assert_warning_contains(&warnings, "50000001");

        let first = user_entry("50000002", None, "first root");
        let second = user_entry("50000003", None, "second root");
        let (context, warnings) =
            build_context(&[first, second], "50000003").expect("build context");
        assert_eq!(texts(&context.messages), ["second root"]);
        assert_warning_contains(&warnings, "multiple roots");
    });

    test!(build_context_warns_and_ignores_an_unknown_active_entry {
        let root = user_entry("60000001", None, "root");
        let unknown = entry(
            "future_entry_with_untrusted_\u{1b}[31m_type",
            "60000002",
            Some("60000001"),
        );
        let leaf = user_entry("60000003", Some("60000002"), "leaf");

        let (context, warnings) =
            build_context(&[root.clone(), unknown, leaf], "60000003").expect("build context");
        assert_eq!(texts(&context.messages), ["root", "leaf"]);
        assert_warning_contains(&warnings, "60000002");
        for warning in &warnings {
            assert!(
                !warning.message.contains('\u{1b}') && warning.message.len() <= 256,
                "unsafe or unbounded warning = {:?}",
                warning.message
            );
        }

        // The two explicit roots produce exactly the multiple-root warning; the
        // inactive unknown entry must not add another.
        let inactive = entry("future_entry", "60000004", None);
        let (_, warnings) =
            build_context(&[inactive, root], "60000001").expect("build context");
        assert_eq!(warnings.len(), 1, "warnings = {warnings:?}");
    });

    test!(build_context_preserves_tool_call_result_pairing {
        let context = context_from_fixture("linear.jsonl");
        assert_eq!(context.messages.len(), 4);
        let call = &context.messages[1].blocks[1];
        let result = &context.messages[2].blocks[0];
        assert_eq!(call.block_type, BlockType::ToolCall);
        assert_eq!(result.block_type, BlockType::ToolResult);
        assert_eq!(call.tool_call_id, result.tool_call_id);
        assert_eq!(call.tool_name, result.tool_name);
    });

    test!(build_context_allows_sibling_tool_results_before_later_context {
        let root = user_entry("61000001", None, "root");
        let assistant = tool_call_entry(
            "61000002",
            Some("61000001"),
            &[("call-1", "read"), ("call-2", "write")],
        );
        let second_result =
            tool_result_entry("61000003", Some("61000002"), "call-2", "write", "second");
        let first_result =
            tool_result_entry("61000004", Some("61000003"), "call-1", "read", "first");
        let mut visible = entry("custom_message", "61000005", Some("61000004"));
        visible.custom_message = Some(Box::new(PiCustomMessage {
            custom_type: "visible.fixture".into(),
            content_text: Some("after tools".into()),
            display: true,
            ..PiCustomMessage::default()
        }));
        let user = user_entry("61000006", Some("61000005"), "continue");

        let entries = vec![
            root,
            assistant,
            second_result,
            first_result,
            visible,
            user,
        ];
        let (context, _) = build_context(&entries, "61000006").expect("build context");
        assert_eq!(
            texts(&context.messages),
            [
                "root",
                "",
                "",
                "",
                "[Custom context: visible.fixture]\nafter tools",
                "continue",
            ]
        );
        assert_eq!(context.messages[2].blocks[0].text, "second");
        assert_eq!(context.messages[3].blocks[0].text, "first");
    });

    test!(build_context_rejects_invalid_tool_result_ordering {
        let unmatched = {
            let root = user_entry("62000001", None, "root");
            let result =
                tool_result_entry("62000002", Some("62000001"), "missing", "read", "result");
            (vec![root, result], "62000002")
        };
        let duplicate_result = {
            let (root, assistant) = single_tool_call("62000003", "62000004");
            let result =
                tool_result_entry("62000005", Some("62000004"), "call-1", "read", "result");
            let duplicate = tool_result_entry(
                "62000006",
                Some("62000005"),
                "call-1",
                "read",
                "duplicate",
            );
            (vec![root, assistant, result, duplicate], "62000006")
        };
        let mismatched_name = {
            let (root, assistant) = single_tool_call("62000014", "62000015");
            let result =
                tool_result_entry("62000016", Some("62000015"), "call-1", "write", "result");
            (vec![root, assistant, result], "62000016")
        };
        let duplicate_call = {
            let root = user_entry("62000017", None, "root");
            let assistant = tool_call_entry(
                "62000018",
                Some("62000017"),
                &[("call-1", "read"), ("call-1", "read")],
            );
            (vec![root, assistant], "62000018")
        };
        let interposed_context = {
            let root = user_entry("62000007", None, "root");
            let assistant = tool_call_entry(
                "62000008",
                Some("62000007"),
                &[("call-1", "read"), ("call-2", "write")],
            );
            let result =
                tool_result_entry("62000009", Some("62000008"), "call-1", "read", "first");
            let mut interposed = entry("custom_message", "6200000a", Some("62000009"));
            interposed.custom_message = Some(Box::new(PiCustomMessage {
                custom_type: "provider-visible".into(),
                content_text: Some("not yet".into()),
                display: false,
                ..PiCustomMessage::default()
            }));
            let second =
                tool_result_entry("6200000b", Some("6200000a"), "call-2", "write", "second");
            (
                vec![root, assistant, result, interposed, second],
                "6200000b",
            )
        };
        let interposed_user = {
            let (root, assistant) = single_tool_call("6200000c", "6200000d");
            let user = user_entry("6200000e", Some("6200000d"), "too soon");
            let result =
                tool_result_entry("6200000f", Some("6200000e"), "call-1", "read", "result");
            (vec![root, assistant, user, result], "6200000f")
        };
        let interposed_assistant = {
            let (root, assistant) = single_tool_call("62000010", "62000011");
            let interposed = assistant_entry(
                "62000012",
                Some("62000011"),
                "too soon",
                "openai-compatible",
                "model",
                1,
                1,
                "stop",
            );
            let result =
                tool_result_entry("62000013", Some("62000012"), "call-1", "read", "result");
            (vec![root, assistant, interposed, result], "62000013")
        };

        let cases = [
            ("unmatched result", unmatched),
            ("duplicate result", duplicate_result),
            ("mismatched result name", mismatched_name),
            ("duplicate call", duplicate_call),
            (
                "interposed context with sibling outstanding",
                interposed_context,
            ),
            ("interposed user", interposed_user),
            ("interposed assistant", interposed_assistant),
        ];
        assert_eq!(cases.len(), 7);
        for (name, (entries, leaf)) in cases {
            let error = build_context(&entries, leaf).expect_err("case {name} accepted");
            assert_eq!(error.kind(), PiErrorKind::Invalid, "case {name}");
        }
    });

    test!(build_context_allows_dangling_tool_calls_only_at_the_final_boundary {
        let root = user_entry("63000001", None, "root");
        let assistant = tool_call_entry(
            "63000002",
            Some("63000001"),
            &[("call-1", "read"), ("call-2", "write")],
        );
        let (context, _) =
            build_context(&[root, assistant], "63000002").expect("build context");
        let pending = pending_tool_calls(&context.messages).expect("pending tool calls");
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].tool_call_id, "call-1");
        assert_eq!(pending[1].tool_call_id, "call-2");
    });

    test!(pending_tool_calls_permits_only_tool_result_blocks {
        let assistant = Message {
            role: Role::Assistant,
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: "call-1".into(),
                tool_name: "read".into(),
                ..Block::default()
            }],
            ..Message::default()
        };
        let cases = [
            (
                "empty tool message",
                Message {
                    role: Role::Tool,
                    ..Message::default()
                },
            ),
            (
                "non-result block",
                Message {
                    role: Role::Tool,
                    blocks: vec![Block {
                        block_type: BlockType::Text,
                        tool_call_id: "call-1".into(),
                        tool_name: "read".into(),
                        text: "not a result".into(),
                        ..Block::default()
                    }],
                    ..Message::default()
                },
            ),
        ];
        for (name, tool_message) in cases {
            let error = pending_tool_calls(&[assistant.clone(), tool_message])
                .expect_err("case {name} accepted");
            assert_eq!(error.kind(), PiErrorKind::Invalid, "case {name}");
        }
    });

    test!(build_context_preserves_the_session_info_name_exactly {
        let mut built = entry("session_info", "64000001", None);
        built.session_info = Some(PiSessionInfo {
            name: Some("  exact session name\t ".into()),
        });
        let (context, _) = build_context(&[built], "64000001").expect("build context");
        assert_eq!(context.session_name, "  exact session name\t ");
    });

    test!(build_context_rejects_a_pending_pi_assistant {
        let pending = assistant_entry(
            "70000001",
            None,
            "partial",
            "openai-compatible",
            "model",
            0,
            0,
            "pending",
        );
        let error = build_context(&[pending], "70000001").expect_err("pending accepted");
        assert_eq!(error.kind(), PiErrorKind::Invalid);
    });

    test!(build_context_rejects_duplicate_forward_parent_and_cycle {
        let root = user_entry("80000001", None, "root");
        let duplicate = user_entry("80000001", None, "duplicate");
        let error = build_context(&[root, duplicate], "80000001").expect_err("duplicate accepted");
        assert_eq!(error.kind(), PiErrorKind::Invalid);

        let forward = user_entry("80000002", Some("80000003"), "forward");
        let future = user_entry("80000003", None, "future");
        let error = build_context(&[forward, future], "80000003").expect_err("forward accepted");
        assert_eq!(error.kind(), PiErrorKind::Invalid);

        let cycle = user_entry("80000004", Some("80000004"), "cycle");
        let error = build_context(&[cycle], "80000004").expect_err("cycle accepted");
        assert_eq!(error.kind(), PiErrorKind::Invalid);
    });

    test!(build_context_usage_comes_from_the_full_active_path {
        let assistant = assistant_entry(
            "90000001",
            None,
            "answer",
            "openai-compatible",
            "model",
            10,
            3,
            "stop",
        );
        let mut branch = entry("branch_summary", "90000002", Some("90000001"));
        branch.branch_summary = Some(Box::new(PiBranchSummary {
            from_id: "90000001".into(),
            summary: "summary".into(),
            usage: Some(pi_usage(4, 2)),
            ..PiBranchSummary::default()
        }));
        let mut compaction = entry("compaction", "90000003", Some("90000002"));
        compaction.compaction = Some(Box::new(PiCompaction {
            summary: "compact".into(),
            tokens_before: 10,
            retained_tail: Some(Vec::new()),
            usage: Some(pi_usage(6, 1)),
            ..PiCompaction::default()
        }));

        let (context, _) = build_context(&[assistant, branch, compaction], "90000003")
            .expect("build context");
        assert_eq!(
            context.usage,
            Usage {
                input_tokens: 20,
                output_tokens: 6,
                cached_input_tokens: 0,
            }
        );
    });

    // Every fixture under `testdata/session/pi-v3` resolves to the message
    // sequence below. `unknown-entry.jsonl` has no resolution expectation of
    // its own (its codec test covers raw-JSON preservation only), so the roles
    // and count are snapshotted here.
    test!(every_fixture_resolves_to_the_expected_message_sequence {
        let cases: &[(&str, &[(Role, &str)])] = &[
            (
                "linear.jsonl",
                &[
                    (Role::User, "run the check"),
                    (Role::Assistant, "I will read it."),
                    (Role::Tool, ""),
                    (Role::Assistant, "The check passed."),
                ],
            ),
            (
                "tree.jsonl",
                &[
                    (Role::User, "root"),
                    (Role::Context, "[Branch summary]\ninactive work was abandoned"),
                    (Role::User, "active branch"),
                ],
            ),
            (
                "compacted.jsonl",
                &[
                    (Role::Context, "[Compaction summary]\nsummary"),
                    (Role::User, "retained"),
                    (Role::User, "after"),
                ],
            ),
            (
                "compaction-dual-form.jsonl",
                &[
                    (Role::Context, "[Compaction summary]\ndual summary"),
                    (Role::User, "real retained path"),
                    (Role::User, "after dual checkpoint"),
                ],
            ),
            (
                "compaction-retained-tail-only.jsonl",
                &[
                    (Role::Context, "[Compaction summary]\nretained-tail summary"),
                    (Role::User, "synthetic retained tail"),
                ],
            ),
            ("unknown-entry.jsonl", &[]),
        ];
        assert_eq!(cases.len(), 6, "every fixture must be covered");
        for (name, want) in cases {
            let file = read_pi_fixture(name);
            let leaf = file.entries.last().expect("fixture entries").id.clone();
            let (context, _) = build_context(&file.entries, &leaf)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let got: Vec<(Role, String)> = context
                .messages
                .iter()
                .map(|message| (message.role.clone(), message.text()))
                .collect();
            let want: Vec<(Role, String)> = want
                .iter()
                .map(|(role, text)| (role.clone(), (*text).to_owned()))
                .collect();
            assert_eq!(got, want, "{name}");
        }
    });
}
