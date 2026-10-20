//! Pi entry and message decoding: validation of each entry kind and its fields.

use super::*;

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
        "context_edit" => {
            entry.context_edit = decode_pi_context_edit(&object).ok();
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

fn decode_pi_context_edit(object: &Object) -> Result<PiContextEdit, PiError> {
    let target_id = required_string(object, "targetId", "context_edit.targetId")?;
    let Some(replacement) = object.get("replacement") else {
        return Err(invalid_field(
            "context_edit.replacement",
            "null or an object with content",
        ));
    };
    if is_json_null(replacement) {
        return Ok(PiContextEdit {
            target_id,
            ..PiContextEdit::default()
        });
    }
    let replacement_object =
        decode_object(replacement.get().as_bytes(), "context_edit.replacement")?;
    let Some(content) = replacement_object.get("content") else {
        return Err(invalid_field(
            "context_edit.replacement.content",
            "a string or content array",
        ));
    };
    let (replacement_text, replacement_blocks) =
        decode_content(content, "context_edit.replacement.content", true)?;
    Ok(PiContextEdit {
        target_id,
        replacement: Some(replacement.clone()),
        replacement_content: Some(content.clone()),
        replacement_text,
        replacement_blocks,
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
