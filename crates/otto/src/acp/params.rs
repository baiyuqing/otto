//! Request parameter parsing and the initialize result.

use super::*;

/// The prompt text of a `session/prompt` request: `text` blocks and
/// `resource_link` lines joined with `\n`.
pub(super) fn parse_prompt(params: &Value) -> Result<(String, String, Option<Channel>), Error> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_params("sessionId is required"))?
        .to_string();
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_params("prompt must be an array of content blocks"))?;
    let mut lines = Vec::new();
    for block in blocks {
        match serde_json::from_value::<ContentBlock>(block.clone()) {
            Ok(ContentBlock::Text(text)) => lines.push(text.text),
            Ok(ContentBlock::ResourceLink(link)) => {
                lines.push(format!("{}: {}", link.name, link.uri))
            }
            Ok(_) => return Err(invalid_params("unsupported content block type")),
            Err(failure) => return Err(invalid_params(failure.to_string())),
        }
    }
    let text = lines.join("\n");
    if text.trim().is_empty() {
        return Err(invalid_params("prompt is empty"));
    }
    let channel_value = params
        .get("_meta")
        .and_then(|meta| meta.get("otto"))
        .and_then(|otto| otto.get("channel"));
    let channel = match channel_value {
        None => None,
        Some(Value::String(value)) => Some(
            serde_json::from_value::<Channel>(Value::String(value.clone()))
                .map_err(|_| invalid_params("_meta.otto.channel must be telegram or feishu"))?,
        ),
        Some(_) => return Err(invalid_params("_meta.otto.channel must be a string")),
    };
    Ok((session_id, text, channel))
}

/// The chat origin of a `session/prompt`, from `_meta.otto.chat`
/// (`<platform>:<chat id>`). A missing or invalid value, or a platform that
/// differs from the channel's, gives no origin; stdout is not touched.
pub(super) fn parse_origin(params: &Value, channel: Option<Channel>) -> Option<Origin> {
    let value = params
        .get("_meta")
        .and_then(|meta| meta.get("otto"))
        .and_then(|otto| otto.get("chat"))?;
    let origin = value
        .as_str()
        .ok_or_else(|| "_meta.otto.chat must be a string".to_string())
        .and_then(|chat| {
            let (platform, id) = chat
                .split_once(':')
                .ok_or_else(|| "_meta.otto.chat must be <platform>:<chat id>".to_string())?;
            Origin::chat(platform, id)
        });
    let origin = origin.and_then(|origin| match (channel, origin.chat_platform()) {
        (Some(channel), Some(platform)) if channel_name(channel) != platform => {
            Err("_meta.otto.chat platform differs from _meta.otto.channel".to_string())
        }
        _ => Ok(origin),
    });
    match origin {
        Ok(origin) => Some(origin),
        Err(message) => {
            eprintln!("acp: ignoring origin: {message}");
            None
        }
    }
}

/// The correlation ID of a `session/prompt`, from `_meta.otto.requestId`,
/// reduced by the same rule as the `X-Request-Id` header. A missing,
/// non-string, or empty-after-sanitizing value is absent; it never rejects
/// the prompt.
pub(super) fn parse_request_id(params: &Value) -> Option<String> {
    let raw = params
        .get("_meta")?
        .get("otto")?
        .get("requestId")?
        .as_str()?;
    Some(crate::log::request_id(raw.as_bytes())).filter(|id| !id.is_empty())
}

/// The idempotency key of a `session/prompt`, from `_meta.otto.inputId`: a
/// retry with the same id in the same session runs once. Unlike
/// `requestId`, a malformed value rejects the prompt.
pub(super) fn parse_input_id(
    params: &Value,
) -> Result<Option<String>, agent_client_protocol_schema::v1::Error> {
    let Some(raw) = params
        .get("_meta")
        .and_then(|meta| meta.get("otto"))
        .and_then(|otto| otto.get("inputId"))
    else {
        return Ok(None);
    };
    match raw.as_str() {
        Some(id) if otto_core::model::is_input_id(id) => Ok(Some(id.to_string())),
        _ => Err(invalid_params(crate::server::INVALID_INPUT_ID)),
    }
}

pub(super) fn channel_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Telegram => "telegram",
        Channel::Feishu => "feishu",
    }
}

/// Advertise memory review only when the controller lives in this process.
pub(super) fn initialize_result(local: bool) -> Reply {
    let capabilities = AgentCapabilities::new()
        .load_session(true)
        .prompt_capabilities(PromptCapabilities::new())
        .mcp_capabilities(McpCapabilities::new())
        .session_capabilities(SessionCapabilities::new().list(SessionListCapabilities::new()));
    let capabilities = match local {
        true => capabilities.meta(serde_json::Map::from_iter([(
            "otto".to_string(),
            json!({"memoryReview": true,
                "approvalDialogue":true,
                "channelContext": true,
                "taskControl": true,
                "scheduleDelivery": true,
                "scheduleDeliveryChats": true,
                "approvalMode":{"version":1},
                "approvalPolicy":{"version":1},
                "approvals":{"version":1}}),
        )])),
        false => capabilities.meta(serde_json::Map::from_iter([(
            "otto".to_string(),
            json!({"approvalDialogue":true,
                "channelContext": true,
                "taskControl": true,
                "scheduleDelivery": true,
                "scheduleDeliveryChats": true,
                "approvalMode":{"version":1},
                "approvalPolicy":{"version":1},
                "approvals":{"version":1}}),
        )])),
    };
    result(
        InitializeResponse::new(ProtocolVersion::V1)
            .agent_capabilities(capabilities)
            .auth_methods(vec![])
            .agent_info(Implementation::new("otto", crate::VERSION)),
    )
}
