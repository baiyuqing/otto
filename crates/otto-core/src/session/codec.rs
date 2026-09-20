//! The Pi v3 JSONL decoder and encoder.
//!
//! Every validation rule, every field path, and every error string is fixed by
//! the Pi v3 files on disk.
//!
//! Ownership: decoding copies the bytes it keeps, so a decoded record does not
//! borrow the input buffer. Encoding returns a fresh buffer.
//!
//! Concurrency: every function here is pure and takes no shared state.
//!
//! Errors: every failure is a [`PiError`]; callers branch on its
//! [`PiErrorKind`] rather than on the message text.
//!
//! Trust boundary: session files are read from disk and may have been written
//! by another tool or corrupted. Nothing is accepted on shape alone; each field
//! is checked against the type the domain expects before it is used.

use std::collections::BTreeMap;

use serde_json::value::RawValue;

use super::pi::{
    MAX_SESSION_ENTRY_BYTES, MAX_SESSION_FILE_BYTES, PI_SESSION_VERSION, PiBranchSummary,
    PiCompaction, PiContentBlock, PiCost, PiCustom, PiCustomMessage, PiEntry, PiFile, PiHeader,
    PiLabel, PiMessage, PiModelChange, PiSessionInfo, PiThinkingLevelChange, PiUsage,
};
use super::{PiError, PiErrorKind};

/// A JSON object kept as raw values, so unknown fields survive untouched.
/// `BTreeMap` also gives the sorted key order the stored files are written in.
pub(crate) type Object = BTreeMap<String, Box<RawValue>>;

/// One record of a session file, for [`encode_pi_record`].
#[derive(Debug, Clone, Copy)]
pub enum PiRecord<'a> {
    Header(&'a PiHeader),
    Entry(&'a PiEntry),
}

// -- file level ------------------------------------------------------------

/// Decodes a whole session file.
///
/// The file is one JSON object per line: the header on line 1 and entries
/// after it. A final record without a trailing newline is accepted, matching
/// what a process killed mid-append leaves behind.
///
/// Rejects an empty file, an empty line, a record over
/// [`MAX_SESSION_ENTRY_BYTES`], and a file over [`MAX_SESSION_FILE_BYTES`].
pub fn decode_pi_file(data: &[u8]) -> Result<PiFile, PiError> {
    if data.len() > MAX_SESSION_FILE_BYTES {
        return Err(PiError::size(
            PiErrorKind::FileTooLarge,
            MAX_SESSION_FILE_BYTES,
        ));
    }
    if data.is_empty() {
        return Err(PiError::invalid("session file is empty"));
    }
    let mut records: Vec<&[u8]> = data.split(|byte| *byte == b'\n').collect();
    if data.last() == Some(&b'\n') {
        records.pop();
    }

    let mut decoded = PiFile::default();
    for (index, record) in records.iter().enumerate() {
        if record.len() > MAX_SESSION_ENTRY_BYTES {
            return Err(PiError::size(
                PiErrorKind::EntryTooLarge,
                MAX_SESSION_ENTRY_BYTES,
            ));
        }
        decode_pi_file_record(&mut decoded, index + 1, record)?;
    }
    Ok(decoded)
}

fn decode_pi_file_record(
    decoded: &mut PiFile,
    line_number: usize,
    record: &[u8],
) -> Result<(), PiError> {
    if record.is_empty() {
        return Err(PiError::invalid(format!("line {line_number} is empty")));
    }
    if line_number == 1 {
        decoded.header =
            decode_pi_header(record).map_err(|error| error.context("session line 1"))?;
        return Ok(());
    }
    let entry = decode_pi_entry(record)
        .map_err(|error| error.context(format!("session line {line_number}")))?;
    decoded.entries.push(entry);
    Ok(())
}

// -- header ----------------------------------------------------------------

/// Decodes line 1 of a session file.
///
/// A record that is not a Pi session header, or carries a version other than
/// [`PI_SESSION_VERSION`], fails with [`PiErrorKind::UnsupportedFormat`] so
/// callers can tell "not ours" apart from "ours and corrupt".
pub fn decode_pi_header(raw: &[u8]) -> Result<PiHeader, PiError> {
    let object = decode_object(raw, "session header")?;
    let type_name = required_string(&object, "type", "session header.type")?;
    if type_name != "session" {
        return Err(PiError::new(
            PiErrorKind::UnsupportedFormat,
            "first record is not a Pi session header",
        ));
    }

    let Some(version_raw) = object.get("version") else {
        return Err(PiError::new(
            PiErrorKind::UnsupportedFormat,
            "Pi session version is missing",
        ));
    };
    let version = decode_scalar::<i64>(version_raw)
        .ok_or_else(|| invalid_field("session header.version", "an integer"))?;
    if version != PI_SESSION_VERSION {
        return Err(PiError::new(
            PiErrorKind::UnsupportedFormat,
            "Pi session version is not supported",
        ));
    }

    Ok(PiHeader {
        type_name,
        version,
        id: required_string(&object, "id", "session header.id")?,
        timestamp: required_string(&object, "timestamp", "session header.timestamp")?,
        cwd: required_string(&object, "cwd", "session header.cwd")?,
        parent_session: optional_string(
            &object,
            "parentSession",
            "session header.parentSession",
            false,
        )?,
        raw: raw.to_vec(),
    })
}

// -- entries ---------------------------------------------------------------

/// Decodes one entry line.
///
/// The shared `type`, `id`, `parentId`, and `timestamp` fields are always
/// validated. A recognized `type` also decodes its payload; an unrecognized
/// one is kept as raw bytes only, so a newer writer's entries round-trip
/// unchanged instead of failing the whole file.
pub fn decode_pi_entry(raw: &[u8]) -> Result<PiEntry, PiError> {
    let object = decode_object(raw, "session entry")?;
    let type_name = required_string(&object, "type", "session entry.type")?;
    let id = required_string(&object, "id", "session entry.id")?;
    let parent_id = required_nullable_string(&object, "parentId", "session entry.parentId")?;
    let timestamp = required_string(&object, "timestamp", "session entry.timestamp")?;

    let mut entry = PiEntry {
        type_name: type_name.clone(),
        id,
        parent_id,
        timestamp,
        raw: raw.to_vec(),
        ..PiEntry::default()
    };

    match type_name.as_str() {
        "message" => {
            let message_raw = required_object_raw(&object, "message", "message entry.message")?;
            entry.message = Some(Box::new(decode_pi_message(
                message_raw.get().as_bytes(),
                "message entry.message",
            )?));
        }
        "model_change" => entry.model_change = Some(decode_pi_model_change(&object)?),
        "thinking_level_change" => {
            entry.thinking_level_change = Some(decode_pi_thinking_level_change(&object)?);
        }
        "compaction" => entry.compaction = Some(Box::new(decode_pi_compaction(&object)?)),
        "branch_summary" => {
            entry.branch_summary = Some(Box::new(decode_pi_branch_summary(&object)?));
        }
        "custom" => entry.custom = Some(decode_pi_custom(&object)?),
        "custom_message" => {
            entry.custom_message = Some(Box::new(decode_pi_custom_message(&object)?));
        }
        "label" => entry.label = Some(decode_pi_label(&object)?),
        "session_info" => entry.session_info = Some(decode_pi_session_info(&object)?),
        _ => {}
    }
    Ok(entry)
}

/// Decodes a message payload at `path`, which names the field for error
/// messages (`message entry.message`, `compaction.retainedTail[0]`, ...).
pub fn decode_pi_message(raw: &[u8], path: &str) -> Result<PiMessage, PiError> {
    let object = decode_object(raw, path)?;
    let role = required_string(&object, "role", &format!("{path}.role"))?;
    let timestamp = required_i64(&object, "timestamp", &format!("{path}.timestamp"))?;

    let mut message: PiMessage = serde_json::from_slice(raw)
        .map_err(|_| PiError::invalid(format!("{path} contains a field with an invalid shape")))?;
    message.role = role.clone();
    message.timestamp = timestamp;

    match role.as_str() {
        "user" => {
            let (text, blocks) =
                decode_content_field(&object, "content", &format!("{path}.content"), true)?;
            message.content_text = text;
            message.content_blocks = blocks;
        }
        "assistant" => validate_assistant_message(&object, &mut message, path)?,
        "toolResult" => validate_tool_result_message(&object, &mut message, path)?,
        "bashExecution" => validate_bash_execution_message(&object, path)?,
        "custom" => validate_custom_agent_message(&object, &mut message, path)?,
        "branchSummary" => {
            required_string(&object, "summary", &format!("{path}.summary"))?;
            required_string(&object, "fromId", &format!("{path}.fromId"))?;
        }
        "compactionSummary" => {
            required_string(&object, "summary", &format!("{path}.summary"))?;
            required_i64(&object, "tokensBefore", &format!("{path}.tokensBefore"))?;
        }
        _ => {
            return Err(invalid_field(
                &format!("{path}.role"),
                "a supported message role",
            ));
        }
    }
    Ok(message)
}

fn validate_assistant_message(
    object: &Object,
    message: &mut PiMessage,
    path: &str,
) -> Result<(), PiError> {
    let (text, blocks) =
        decode_content_field(object, "content", &format!("{path}.content"), false)?;
    message.content_text = text;
    message.content_blocks = blocks;

    for field in ["api", "provider", "model", "stopReason"] {
        required_string(object, field, &format!("{path}.{field}"))?;
    }
    let usage_raw = required_object_raw(object, "usage", &format!("{path}.usage"))?;
    message.usage = Some(decode_pi_usage(usage_raw, &format!("{path}.usage"))?);

    for field in [
        "responseModel",
        "responseId",
        "errorMessage",
        "rawStopReason",
    ] {
        optional_string(object, field, &format!("{path}.{field}"), false)?;
    }
    validate_optional_bool(object, "endTurn", &format!("{path}.endTurn"))?;
    if let Some(raw) = object.get("deferred") {
        validate_deferred_handle(raw, &format!("{path}.deferred"))?;
    }
    if let Some(raw) = object.get("diagnostics") {
        validate_diagnostics(raw, &format!("{path}.diagnostics"))?;
    }
    Ok(())
}

fn validate_tool_result_message(
    object: &Object,
    message: &mut PiMessage,
    path: &str,
) -> Result<(), PiError> {
    for field in ["toolCallId", "toolName"] {
        required_string(object, field, &format!("{path}.{field}"))?;
    }
    let (text, blocks) =
        decode_content_field(object, "content", &format!("{path}.content"), false)?;
    message.content_text = text;
    message.content_blocks = blocks;

    required_bool(object, "isError", &format!("{path}.isError"))?;
    if let Some(raw) = object.get("usage") {
        message.usage = Some(decode_pi_usage(raw, &format!("{path}.usage"))?);
    }
    if let Some(raw) = object.get("addedToolNames")
        && (!is_json_array(raw) || serde_json::from_str::<Vec<String>>(raw.get()).is_err())
    {
        return Err(invalid_field(
            &format!("{path}.addedToolNames"),
            "an array of strings",
        ));
    }
    Ok(())
}

fn validate_bash_execution_message(object: &Object, path: &str) -> Result<(), PiError> {
    for field in ["command", "output"] {
        required_string(object, field, &format!("{path}.{field}"))?;
    }
    for field in ["cancelled", "truncated"] {
        required_bool(object, field, &format!("{path}.{field}"))?;
    }
    if let Some(raw) = object.get("exitCode")
        && decode_scalar::<i64>(raw).is_none()
    {
        return Err(invalid_field(&format!("{path}.exitCode"), "an integer"));
    }
    optional_string(
        object,
        "fullOutputPath",
        &format!("{path}.fullOutputPath"),
        false,
    )?;
    validate_optional_bool(
        object,
        "excludeFromContext",
        &format!("{path}.excludeFromContext"),
    )
}

fn validate_custom_agent_message(
    object: &Object,
    message: &mut PiMessage,
    path: &str,
) -> Result<(), PiError> {
    required_string(object, "customType", &format!("{path}.customType"))?;
    let (text, blocks) = decode_content_field(object, "content", &format!("{path}.content"), true)?;
    message.content_text = text;
    message.content_blocks = blocks;
    required_bool(object, "display", &format!("{path}.display"))?;
    Ok(())
}

fn decode_pi_usage(raw: &RawValue, path: &str) -> Result<PiUsage, PiError> {
    let object = decode_object(raw.get().as_bytes(), path)?;
    let mut usage: PiUsage = serde_json::from_str(raw.get())
        .map_err(|_| PiError::invalid(format!("{path} contains a field with an invalid shape")))?;
    for field in ["input", "output", "cacheRead", "cacheWrite", "totalTokens"] {
        required_i64(&object, field, &format!("{path}.{field}"))?;
    }
    for field in ["cacheWrite1h", "reasoning"] {
        if let Some(value) = object.get(field)
            && decode_scalar::<i64>(value).is_none()
        {
            return Err(invalid_field(&format!("{path}.{field}"), "an integer"));
        }
    }
    let cost_raw = required_object_raw(&object, "cost", &format!("{path}.cost"))?;
    usage.cost = decode_pi_cost(cost_raw, &format!("{path}.cost"))?;
    Ok(usage)
}

fn decode_pi_cost(raw: &RawValue, path: &str) -> Result<PiCost, PiError> {
    let object = decode_object(raw.get().as_bytes(), path)?;
    let cost: PiCost = serde_json::from_str(raw.get())
        .map_err(|_| PiError::invalid(format!("{path} contains a field with an invalid shape")))?;
    for field in ["input", "output", "cacheRead", "cacheWrite", "total"] {
        required_f64(&object, field, &format!("{path}.{field}"))?;
    }
    Ok(cost)
}

fn decode_pi_model_change(object: &Object) -> Result<PiModelChange, PiError> {
    Ok(PiModelChange {
        provider: required_string(object, "provider", "model_change.provider")?,
        model_id: required_string(object, "modelId", "model_change.modelId")?,
    })
}

fn decode_pi_thinking_level_change(object: &Object) -> Result<PiThinkingLevelChange, PiError> {
    Ok(PiThinkingLevelChange {
        thinking_level: required_string(
            object,
            "thinkingLevel",
            "thinking_level_change.thinkingLevel",
        )?,
    })
}

fn decode_pi_compaction(object: &Object) -> Result<PiCompaction, PiError> {
    let summary = required_string(object, "summary", "compaction.summary")?;
    let tokens_before = required_i64(object, "tokensBefore", "compaction.tokensBefore")?;
    let first_kept_entry_id = optional_string(
        object,
        "firstKeptEntryId",
        "compaction.firstKeptEntryId",
        false,
    )?;

    let mut compaction = PiCompaction {
        summary,
        first_kept_entry_id: first_kept_entry_id.clone(),
        tokens_before,
        ..PiCompaction::default()
    };
    if let Some(raw) = object.get("details") {
        compaction.details = Some(raw.clone());
    }
    if let Some(raw) = object.get("fromHook") {
        compaction.from_hook = Some(decode_bool(raw, "compaction.fromHook")?);
    }
    if let Some(raw) = object.get("usage") {
        compaction.usage = Some(decode_pi_usage(raw, "compaction.usage")?);
    }
    let has_retained_tail = object.contains_key("retainedTail");
    if let Some(raw) = object.get("retainedTail") {
        let messages = json_array_elements(raw)
            .ok_or_else(|| invalid_field("compaction.retainedTail", "an array of messages"))?;
        let mut tail = Vec::with_capacity(messages.len());
        for (index, message_raw) in messages.iter().enumerate() {
            tail.push(decode_pi_message(
                message_raw.get().as_bytes(),
                &format!("compaction.retainedTail[{index}]"),
            )?);
        }
        compaction.retained_tail = Some(tail);
    }
    if first_kept_entry_id.is_none() && !has_retained_tail {
        return Err(PiError::invalid(
            "compaction requires firstKeptEntryId or retainedTail",
        ));
    }
    Ok(compaction)
}

fn decode_pi_branch_summary(object: &Object) -> Result<PiBranchSummary, PiError> {
    let mut entry = PiBranchSummary {
        from_id: required_string(object, "fromId", "branch_summary.fromId")?,
        summary: required_string(object, "summary", "branch_summary.summary")?,
        ..PiBranchSummary::default()
    };
    if let Some(raw) = object.get("details") {
        entry.details = Some(raw.clone());
    }
    if let Some(raw) = object.get("fromHook") {
        entry.from_hook = Some(decode_bool(raw, "branch_summary.fromHook")?);
    }
    if let Some(raw) = object.get("usage") {
        entry.usage = Some(decode_pi_usage(raw, "branch_summary.usage")?);
    }
    Ok(entry)
}

fn decode_pi_custom(object: &Object) -> Result<PiCustom, PiError> {
    Ok(PiCustom {
        custom_type: required_string(object, "customType", "custom.customType")?,
        data: object.get("data").cloned(),
    })
}

fn decode_pi_custom_message(object: &Object) -> Result<PiCustomMessage, PiError> {
    let custom_type = required_string(object, "customType", "custom_message.customType")?;
    let Some(content_raw) = object.get("content") else {
        return Err(invalid_field("custom_message.content", "present"));
    };
    let (content_text, content_blocks) =
        decode_content(content_raw, "custom_message.content", true)?;
    let display = required_bool(object, "display", "custom_message.display")?;
    Ok(PiCustomMessage {
        custom_type,
        content: Some(content_raw.clone()),
        details: object.get("details").cloned(),
        display,
        content_text,
        content_blocks,
    })
}

fn decode_pi_label(object: &Object) -> Result<PiLabel, PiError> {
    Ok(PiLabel {
        target_id: required_string(object, "targetId", "label.targetId")?,
        label: optional_string(object, "label", "label.label", false)?,
    })
}

fn decode_pi_session_info(object: &Object) -> Result<PiSessionInfo, PiError> {
    Ok(PiSessionInfo {
        name: optional_string(object, "name", "session_info.name", false)?,
    })
}

// -- content ---------------------------------------------------------------

type DecodedContent = (Option<String>, Vec<PiContentBlock>);

fn decode_content_field(
    object: &Object,
    field: &str,
    path: &str,
    allow_string: bool,
) -> Result<DecodedContent, PiError> {
    let Some(raw) = object.get(field) else {
        return Err(invalid_field(path, "present"));
    };
    decode_content(raw, path, allow_string)
}

/// Decodes a `content` value. `allow_string` selects the roles where Pi also
/// permits a bare string instead of a block array.
pub(crate) fn decode_content(
    raw: &RawValue,
    path: &str,
    allow_string: bool,
) -> Result<DecodedContent, PiError> {
    if allow_string && is_json_string(raw) {
        let text = serde_json::from_str::<String>(raw.get())
            .map_err(|_| invalid_field(path, "a string or content array"))?;
        return Ok((Some(text), Vec::new()));
    }
    let shape = if allow_string {
        "a string or content array"
    } else {
        "a content array"
    };
    if !is_json_array(raw) {
        return Err(invalid_field(path, shape));
    }
    let raw_blocks =
        json_array_elements(raw).ok_or_else(|| invalid_field(path, "a content array"))?;

    let mut blocks = Vec::with_capacity(raw_blocks.len());
    for (index, raw_block) in raw_blocks.iter().enumerate() {
        let block_path = format!("{path}[{index}]");
        let object = decode_object(raw_block.get().as_bytes(), &block_path)?;
        let block_type = required_string(&object, "type", &format!("{block_path}.type"))?;
        let mut block: PiContentBlock = serde_json::from_str(raw_block.get()).map_err(|_| {
            PiError::invalid(format!(
                "{block_path} contains a field with an invalid shape"
            ))
        })?;
        block.type_name = block_type.clone();
        block.raw = raw_block.get().as_bytes().to_vec();

        for field in [
            "textSignature",
            "thinkingSignature",
            "thoughtSignature",
            "namespace",
        ] {
            optional_string(&object, field, &format!("{block_path}.{field}"), false)?;
        }
        validate_optional_bool(&object, "redacted", &format!("{block_path}.redacted"))?;

        match block_type.as_str() {
            "text" => {
                required_string(&object, "text", &format!("{block_path}.text"))?;
            }
            "image" => {
                for field in ["data", "mimeType"] {
                    required_string(&object, field, &format!("{block_path}.{field}"))?;
                }
            }
            "thinking" => {
                required_string(&object, "thinking", &format!("{block_path}.thinking"))?;
            }
            "toolCall" => {
                for field in ["id", "name"] {
                    required_string(&object, field, &format!("{block_path}.{field}"))?;
                }
                let Some(arguments) = object.get("arguments") else {
                    return Err(invalid_field(
                        &format!("{block_path}.arguments"),
                        "an object",
                    ));
                };
                decode_object(
                    arguments.get().as_bytes(),
                    &format!("{block_path}.arguments"),
                )?;
            }
            _ => {
                return Err(invalid_field(
                    &format!("{block_path}.type"),
                    "a supported content block type",
                ));
            }
        }
        blocks.push(block);
    }
    Ok((None, blocks))
}

fn validate_deferred_handle(raw: &RawValue, path: &str) -> Result<(), PiError> {
    let object = decode_object(raw.get().as_bytes(), path)?;
    for field in ["provider", "modelId", "api", "id"] {
        required_string(&object, field, &format!("{path}.{field}"))?;
    }
    for field in ["expiresAt", "pollAfterMs"] {
        if let Some(value) = object.get(field)
            && decode_scalar::<i64>(value).is_none()
        {
            return Err(invalid_field(&format!("{path}.{field}"), "an integer"));
        }
    }
    Ok(())
}

fn validate_diagnostics(raw: &RawValue, path: &str) -> Result<(), PiError> {
    if !is_json_array(raw) {
        return Err(invalid_field(path, "an array"));
    }
    let diagnostics = json_array_elements(raw).ok_or_else(|| invalid_field(path, "an array"))?;
    for (index, diagnostic_raw) in diagnostics.iter().enumerate() {
        let diagnostic_path = format!("{path}[{index}]");
        let object = decode_object(diagnostic_raw.get().as_bytes(), &diagnostic_path)?;
        required_string(&object, "type", &format!("{diagnostic_path}.type"))?;
        required_i64(
            &object,
            "timestamp",
            &format!("{diagnostic_path}.timestamp"),
        )?;
        if let Some(details) = object.get("details") {
            decode_object(
                details.get().as_bytes(),
                &format!("{diagnostic_path}.details"),
            )?;
        }
        if let Some(error_raw) = object.get("error") {
            let error_object = decode_object(
                error_raw.get().as_bytes(),
                &format!("{diagnostic_path}.error"),
            )?;
            required_string(
                &error_object,
                "message",
                &format!("{diagnostic_path}.error.message"),
            )?;
            for field in ["name", "stack"] {
                optional_string(
                    &error_object,
                    field,
                    &format!("{diagnostic_path}.error.{field}"),
                    false,
                )?;
            }
            if let Some(code) = error_object.get("code")
                && !is_json_string(code)
                && decode_scalar::<f64>(code).is_none()
            {
                return Err(invalid_field(
                    &format!("{diagnostic_path}.error.code"),
                    "a string or number",
                ));
            }
        }
    }
    Ok(())
}

fn validate_optional_bool(object: &Object, field: &str, path: &str) -> Result<(), PiError> {
    match object.get(field) {
        Some(raw) => decode_bool(raw, path).map(|_| ()),
        None => Ok(()),
    }
}

// -- encoding --------------------------------------------------------------

/// Encodes one record as the exact bytes of a session file line, without the
/// trailing newline.
///
/// A record decoded from a file is re-emitted from its captured bytes so
/// unknown fields survive; the bytes are size-checked and re-decoded first, so
/// a caller cannot smuggle unvalidated JSON back into a file. A record built
/// in memory is encoded field by field and then decoded again as a self-check.
pub fn encode_pi_record(record: PiRecord<'_>) -> Result<Vec<u8>, PiError> {
    let encoded = match record {
        PiRecord::Header(header) => encode_pi_header(header),
        PiRecord::Entry(entry) => encode_pi_entry(entry),
    }
    .map_err(|error| error.context("encode Pi session record"))?;
    if encoded.len() > MAX_SESSION_ENTRY_BYTES {
        return Err(PiError::size(
            PiErrorKind::EntryTooLarge,
            MAX_SESSION_ENTRY_BYTES,
        ));
    }
    decode_object(&encoded, "encoded session record")?;
    Ok(encoded)
}

fn encode_pi_header(header: &PiHeader) -> Result<Vec<u8>, PiError> {
    if !header.raw.is_empty() {
        if header.raw.len() > MAX_SESSION_ENTRY_BYTES {
            return Err(PiError::size(
                PiErrorKind::EntryTooLarge,
                MAX_SESSION_ENTRY_BYTES,
            ));
        }
        decode_pi_header(&header.raw)?;
        return Ok(header.raw.clone());
    }
    let encoded = serde_json::to_vec(header)
        .map_err(|error| PiError::other(format!("encode session header: {error}")))?;
    decode_pi_header(&encoded)?;
    Ok(encoded)
}

fn encode_pi_entry(entry: &PiEntry) -> Result<Vec<u8>, PiError> {
    if !entry.raw.is_empty() {
        if entry.raw.len() > MAX_SESSION_ENTRY_BYTES {
            return Err(PiError::size(
                PiErrorKind::EntryTooLarge,
                MAX_SESSION_ENTRY_BYTES,
            ));
        }
        decode_pi_entry(&entry.raw)?;
        return Ok(entry.raw.clone());
    }

    let mut object = Object::new();
    object.insert("type".into(), to_raw(&entry.type_name)?);
    object.insert("id".into(), to_raw(&entry.id)?);
    object.insert("parentId".into(), to_raw(&entry.parent_id)?);
    object.insert("timestamp".into(), to_raw(&entry.timestamp)?);

    let payload: Option<Box<RawValue>> = match entry.type_name.as_str() {
        "message" => {
            let Some(message) = &entry.message else {
                return Err(PiError::invalid("message payload is required"));
            };
            object.insert("message".into(), to_raw(message)?);
            None
        }
        "model_change" => entry.model_change.as_ref().map(to_raw).transpose()?,
        "thinking_level_change" => entry
            .thinking_level_change
            .as_ref()
            .map(to_raw)
            .transpose()?,
        "compaction" => entry.compaction.as_ref().map(to_raw).transpose()?,
        "branch_summary" => entry.branch_summary.as_ref().map(to_raw).transpose()?,
        "custom" => entry.custom.as_ref().map(to_raw).transpose()?,
        "custom_message" => entry.custom_message.as_ref().map(to_raw).transpose()?,
        "label" => entry.label.as_ref().map(to_raw).transpose()?,
        "session_info" => entry.session_info.as_ref().map(to_raw).transpose()?,
        _ => {
            return Err(PiError::invalid(
                "raw JSON is required to encode an unknown entry type",
            ));
        }
    };
    if entry.type_name != "message" {
        let Some(payload) = payload else {
            return Err(PiError::invalid(format!(
                "{} payload is required",
                entry.type_name
            )));
        };
        let fields = decode_object(
            payload.get().as_bytes(),
            &format!("{} payload", entry.type_name),
        )?;
        object.extend(fields);
    }

    let encoded = serde_json::to_vec(&object)
        .map_err(|error| PiError::other(format!("encode session entry: {error}")))?;
    decode_pi_entry(&encoded)?;
    Ok(encoded)
}

fn to_raw<T: serde::Serialize + ?Sized>(value: &T) -> Result<Box<RawValue>, PiError> {
    serde_json::value::to_raw_value(value)
        .map_err(|error| PiError::other(format!("encode session field: {error}")))
}

// -- shape helpers ---------------------------------------------------------

pub(crate) fn decode_object(raw: &[u8], path: &str) -> Result<Object, PiError> {
    serde_json::from_slice(raw)
        .map_err(|_| PiError::invalid(format!("{path} must be one JSON object")))
}

fn required_object_raw<'a>(
    object: &'a Object,
    field: &str,
    path: &str,
) -> Result<&'a RawValue, PiError> {
    let Some(raw) = object.get(field) else {
        return Err(invalid_field(path, "an object"));
    };
    decode_object(raw.get().as_bytes(), path)?;
    Ok(raw)
}

pub(crate) fn required_string(object: &Object, field: &str, path: &str) -> Result<String, PiError> {
    match object.get(field) {
        Some(raw) if !is_json_null(raw) => {
            serde_json::from_str::<String>(raw.get()).map_err(|_| invalid_field(path, "a string"))
        }
        _ => Err(invalid_field(path, "a string")),
    }
}

pub(crate) fn optional_string(
    object: &Object,
    field: &str,
    path: &str,
    allow_null: bool,
) -> Result<Option<String>, PiError> {
    let Some(raw) = object.get(field) else {
        return Ok(None);
    };
    if is_json_null(raw) {
        if allow_null {
            return Ok(None);
        }
        return Err(invalid_field(path, "a string"));
    }
    serde_json::from_str::<String>(raw.get())
        .map(Some)
        .map_err(|_| invalid_field(path, "a string"))
}

fn required_nullable_string(
    object: &Object,
    field: &str,
    path: &str,
) -> Result<Option<String>, PiError> {
    let Some(raw) = object.get(field) else {
        return Err(invalid_field(path, "a string or null"));
    };
    if is_json_null(raw) {
        return Ok(None);
    }
    serde_json::from_str::<String>(raw.get())
        .map(Some)
        .map_err(|_| invalid_field(path, "a string or null"))
}

pub(crate) fn required_i64(object: &Object, field: &str, path: &str) -> Result<i64, PiError> {
    object
        .get(field)
        .and_then(|raw| decode_scalar::<i64>(raw))
        .ok_or_else(|| invalid_field(path, "an integer"))
}

fn required_f64(object: &Object, field: &str, path: &str) -> Result<f64, PiError> {
    object
        .get(field)
        .and_then(|raw| decode_scalar::<f64>(raw))
        .ok_or_else(|| invalid_field(path, "a number"))
}

fn required_bool(object: &Object, field: &str, path: &str) -> Result<bool, PiError> {
    match object.get(field) {
        Some(raw) => decode_bool(raw, path),
        None => Err(invalid_field(path, "a boolean")),
    }
}

fn decode_bool(raw: &RawValue, path: &str) -> Result<bool, PiError> {
    decode_scalar::<bool>(raw).ok_or_else(|| invalid_field(path, "a boolean"))
}

/// Decodes a non-null JSON scalar, returning `None` for `null` and for any
/// value of the wrong type.
fn decode_scalar<T: serde::de::DeserializeOwned>(raw: &RawValue) -> Option<T> {
    if is_json_null(raw) {
        return None;
    }
    serde_json::from_str::<T>(raw.get()).ok()
}

pub(crate) fn invalid_field(path: &str, shape: &str) -> PiError {
    PiError::invalid(format!("{path} must be {shape}"))
}

pub(crate) fn is_json_null(raw: &RawValue) -> bool {
    raw.get().trim() == "null"
}

fn is_json_string(raw: &RawValue) -> bool {
    raw.get().trim_start().starts_with('"')
}

fn is_json_array(raw: &RawValue) -> bool {
    raw.get().trim_start().starts_with('[')
}

/// Elements of a JSON array, or `None` when the value is not an array.
fn json_array_elements(raw: &RawValue) -> Option<Vec<Box<RawValue>>> {
    if !is_json_array(raw) {
        return None;
    }
    serde_json::from_str(raw.get()).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::session::pi::PiHeader;

    /// The six checked-in Pi v3 fixtures, embedded so the decoder tests run on
    /// every target including wasm32.
    pub(crate) const FIXTURES: &[(&str, &[u8])] = &[
        (
            "compacted.jsonl",
            include_bytes!("../../../../testdata/session/pi-v3/compacted.jsonl"),
        ),
        (
            "compaction-dual-form.jsonl",
            include_bytes!("../../../../testdata/session/pi-v3/compaction-dual-form.jsonl"),
        ),
        (
            "compaction-retained-tail-only.jsonl",
            include_bytes!(
                "../../../../testdata/session/pi-v3/compaction-retained-tail-only.jsonl"
            ),
        ),
        (
            "linear.jsonl",
            include_bytes!("../../../../testdata/session/pi-v3/linear.jsonl"),
        ),
        (
            "tree.jsonl",
            include_bytes!("../../../../testdata/session/pi-v3/tree.jsonl"),
        ),
        (
            "unknown-entry.jsonl",
            include_bytes!("../../../../testdata/session/pi-v3/unknown-entry.jsonl"),
        ),
    ];

    pub(crate) fn fixture(name: &str) -> &'static [u8] {
        FIXTURES
            .iter()
            .find(|(fixture_name, _)| *fixture_name == name)
            .map(|(_, data)| *data)
            .unwrap_or_else(|| panic!("unknown fixture {name}"))
    }

    pub(crate) fn read_pi_fixture(name: &str) -> PiFile {
        decode_pi_file(fixture(name)).expect("fixture decodes")
    }

    macro_rules! test {
        ($name:ident, $body:block) => {
            #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
            #[cfg_attr(not(target_arch = "wasm32"), test)]
            fn $name() $body
        };
    }

    test!(decode_pi_v3_linear_fixture, {
        let decoded = read_pi_fixture("linear.jsonl");
        assert_eq!(decoded.header.type_name, "session");
        assert_eq!(decoded.header.version, 3);
        assert_eq!(decoded.header.cwd, "/workspace");
        assert_eq!(decoded.entries.len(), 4);
        assert!(decoded.entries[0].parent_id.is_none());

        let assistant = decoded.entries[1]
            .message
            .as_ref()
            .expect("assistant message");
        assert_eq!(assistant.role, "assistant");
        assert_eq!(assistant.content_blocks.len(), 2);
        assert_eq!(assistant.content_blocks[1].type_name, "toolCall");

        let result = decoded.entries[2].message.as_ref().expect("tool result");
        assert_eq!(result.role, "toolResult");
        assert_eq!(result.tool_call_id, "call-1");
        assert_eq!(result.is_error, Some(false));
    });

    test!(decode_pi_v3_tree_fixture_uses_exact_entry_shapes, {
        let decoded = read_pi_fixture("tree.jsonl");
        assert_eq!(
            decoded.header.parent_session.as_deref(),
            Some("/workspace/parent.jsonl")
        );

        let seen = |type_name: &str| -> PiEntry {
            decoded
                .entries
                .iter()
                .rev()
                .find(|entry| entry.type_name == type_name)
                .unwrap_or_else(|| panic!("no {type_name} entry"))
                .clone()
        };
        assert_eq!(
            seen("custom").custom.expect("custom payload").custom_type,
            "otto.runtime"
        );
        assert_eq!(
            seen("model_change")
                .model_change
                .expect("model_change payload")
                .model_id,
            "test-model-2"
        );
        assert!(
            seen("thinking_level_change")
                .thinking_level_change
                .is_some()
        );
        assert_eq!(
            seen("branch_summary")
                .branch_summary
                .expect("branch_summary payload")
                .from_id,
            "a0000003"
        );
        assert_eq!(
            seen("label").label.expect("label payload").target_id,
            "a0000002"
        );
        assert!(
            seen("session_info")
                .session_info
                .expect("session_info payload")
                .name
                .is_some()
        );
    });

    test!(decode_pi_v3_compaction_fixture, {
        let decoded = read_pi_fixture("compacted.jsonl");
        let legacy = decoded.entries[2]
            .compaction
            .as_ref()
            .expect("legacy compaction");
        assert_eq!(legacy.first_kept_entry_id.as_deref(), Some("c0000002"));

        let checkpoint = decoded.entries[3].compaction.as_ref().expect("checkpoint");
        assert_eq!(checkpoint.summary, "summary");
        assert_eq!(
            checkpoint
                .retained_tail
                .as_deref()
                .unwrap_or_default()
                .len(),
            1
        );
        assert_eq!(
            checkpoint.retained_tail.as_ref().expect("tail")[0].role,
            "user"
        );
        let usage = checkpoint.usage.as_ref().expect("checkpoint usage");
        assert_eq!((usage.input, usage.output), (12, 4));
        assert_eq!(checkpoint.from_hook, Some(false));
    });

    test!(decode_pi_v3_compaction_boundary_fixtures, {
        let retained = read_pi_fixture("compaction-retained-tail-only.jsonl");
        let checkpoint = retained
            .entries
            .last()
            .expect("entries")
            .compaction
            .as_ref()
            .expect("checkpoint");
        assert!(checkpoint.first_kept_entry_id.is_none());
        assert_eq!(
            checkpoint
                .retained_tail
                .as_deref()
                .unwrap_or_default()
                .len(),
            1
        );
        assert_eq!(
            checkpoint.retained_tail.as_ref().expect("tail")[0].role,
            "user"
        );
        let details = checkpoint.details.as_ref().expect("details").get();
        assert!(
            details.contains(r#""readFiles":"malformed""#),
            "malformed external details were not preserved: {details}"
        );

        let dual = read_pi_fixture("compaction-dual-form.jsonl");
        let checkpoint = dual.entries[3].compaction.as_ref().expect("checkpoint");
        assert_eq!(checkpoint.first_kept_entry_id.as_deref(), Some("e0000003"));
        assert_eq!(
            checkpoint
                .retained_tail
                .as_deref()
                .unwrap_or_default()
                .len(),
            1
        );
    });

    test!(decode_pi_v3_preserves_unknown_entry_raw_json, {
        let decoded = read_pi_fixture("unknown-entry.jsonl");
        let entry = &decoded.entries[1];
        assert_eq!(entry.type_name, "future_entry");
        let raw = String::from_utf8(entry.raw.clone()).expect("utf-8");
        assert!(raw.contains(r#""futureField""#), "raw: {raw}");
        let encoded = encode_pi_record(PiRecord::Entry(entry)).expect("encode");
        assert_eq!(encoded, entry.raw, "encoded unknown entry changed raw JSON");
    });

    test!(
        decode_pi_v3_preserves_unknown_fields_on_supported_objects,
        {
            let decoded = read_pi_fixture("unknown-entry.jsonl");
            let header_raw = String::from_utf8(decoded.header.raw.clone()).expect("utf-8");
            assert!(
                header_raw.contains(r#""futureHeaderField""#),
                "{header_raw}"
            );

            for index in [0usize, 2] {
                let entry = &decoded.entries[index];
                let raw = String::from_utf8(entry.raw.clone()).expect("utf-8");
                assert!(raw.contains("future"), "entry {index} raw: {raw}");
                let encoded = encode_pi_record(PiRecord::Entry(entry)).expect("encode");
                assert_eq!(encoded, entry.raw, "entry {index} changed raw JSON");
            }

            let encoded_header =
                encode_pi_record(PiRecord::Header(&decoded.header)).expect("encode header");
            assert_eq!(encoded_header, decoded.header.raw);
        }
    );

    test!(decode_pi_v3_tolerates_additional_nested_fields, {
        const INPUT: &str = r#"{"type":"message","id":"e0000001","parentId":null,"timestamp":"2026-08-27T12:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"ok","futureText":true}],"api":"openai-completions","provider":"openai-compatible","model":"test-model","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0,"futureCost":0},"futureUsage":0},"stopReason":"stop","timestamp":1787832000000,"futureMessage":true},"futureEntry":true}"#;
        let entry = decode_pi_entry(INPUT.as_bytes()).expect("decode");
        assert!(entry.message.is_some());
        assert_eq!(entry.raw, INPUT.as_bytes());
    });

    test!(decode_pi_v3_accepts_final_record_without_lf, {
        let contents = fixture("linear.jsonl");
        let trimmed = contents.strip_suffix(b"\n").expect("fixture ends with LF");
        let decoded = decode_pi_file(trimmed).expect("decode");
        assert_eq!(decoded.entries.len(), 4);
    });

    test!(decode_pi_v3_rejects_malformed_known_shapes, {
        const BASE: &str = r#""id":"e0000001","parentId":null,"timestamp":"2026-08-27T12:00:00Z""#;
        let usage = r#""usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}"#;
        let cases: Vec<(&str, String)> = vec![
            (
                "missing base parentId",
                r#"{"type":"future_entry","id":"e0000001","timestamp":"2026-08-27T12:00:00Z"}"#.into(),
            ),
            (
                "wrong base id",
                r#"{"type":"future_entry","id":1,"parentId":null,"timestamp":"2026-08-27T12:00:00Z"}"#.into(),
            ),
            ("custom type", format!(r#"{{"type":"custom",{BASE},"customType":1}}"#)),
            (
                "model id",
                format!(r#"{{"type":"model_change",{BASE},"provider":"openai-compatible"}}"#),
            ),
            (
                "thinking level",
                format!(r#"{{"type":"thinking_level_change",{BASE},"thinkingLevel":false}}"#),
            ),
            (
                "message role",
                format!(r#"{{"type":"message",{BASE},"message":{{"role":1,"timestamp":1}}}}"#),
            ),
            (
                "unknown message role",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"futureRole","timestamp":1}}}}"#
                ),
            ),
            (
                "user content",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"user","content":42,"timestamp":1}}}}"#
                ),
            ),
            (
                "unknown content type",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"user","content":[{{"type":"futureBlock"}}],"timestamp":1}}}}"#
                ),
            ),
            (
                "text block",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"user","content":[{{"type":"text","text":false}}],"timestamp":1}}}}"#
                ),
            ),
            (
                "text signature",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"user","content":[{{"type":"text","text":"ok","textSignature":1}}],"timestamp":1}}}}"#
                ),
            ),
            (
                "tool arguments",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"assistant","content":[{{"type":"toolCall","id":"call-1","name":"read","arguments":[]}}],"api":"openai-completions","provider":"openai-compatible","model":"test",{usage},"stopReason":"toolUse","timestamp":1}}}}"#
                ),
            ),
            (
                "tool namespace",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"assistant","content":[{{"type":"toolCall","id":"call-1","name":"read","arguments":{{}},"namespace":1}}],"api":"openai-completions","provider":"openai-compatible","model":"test",{usage},"stopReason":"toolUse","timestamp":1}}}}"#
                ),
            ),
            (
                "usage",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"assistant","content":[],"api":"openai-completions","provider":"openai-compatible","model":"test","usage":{{"input":"zero"}},"stopReason":"stop","timestamp":1}}}}"#
                ),
            ),
            (
                "deferred handle",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"assistant","content":[],"api":"openai-completions","provider":"openai-compatible","model":"test",{usage},"stopReason":"deferred","deferred":{{"provider":1}},"timestamp":1}}}}"#
                ),
            ),
            (
                "diagnostic timestamp",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"assistant","content":[],"api":"openai-completions","provider":"openai-compatible","model":"test",{usage},"stopReason":"stop","diagnostics":[{{"type":"warning","timestamp":"now"}}],"timestamp":1}}}}"#
                ),
            ),
            (
                "tool result flag",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":[],"isError":"false","timestamp":1}}}}"#
                ),
            ),
            (
                "null bash exit code",
                format!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"bashExecution","command":"pwd","output":"/workspace","exitCode":null,"cancelled":false,"truncated":false,"timestamp":1}}}}"#
                ),
            ),
            (
                "compaction boundary",
                format!(r#"{{"type":"compaction",{BASE},"summary":"summary","tokensBefore":1}}"#),
            ),
            (
                "compaction tail",
                format!(
                    r#"{{"type":"compaction",{BASE},"summary":"summary","tokensBefore":1,"retainedTail":{{}}}}"#
                ),
            ),
            (
                "branch from id",
                format!(r#"{{"type":"branch_summary",{BASE},"fromId":1,"summary":"summary"}}"#),
            ),
            (
                "custom display",
                format!(
                    r#"{{"type":"custom_message",{BASE},"customType":"fixture","content":"text","display":"true"}}"#
                ),
            ),
            ("label target", format!(r#"{{"type":"label",{BASE},"targetId":null}}"#)),
            ("session name", format!(r#"{{"type":"session_info",{BASE},"name":1}}"#)),
        ];
        assert_eq!(cases.len(), 24);
        for (name, input) in cases {
            let error = decode_pi_entry(input.as_bytes()).expect_err(name);
            assert_eq!(error.kind(), PiErrorKind::Invalid, "case {name}: {error}");
        }
    });

    test!(decode_pi_v3_rejects_unsupported_headers, {
        let cases = [
            ("old Otto", r#"{"type":"header","header":{"version":1}}"#),
            (
                "old Pi",
                r#"{"type":"session","version":2,"id":"id","timestamp":"now","cwd":"/workspace"}"#,
            ),
            (
                "future Pi",
                r#"{"type":"session","version":4,"id":"id","timestamp":"now","cwd":"/workspace"}"#,
            ),
            (
                "missing v3",
                r#"{"type":"session","id":"id","timestamp":"now","cwd":"/workspace"}"#,
            ),
        ];
        for (name, input) in cases {
            let error = decode_pi_header(input.as_bytes()).expect_err(name);
            assert_eq!(
                error.kind(),
                PiErrorKind::UnsupportedFormat,
                "case {name}: {error}"
            );
        }
    });

    test!(decode_pi_v3_rejects_malformed_headers, {
        let cases = [
            ("not JSON", "not-json"),
            (
                "null version",
                r#"{"type":"session","version":null,"id":"id","timestamp":"now","cwd":"/workspace"}"#,
            ),
            (
                "missing id",
                r#"{"type":"session","version":3,"timestamp":"now","cwd":"/workspace"}"#,
            ),
            (
                "wrong timestamp",
                r#"{"type":"session","version":3,"id":"id","timestamp":1,"cwd":"/workspace"}"#,
            ),
            (
                "parent session",
                r#"{"type":"session","version":3,"id":"id","timestamp":"now","cwd":"/workspace","parentSession":false}"#,
            ),
            (
                "trailing JSON",
                r#"{"type":"session","version":3,"id":"id","timestamp":"now","cwd":"/workspace"}{}"#,
            ),
            ("top-level array", "[]"),
        ];
        for (name, input) in cases {
            let error = decode_pi_header(input.as_bytes()).expect_err(name);
            assert_eq!(error.kind(), PiErrorKind::Invalid, "case {name}: {error}");
        }
    });

    test!(decode_pi_v3_rejects_oversized_entry, {
        let data = vec![b'x'; MAX_SESSION_ENTRY_BYTES + 1];
        let error = decode_pi_file(&data).expect_err("oversized entry accepted");
        assert_eq!(error.kind(), PiErrorKind::EntryTooLarge);
    });

    test!(decode_pi_v3_rejects_oversized_file, {
        // The claim under test is that the declared length alone decides,
        // before any record is parsed. The buffer below holds no valid JSON at
        // all.
        let data = vec![0u8; MAX_SESSION_FILE_BYTES + 1];
        let error = decode_pi_file(&data).expect_err("oversized file accepted");
        assert_eq!(error.kind(), PiErrorKind::FileTooLarge);
    });

    test!(
        encode_pi_record_rejects_oversized_raw_record_before_decoding,
        {
            let entry = PiEntry {
                raw: vec![b' '; MAX_SESSION_ENTRY_BYTES + 1],
                ..PiEntry::default()
            };
            let error =
                encode_pi_record(PiRecord::Entry(&entry)).expect_err("oversized raw accepted");
            assert_eq!(error.kind(), PiErrorKind::EntryTooLarge);
        }
    );

    test!(encode_pi_record_returns_error_for_invalid_typed_payload, {
        // A typed payload can fail its self-check in more than one way.
        // `RawValue` cannot hold invalid JSON, so this case uses a user message
        // with no content at all.
        let entry = PiEntry {
            type_name: "message".into(),
            id: "e0000001".into(),
            timestamp: "2026-08-27T12:00:01Z".into(),
            message: Some(Box::new(PiMessage {
                role: "user".into(),
                timestamp: 1,
                ..PiMessage::default()
            })),
            ..PiEntry::default()
        };
        let error =
            encode_pi_record(PiRecord::Entry(&entry)).expect_err("invalid payload accepted");
        assert_eq!(error.kind(), PiErrorKind::Invalid);
    });

    test!(encode_pi_record_uses_exact_field_names, {
        let header = encode_pi_record(PiRecord::Header(&PiHeader {
            type_name: "session".into(),
            version: PI_SESSION_VERSION,
            id: "950e8400-e29b-41d4-a716-446655440000".into(),
            timestamp: "2026-08-27T12:00:00Z".into(),
            cwd: "/workspace".into(),
            ..PiHeader::default()
        }))
        .expect("encode header");
        assert_raw_json_fields(&header, &["type", "version", "id", "timestamp", "cwd"]);

        let entry = encode_pi_record(PiRecord::Entry(&PiEntry {
            type_name: "message".into(),
            id: "e0000001".into(),
            parent_id: None,
            timestamp: "2026-08-27T12:00:01Z".into(),
            message: Some(Box::new(PiMessage {
                role: "user".into(),
                content: Some(
                    serde_json::value::RawValue::from_string(r#""hello""#.into()).expect("raw"),
                ),
                timestamp: 1_787_832_001_000,
                ..PiMessage::default()
            })),
            ..PiEntry::default()
        }))
        .expect("encode entry");
        assert_raw_json_fields(&entry, &["type", "id", "parentId", "timestamp", "message"]);
        let text = String::from_utf8(entry).expect("utf-8");
        assert!(
            !text.contains("parent_id"),
            "entry used non-Pi casing: {text}"
        );
    });

    fn assert_raw_json_fields(raw: &[u8], want: &[&str]) {
        let object = decode_object(raw, "test").expect("object");
        assert_eq!(
            object.len(),
            want.len(),
            "fields = {:?}, want {want:?}",
            object.keys().collect::<Vec<_>>()
        );
        for field in want {
            assert!(object.contains_key(*field), "field {field} missing");
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn pi_v3_fixtures_are_exact_lf_delimited_json() {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/session/pi-v3");
        let mut names: Vec<String> = std::fs::read_dir(&directory)
            .expect("fixture directory")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.ends_with(".jsonl"))
            .collect();
        names.sort();
        assert_eq!(names.len(), 6, "fixture count");
        assert_eq!(
            names,
            FIXTURES
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect::<Vec<_>>(),
            "embedded fixture list is stale"
        );

        for (name, contents) in FIXTURES {
            assert_eq!(
                *contents,
                std::fs::read(directory.join(name))
                    .expect("read fixture")
                    .as_slice(),
                "{name} on disk differs from the embedded copy"
            );
            assert!(contents.ends_with(b"\n"), "{name} does not end with LF");
            assert!(!contents.contains(&b'\r'), "{name} contains CR");
            for (index, line) in contents
                .strip_suffix(b"\n")
                .expect("LF")
                .split(|byte| *byte == b'\n')
                .enumerate()
            {
                serde_json::from_slice::<serde_json::Value>(line)
                    .unwrap_or_else(|_| panic!("{name} line {} is not JSON", index + 1));
            }
        }
    }
}
