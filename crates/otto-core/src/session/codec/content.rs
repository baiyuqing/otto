//! Pi content block decoding and the validation of deferred handles and diagnostics.

use super::*;

// -- content ---------------------------------------------------------------

type DecodedContent = (Option<String>, Vec<PiContentBlock>);

pub(super) fn decode_content_field(
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

pub(super) fn validate_deferred_handle(raw: &RawValue, path: &str) -> Result<(), PiError> {
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

pub(super) fn validate_diagnostics(raw: &RawValue, path: &str) -> Result<(), PiError> {
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

pub(super) fn validate_optional_bool(
    object: &Object,
    field: &str,
    path: &str,
) -> Result<(), PiError> {
    match object.get(field) {
        Some(raw) => decode_bool(raw, path).map(|_| ()),
        None => Ok(()),
    }
}
